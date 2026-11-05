"""PLS — Proxy LocalHost:Server.

A loopback router that exposes one model id and two lanes:

    /v1/...            cloud parent
    /local/v1/...      local subagent; its answer is summarised or withheld

Under a policy (`privacy` or `cost`) a local decision model, served by wally
over the entitlement socket, reads each new user prompt and decides what the
cloud parent is told to do: delegate to a local subagent, and with which
summary type, or do it itself. Mode and policy come from a JSON file re-read
on every request. The local model server is spawned lazily and killed
on exit, including on SIGTERM and when the parent disappears.

Binds port 0 and prints `{"port": N}` on stdout once listening.
"""

import asyncio
import atexit
import collections
import hashlib
import json
import os
import re
import secrets
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

import httpx
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, Response, StreamingResponse
import uvicorn

MODEL_ID = "Hybrid-Wally-0.1"
DEFAULT_MODE = "hybrid"
MODES = ("hybrid", "cloud")
DEFAULT_POLICY = "privacy"
POLICIES = ("privacy", "cost")
DEFAULT_MAX_TURNS = 20
QUEUE_MAX = 8
SUMMARY_MAX_TOKENS = 512

WITHHELD = (
    "[content withheld: this subagent ran under local-only disclosure — its "
    "result was kept on your device]"
)
WRAP_UP = (
    "You have reached the turn limit. Wrap up now: synthesize the best result "
    "available and answer in text only."
)
# The cloud parent's job under each policy, appended to its system prompt once.
POLICY_PROMPTS = {
    "privacy": (
        "\n\nYou are the orchestrator in a privacy-first pair. Your only jobs are "
        "to talk with the user, launch the `subagent` tool, and relay what it "
        "returns. The subagent is the worker: it plans builds and explores files, "
        "folders and the system on the user's device. You never read, write or "
        "run anything yourself, and you have no other tool. When an instruction "
        "carries a `pls-task:` line, copy that line exactly as the first line of "
        "the subagent prompt."
    ),
    "cost": (
        "\n\nYou are the planner in a cost-saving pair. Talk with the user and "
        "plan with them. Hand coding, execution and debugging to the `subagent` "
        "tool, which runs on the user's device. Do a task yourself only when the "
        "router says it is small. When an instruction carries a `pls-task:` line, "
        "copy that line exactly as the first line of the subagent prompt."
    ),
}

# Where a delegated task names itself on the subagent prompt. The router, not
# the cloud model, owns what the id means, so the cloud cannot loosen it.
TASK_MARKER = "pls-task:"
TASK_RE = re.compile(r"^\s*pls-task:\s*(t\d+)\b[ \t]*\n?")
ROUTER_TAG = "[router]"

# The guard is router-owned: the router says what to call and checks the
# answer. A rejected reply is dropped and asked again; after GUARD_RETRIES the
# router emits the subagent call itself.
GUARD_RETRIES = 2
SUBAGENT_TOOL = "subagent"
# Tools a local subagent loses under privacy: each sends text to the parent
# without passing through the summary-type filter.
CHILD_DENIED = {"send_message"}

# Told to the local subagent, so it knows who sees its answer. The router
# enforces the same rule on the answer afterwards.
CHILD_RULES = {
    "summary": (
        " When you finish, end with a short summary of what you found or did. "
        "Only that summary goes back to the main agent, so keep file paths, "
        "names and numbers in it, and do not paste file contents or raw values."
    ),
    "local_only": (
        " Nothing you return leaves this device: the main agent only learns that "
        "the task ran. Finish the task completely here."
    ),
}

SUMMARY_PROMPT = (
    "Summarise the following result in at most 120 words. Keep file paths, "
    "names and numbers. Answer with the summary only.\n\n"
)

# Returned instead of the raw result when the summariser fails, so a failed
# summary does not silently become a full-content leak.
SUMMARISE_FAILED = (
    "[summary unavailable: the local summariser failed and the full result was "
    "not returned]"
)

ENTITLEMENT_SOCKET = os.environ.get("PLS_ENTITLEMENT_SOCKET", "")
# Per-launch secret. The port is loopback but shared, so without it any local
# process could spend the account's cloud quota through this bearer token.
TOKEN = os.environ.get("PLS_TOKEN", "")
# Debug console: remove once the MVP ships.
DEBUG = os.environ.get("PLS_DEBUG") == "1"
EVENTS: "collections.deque[dict]" = collections.deque(maxlen=1000)
SUBSCRIBERS: "set[asyncio.Queue]" = set()
premium_cache = {"value": False, "at": 0.0}

PROFILE_DIR = Path(
    os.environ.get("WALLY_PROFILE_DIR")
    or os.environ.get("RCLI_PROFILE_DIR")
    or Path.home() / ".config" / "wally"
)
CONFIG_DIR = PROFILE_DIR / "pls"
CONFIG_PATH = CONFIG_DIR / "config.json"
TASKS_PATH = CONFIG_DIR / "tasks.json"
CREDENTIALS_PATH = PROFILE_DIR / "credentials.json"
PID_PATH = CONFIG_DIR / "pls.pid"
SERVER_SCRIPT = Path(__file__).with_name("model_server.py")

app = FastAPI()
http = httpx.AsyncClient(timeout=httpx.Timeout(600.0, connect=10.0))

local_sem = asyncio.Semaphore(1)
waiting = 0
turns: dict[str, int] = {}
local_sessions: "set[str]" = set()
model_proc: subprocess.Popen | None = None
model_port: int | None = None
model_lock = asyncio.Lock()
explicit_parent = int(os.environ.get("PLS_PARENT_PID") or 0)
parent_pid = explicit_parent or os.getppid()
shutting_down = False

STATS: dict[str, Any] = {
    "requests": 0,
    "errors": 0,
    "queued": 0,
    "overflow": 0,
    "local_turns": 0,
    "cloud_tokens": 0,
    "local_tokens": 0,
    "cap_hits": 0,
    "disclosure": {"summary": 0, "local_only": 0},
    "pii_files": 0,
    "pii_hits": 0,
    "premium_blocked": 0,
    "decisions": 0,
    "decision_failures": 0,
    "guard_retries": 0,
    "guard_synth": 0,
    "mode": DEFAULT_MODE,
    "policy": DEFAULT_POLICY,
    "last": None,
}


PII_PATTERNS = {
    "email": re.compile(r"\b[\w.+-]+@[\w-]+\.[\w.]{2,}\b"),
    "phone": re.compile(r"\+?\d[\d ().-]{7,}\d"),
    "key": re.compile(r"\b(?:sk-[A-Za-z0-9_-]{16,}|AKIA[0-9A-Z]{16}|gh[pousr]_[A-Za-z0-9]{20,})\b"),
    "ipv4": re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"),
    "iban": re.compile(r"\b[A-Z]{2}\d{2}[A-Z0-9]{12,30}\b"),
}
PATH_RE = re.compile(r"(?<![\w.-])((?:~|\.{1,2})?/[\w./@-]+)")


def short_paths(text: str) -> str:
    home = str(Path.home())
    return text.replace(home, "~") if home else text


def redact(text: str) -> str:
    """Replace PII values with typed placeholders. The shape survives, the value does not."""
    for name, pattern in PII_PATTERNS.items():
        text = pattern.sub(f"[{name}]", text)
    return text


def redact_messages(messages: list) -> list:
    """Redact every non-system message before it goes to the cloud.

    Counts alone still leave the values in the transcript, and a pasted file
    body carries them. Placeholders do not match the patterns, so a turn that
    passes through this twice keeps the same text.
    """
    out = []
    for message in messages:
        content = message.get("content")
        if message.get("role") == "system":
            out.append(message)
        elif isinstance(content, str):
            out.append({**message, "content": redact(content)})
        elif isinstance(content, list):
            parts = [
                {**part, "text": redact(part["text"])}
                if isinstance(part, dict) and isinstance(part.get("text"), str)
                else part
                for part in content
            ]
            out.append({**message, "content": parts})
        else:
            out.append(message)
    return out


# Harness context sent as `user` messages after the person's own prompt. dsh
# and Claude Code wrap theirs in <system-reminder>; dsh also adds a runtime
# snapshot. These are not what the person asked, so routing skips them.
INJECTED_PREFIXES = ("<system-reminder>", "Current runtime context.")


def message_text(message: dict) -> str:
    content = message.get("content")
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return str(content or "")


def is_injected(message: dict) -> bool:
    return message.get("role") == "user" and message_text(message).lstrip().startswith(INJECTED_PREFIXES)


# dsh hands a subagent's output to the parent as a `user` message. These are
# not the person speaking, so they are never decided or instructed.
AGENT_MESSAGE_RE = re.compile(
    r"^\s*(Agent [0-9a-f-]{36} sent a message:|Background subagent [0-9a-f-]{36} )"
)
AGENT_MESSAGE_MASK = (
    "[router] A subagent sent a message here. Under the privacy policy it stays "
    "on this device."
)


def is_agent_message(message: dict) -> bool:
    return message.get("role") == "user" and bool(AGENT_MESSAGE_RE.match(message_text(message)))


def is_router_turn(message: dict) -> bool:
    """A user turn the router added itself (a guard re-ask)."""
    return message.get("role") == "user" and message_text(message).lstrip().startswith(ROUTER_TAG)


def is_prompt(message: dict) -> bool:
    """A user message the person wrote: not harness context, not the router,
    not a subagent reporting back."""
    return (
        message.get("role") == "user"
        and not is_injected(message)
        and not is_router_turn(message)
        and not is_agent_message(message)
    )


def last_turn(body: dict) -> dict | None:
    """The newest message that is not system text or harness-injected context."""
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "system" and not is_injected(message):
            return message
    return None


# --------------------------------------------------------------------------
# task records: one per user prompt, decided once, re-applied on every request
# --------------------------------------------------------------------------

TASKS: dict[str, dict] = {}
TASK_TEXT: dict[str, str] = {}
task_counter = {"next": 1}


def prompt_key(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()[:16]


def persisted(record: dict) -> bool:
    """Only privacy records outlive the router: they keep a resumed session
    masked. A cost record is re-decided each launch, so a decision made with
    an older question or model is never reused."""
    return record.get("policy") == "privacy"


def load_tasks() -> None:
    """Restore privacy records (never text) so a resumed session stays masked."""
    try:
        data = json.loads(TASKS_PATH.read_text())
    except (OSError, ValueError):
        return
    if not isinstance(data, dict):
        return
    for key, record in data.items():
        if isinstance(record, dict) and isinstance(record.get("id"), str) and persisted(record):
            TASKS[key] = record
            number = record["id"][1:]
            if number.isdigit():
                task_counter["next"] = max(task_counter["next"], int(number) + 1)


def save_tasks() -> None:
    try:
        TASKS_PATH.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(TASKS_PATH, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as handle:
            json.dump({key: r for key, r in TASKS.items() if persisted(r)}, handle)
        os.chmod(TASKS_PATH, 0o600)
    except OSError as error:
        log({"event": "tasks_save_failed", "error": str(error)})


def new_record(policy: str, verdict: dict | None) -> dict:
    """The record a decision produces. A failed decision fails safe."""
    decided = verdict is not None
    yes = bool(verdict and verdict.get("answer") == "yes")
    if policy == "privacy":
        private = yes if decided else True
        summary = "local_only" if private else "summary"
        big = True
    else:
        private = False
        big = yes if decided else False
        summary = "summary" if big else None
    record = {"id": f"t{task_counter['next']}", "policy": policy, "private": private,
              "big": big, "summary": summary, "decided": decided}
    task_counter["next"] += 1
    return record


async def decide(policy: str, text: str) -> dict | None:
    """Ask wally's decision model about one prompt. None on any failure."""
    if not ENTITLEMENT_SOCKET:
        return None
    payload = json.dumps({"policy": policy, "text": text}).encode()
    try:
        reader, writer = await asyncio.open_unix_connection(ENTITLEMENT_SOCKET)
        writer.write(
            b"POST /decide HTTP/1.1\r\nHost: local\r\ncontent-type: application/json\r\n"
            + f"content-length: {len(payload)}\r\n\r\n".encode() + payload
        )
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(), 5)
        writer.close()
    except (OSError, TimeoutError) as error:
        log({"event": "decide_failed", "error": str(error) or type(error).__name__})
        return None
    head, _, body = raw.partition(b"\r\n\r\n")
    if not head.startswith(b"HTTP/1.1 200"):
        log({"event": "decide_failed", "status": head.split(b"\r\n", 1)[0].decode(errors="replace")})
        return None
    try:
        verdict = json.loads(body)
    except ValueError:
        log({"event": "decide_failed", "error": "bad body"})
        return None
    if verdict.get("answer") not in ("yes", "no"):
        return None
    return verdict


async def ensure_tasks(body: dict, policy: str) -> list[dict]:
    """Decide every prompt in the request that has no settled record yet.

    Returns the records created or re-decided by this call.
    """
    fresh = []
    for message in body.get("messages") or []:
        if not is_prompt(message):
            continue
        text = message_text(message)
        key = prompt_key(text)
        existing = TASKS.get(key)
        if existing and existing.get("policy") == policy and existing.get("decided"):
            TASK_TEXT.setdefault(existing["id"], text)
            continue
        verdict = await decide(policy, text)
        STATS["decisions"] += 1
        if verdict is None:
            STATS["decision_failures"] += 1
        record = new_record(policy, verdict)
        if existing and existing.get("policy") == policy:
            # A retry keeps the id the cloud may already have been told.
            record["id"] = existing["id"]
        TASKS[key] = record
        TASK_TEXT[record["id"]] = text
        fresh.append(record)
        log({"event": "decided", "task": record["id"], "policy": policy,
             "private": record["private"], "big": record["big"],
             "summary": record["summary"], "decided": record["decided"],
             "p_yes": (verdict or {}).get("p_yes")})
    if fresh:
        save_tasks()
    return fresh


def placeholder(record: dict) -> str:
    return (
        "[The user's request is private and stays on this device.] Launch the "
        f"`{SUBAGENT_TOOL}` tool now. Its prompt must be exactly:\n"
        f"{TASK_MARKER} {record['id']}"
    )


def instruction(record: dict) -> str:
    if record.get("summary") is None:
        return f"{ROUTER_TAG} This task is small: do it yourself, no subagent."
    return (
        f"{ROUTER_TAG} Launch the `{SUBAGENT_TOOL}` tool for this task. Begin its "
        f"prompt with the line `{TASK_MARKER} {record['id']}`, then state the task."
    )


def rewrite_prompt(message: dict, record: dict) -> dict:
    if record["policy"] == "privacy" and record["private"]:
        return {**message, "content": placeholder(record)}
    extra = "\n\n" + instruction(record)
    content = message.get("content")
    if isinstance(content, list):
        return {**message, "content": [*content, {"type": "text", "text": extra}]}
    return {**message, "content": str(content or "") + extra}


def apply_policy(body: dict, policy: str) -> dict:
    """The cloud-bound copy of `body` under `policy`. Never mutates `body`.

    Every prompt with a record is rewritten the same way on every request, so
    a private prompt stays masked on later turns and the cloud prefix is stable.
    """
    messages = []
    for message in body.get("messages") or []:
        if policy == "privacy" and is_agent_message(message):
            # A subagent's `send_message` skips the summary-type filter on its
            # final answer, so under privacy what it says never reaches the cloud.
            messages.append({**message, "content": AGENT_MESSAGE_MASK})
            continue
        record = TASKS.get(prompt_key(message_text(message))) if is_prompt(message) else None
        messages.append(rewrite_prompt(message, record) if record else message)
    shaped = {**body, "messages": messages}

    prompt = POLICY_PROMPTS[policy]
    for index, message in enumerate(messages):
        if message.get("role") != "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            if not any(isinstance(p, dict) and p.get("text") == prompt for p in content):
                messages[index] = {**message, "content": [*content, {"type": "text", "text": prompt}]}
        elif prompt not in str(content or ""):
            messages[index] = {**message, "content": str(content or "") + prompt}
        break

    if policy == "privacy" and isinstance(body.get("tools"), list):
        shaped["tools"] = [t for t in body["tools"]
                           if (t.get("function") or {}).get("name") == SUBAGENT_TOOL]
    return shaped


def pending_task(body: dict) -> dict | None:
    """The record of a prompt no assistant turn has answered yet, if any."""
    last = last_turn(body)
    if not last or not is_prompt(last):
        return None
    return TASKS.get(prompt_key(message_text(last)))


# --------------------------------------------------------------------------
# the local subagent side
# --------------------------------------------------------------------------

def resolve_child(messages: list, policy: str) -> tuple[str, list]:
    """Read the `pls-task:` id off the child's prompt and apply its record.

    Returns the summary type and the messages with the marker stripped and,
    for a private task, the placeholder swapped for the real prompt. A child
    without a known id fails safe: `local_only` under privacy.
    """
    fallback = "local_only" if policy == "privacy" else "summary"
    for index, message in enumerate(messages):
        if not is_prompt(message):
            continue
        content = message.get("content")
        if not isinstance(content, str):
            return fallback, messages
        found = TASK_RE.match(content)
        if not found:
            return fallback, messages
        task_id = found.group(1)
        record = next((r for r in TASKS.values() if r.get("id") == task_id), None)
        if record is None or record.get("summary") is None:
            log({"event": "child_unknown_task", "task": task_id})
            return fallback, messages
        rest = content[found.end():].strip()
        if record["private"] and task_id in TASK_TEXT:
            rest = TASK_TEXT[task_id] + (f"\n\n{rest}" if rest else "")
        updated = list(messages)
        updated[index] = {**message, "content": rest}
        return record["summary"], updated
    return fallback, messages


def inject_child_rules(body: dict, summary: str) -> None:
    rule = CHILD_RULES.get(summary)
    if not rule:
        return
    for message in body.get("messages") or []:
        if message.get("role") != "system":
            continue
        if isinstance(message.get("content"), str) and rule not in message["content"]:
            message["content"] += rule
        return


def parse_calls(buffered: bytes) -> dict[int, dict]:
    """Tool calls in a held SSE body, by index, with their arguments joined.

    Reads deltas, because a name and its arguments can arrive in separate
    chunks and the finish reason only says that *some* call happened.
    """
    calls: dict[int, dict] = {}
    for line in buffered.split(b"data: ")[1:]:
        raw = line.split(b"\n", 1)[0]
        if raw in (b"", b"[DONE]"):
            continue
        try:
            event = json.loads(raw)
        except Exception:
            continue
        for choice in event.get("choices") or []:
            for call in (choice.get("delta") or {}).get("tool_calls") or []:
                slot = calls.setdefault(
                    int(call.get("index") or 0), {"name": "", "arguments": ""}
                )
                function = call.get("function") or {}
                if function.get("name"):
                    slot["name"] = function["name"]
                if function.get("arguments"):
                    slot["arguments"] += function["arguments"]
    return calls


def subagent_prompt(record: dict) -> str:
    """The prompt for a subagent call the router has to write itself."""
    marker = f"{TASK_MARKER} {record['id']}"
    if record.get("private"):
        return marker
    text = TASK_TEXT.get(record["id"], "")
    return f"{marker}\n\n{text}" if text else marker


def patch_call_args(calls: dict[int, dict], record: dict | None) -> bool:
    """Make every subagent call carry a prompt that names its task.

    A pinned `tool_choice` makes the gateway strip arguments, so a call can
    arrive as `subagent {}`; dsh rejects that. A call without the task line
    gets it, so the subagent side knows its summary type.
    """
    changed = False
    for call in calls.values():
        if call.get("name") != SUBAGENT_TOOL:
            continue
        try:
            arguments = json.loads(call.get("arguments") or "{}")
        except ValueError:
            arguments = {}
        if not isinstance(arguments, dict):
            arguments = {}
        prompt = str(arguments.get("prompt") or "")
        if record is not None:
            if not prompt:
                arguments["prompt"] = subagent_prompt(record)
                changed = True
            elif not TASK_RE.match(prompt):
                arguments["prompt"] = f"{TASK_MARKER} {record['id']}\n\n{prompt}"
                changed = True
        if not arguments.get("description"):
            arguments["description"] = "Run the task on this device"
            changed = True
        call["arguments"] = json.dumps(arguments)
    return changed


def call_events(calls: dict[int, dict]) -> list[str]:
    """SSE events that carry `calls` and close the message, without a role chunk."""
    created = int(time.time())
    cid = "chatcmpl-pls-patch"
    events = [sse(chunk(cid, created, {"tool_calls": [{
        "index": index,
        "id": f"call_pls_{index}",
        "type": "function",
        "function": {"name": call["name"], "arguments": call["arguments"]},
    }]})) for index, call in calls.items()]
    events.append(sse(chunk(cid, created, {}, "tool_calls")))
    events.append("data: [DONE]\n\n")
    return events


def text_events(text: str) -> list[str]:
    """SSE events that carry a text reply and close the message."""
    created = int(time.time())
    cid = "chatcmpl-pls-note"
    return [sse(chunk(cid, created, {"content": text})),
            sse(chunk(cid, created, {}, "stop")),
            "data: [DONE]\n\n"]


def calls_stream(calls: dict[int, dict]) -> Response:
    """Re-emit held calls as one SSE body, after a rewrite."""
    role = sse(chunk("chatcmpl-pls-patch", int(time.time()), {"role": "assistant"}))

    async def stream():
        yield role
        for event in call_events(calls):
            yield event

    return StreamingResponse(stream(), media_type="text/event-stream")


def carries_text(event: bytes) -> bool:
    """Whether one SSE event adds visible text (or reasoning) and no tool call."""
    raw = event.strip()
    if not raw.startswith(b"data: ") or raw == b"data: [DONE]":
        return False
    try:
        payload = json.loads(raw[len(b"data: "):])
    except ValueError:
        return False
    for choice in payload.get("choices") or []:
        delta = choice.get("delta") or {}
        if delta.get("tool_calls"):
            return False
        if delta.get("content") or delta.get("reasoning_content"):
            return True
    return False


async def body_parts(response: Response):
    """The bytes of a response this router built, streamed or not."""
    if isinstance(response, StreamingResponse):
        async for part in response.body_iterator:
            yield part.encode() if isinstance(part, str) else part
    else:
        yield response.body


def synthetic_call(record: dict) -> Response:
    """Emit the subagent call ourselves when the model will not.

    dsh executes whatever tool call the stream carries, so this is the only
    hard guarantee that a private task never runs on the cloud.
    """
    calls = {0: {"name": SUBAGENT_TOOL, "arguments": ""}}
    patch_call_args(calls, record)
    return calls_stream(calls)


def scan_pii(messages: list) -> dict:
    """Count PII per file mentioned in the request.

    The file is whatever path the message carries; a message with no path is
    counted under `<inline>`. Only counts travel, never the values.
    """
    files: dict[str, dict[str, int]] = {}

    def add(path: str, segment: str) -> None:
        for name, pattern in PII_PATTERNS.items():
            found = len(pattern.findall(segment))
            if found:
                counts = files.setdefault(path, {})
                counts[name] = counts.get(name, 0) + found

    for message in messages:
        content = message.get("content")
        if isinstance(content, list):
            text = " ".join(
                part.get("text", "") for part in content if isinstance(part, dict)
            )
        else:
            text = content or ""
        if not text:
            continue
        # A message names the files it covers; each path owns the text up to
        # the next one, so a key in one file is not blamed on its neighbour.
        spans = list(PATH_RE.finditer(text))
        if not spans:
            add("<inline>", text)
            continue
        if spans[0].start() > 0:
            add("<inline>", text[: spans[0].start()])
        for index, span in enumerate(spans):
            end = spans[index + 1].start() if index + 1 < len(spans) else len(text)
            add(span.group(1), text[span.end():end])

    return {"files": files, "total": sum(sum(c.values()) for c in files.values())}


async def premium() -> bool:
    """Ask wally whether this account may run work locally.

    No socket means a hand-run router, which is not gated.
    """
    if not ENTITLEMENT_SOCKET:
        return True
    now = time.monotonic()
    if now - premium_cache["at"] < 5:
        return premium_cache["value"]
    value = premium_cache["value"]
    try:
        reader, writer = await asyncio.open_unix_connection(ENTITLEMENT_SOCKET)
        writer.write(b"GET /entitlement HTTP/1.1\r\nHost: local\r\n\r\n")
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(512), 2)
        writer.close()
        body = raw.split(b"\r\n\r\n", 1)[-1]
        value = bool(json.loads(body).get("sub"))
    except Exception:
        pass
    premium_cache.update(value=value, at=now)
    return value


def log(event: dict) -> None:
    print(json.dumps(event), file=sys.stderr, flush=True)
    if DEBUG:
        event = {"ts": round(time.time(), 3), **event}
        EVENTS.append(event)
        for queue in list(SUBSCRIBERS):
            try:
                queue.put_nowait(event)
            except Exception:
                pass


# --------------------------------------------------------------------------
# config and credentials
# --------------------------------------------------------------------------


def read_config() -> dict:
    try:
        data = json.loads(CONFIG_PATH.read_text())
    except Exception:
        data = {}
    if not isinstance(data, dict):
        data = {}
    mode = data.get("mode")
    if mode not in MODES:
        mode = DEFAULT_MODE
    policy = data.get("policy")
    if policy not in POLICIES:
        policy = DEFAULT_POLICY
    try:
        max_turns = int(data.get("max_turns", DEFAULT_MAX_TURNS))
    except Exception:
        max_turns = DEFAULT_MAX_TURNS
    return {
        "mode": mode,
        "policy": policy,
        "max_turns": max(1, max_turns),
        "model_path": os.environ.get("PLS_MODEL") or data.get("model_path") or "",
        "cloud_model": data.get("cloud_model") or os.environ.get("PLS_CLOUD_MODEL", ""),
    }


def read_credentials() -> dict:
    try:
        return json.loads(CREDENTIALS_PATH.read_text())
    except Exception:
        return {}


def write_credentials(creds: dict) -> None:
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    tmp = CREDENTIALS_PATH.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(creds))
    os.chmod(tmp, 0o600)
    tmp.replace(CREDENTIALS_PATH)


async def refresh_credentials(creds: dict) -> dict:
    refresh_token = creds.get("refresh_token", "")
    console = creds.get("console_url", "")
    if not refresh_token or not console:
        return creds
    try:
        response = await http.post(
            f"{console.rstrip('/')}/auth/cli/refresh",
            json={"refresh_token": refresh_token},
        )
        if response.status_code != 200:
            log({"event": "refresh_failed", "status": response.status_code})
            return creds
        grant = response.json()
    except Exception:
        return creds
    if not grant.get("access_token"):
        return creds
    creds = dict(creds)
    creds["access_token"] = grant["access_token"]
    if grant.get("refresh_token"):
        creds["refresh_token"] = grant["refresh_token"]
    if grant.get("expires_in"):
        creds["expires_at"] = int(time.time()) + int(grant["expires_in"])
    try:
        write_credentials(creds)
    except Exception:
        pass
    log({"event": "refreshed"})
    return creds


async def cloud_credentials() -> dict:
    creds = read_credentials()
    expires_at = int(creds.get("expires_at") or 0)
    if expires_at and expires_at <= time.time() + 60:
        creds = await refresh_credentials(creds)
    return creds


# --------------------------------------------------------------------------
# model server
# --------------------------------------------------------------------------


def kill_model_server() -> None:
    global model_proc
    proc = model_proc
    model_proc = None
    if proc is None or proc.poll() is not None:
        return
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except Exception:
        try:
            proc.kill()
        except Exception:
            pass


async def ensure_model_server() -> int:
    global model_proc, model_port
    async with model_lock:
        if model_proc is not None and model_proc.poll() is None and model_port:
            return model_port
        config = read_config()
        model_path = config["model_path"]
        if not model_path:
            raise RuntimeError("no model path: set model_path in config.json or PLS_MODEL")
        env = dict(os.environ)
        env["PLS_MODEL"] = model_path
        env["PLS_MODEL_ID"] = MODEL_ID
        proc = subprocess.Popen(
            [sys.executable, str(SERVER_SCRIPT)],
            stdout=subprocess.PIPE,
            stderr=None,
            env=env,
            text=True,
        )
        line = await asyncio.get_running_loop().run_in_executor(None, proc.stdout.readline)
        if not line:
            proc.kill()
            raise RuntimeError("model server exited before the handshake")
        port = int(json.loads(line)["port"])
        model_proc = proc
        model_port = port
        log({"event": "model_server", "port": port})
        return port


# --------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------


def session_key(request: Request, body: dict) -> str:
    for name in ("x-session-affinity", "x-session-id", "x-opencode-session-id"):
        value = request.headers.get(name)
        if value:
            return value
    messages = body.get("messages") or []
    first = json.dumps(messages[:1], sort_keys=True) if messages else ""
    return hashlib.sha1(first.encode()).hexdigest()[:16]


def sse(payload: dict) -> str:
    return f"data: {json.dumps(payload)}\n\n"


def chunk(cid: str, created: int, delta: dict, finish: str | None = None) -> dict:
    return {
        "id": cid,
        "object": "chat.completion.chunk",
        "created": created,
        "model": MODEL_ID,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }


def as_stream(result: dict, want_usage: bool = False) -> StreamingResponse:
    cid = result.get("id", "chatcmpl-pls")
    created = result.get("created", int(time.time()))
    choice = (result.get("choices") or [{}])[0]
    message = choice.get("message") or {}
    finish = choice.get("finish_reason") or "stop"

    async def body():
        yield sse(chunk(cid, created, {"role": "assistant"}))
        if message.get("content"):
            yield sse(chunk(cid, created, {"content": message["content"]}))
        for index, call in enumerate(message.get("tool_calls") or []):
            yield sse(chunk(cid, created, {"tool_calls": [{"index": index, **call}]}))
        yield sse(chunk(cid, created, {}, finish))
        if want_usage and result.get("usage"):
            yield sse(
                {
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "model": MODEL_ID,
                    "choices": [],
                    "usage": result["usage"],
                }
            )
        yield "data: [DONE]\n\n"

    return StreamingResponse(body(), media_type="text/event-stream")


async def summarise(text: str) -> str:
    port = await ensure_model_server()
    try:
        response = await http.post(
            f"http://127.0.0.1:{port}/v1/chat/completions",
            json={
                "model": MODEL_ID,
                "messages": [{"role": "user", "content": SUMMARY_PROMPT + text}],
                "max_tokens": SUMMARY_MAX_TOKENS,
                "temperature": 0.2,
            },
            headers={"x-session-affinity": "pls-summary"},
        )
        result = response.json()
        return ((result.get("choices") or [{}])[0].get("message") or {}).get("content") or ""
    except Exception as error:
        log({"event": "summary_failed", "error": str(error)})
        return SUMMARISE_FAILED


# --------------------------------------------------------------------------
# routes
# --------------------------------------------------------------------------


@app.get("/health")
def health():
    return {"ok": True, "mode": read_config()["mode"]}


@app.get("/stats")
def stats():
    data = dict(STATS)
    data["mode"] = read_config()["mode"]
    data["waiting"] = waiting
    data["model_port"] = model_port
    return data


@app.post("/stats/reset")
def stats_reset():
    for key in ("requests", "errors", "queued", "overflow", "local_turns",
                "cloud_tokens", "local_tokens", "cap_hits", "guard_retries",
                "guard_synth", "decisions", "decision_failures"):
        STATS[key] = 0
    STATS["disclosure"] = {"summary": 0, "local_only": 0}
    STATS["last"] = None
    return {"ok": True}


DEBUG_PAGE = """<!doctype html>
<html><head><meta charset="utf-8"><title>PLS debug</title>
<style>
:root{color-scheme:dark}
body{background:#0e1116;color:#c9d1d9;font:12px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;margin:0}
header{position:sticky;top:0;background:#161b22;border-bottom:1px solid #30363d;padding:8px 12px;display:flex;gap:14px;align-items:center;flex-wrap:wrap}
h1{font-size:13px;margin:0;font-weight:600;color:#e6edf3}
.pill{border:1px solid #30363d;border-radius:10px;padding:1px 8px;color:#8b949e}
.pill b{color:#e6edf3}
table{border-collapse:collapse;width:100%}
th,td{text-align:left;padding:3px 8px;border-bottom:1px solid #21262d;vertical-align:top;white-space:pre-wrap}
th{position:sticky;top:41px;background:#0e1116;color:#8b949e;font-weight:500}
.cloud{color:#79c0ff}.local{color:#56d364}.blocked{color:#f0883e}.file{color:#d2a8ff}
.pii{color:#f778ba}.dim{color:#6e7681}.warn{color:#e3b341}
button.copy{padding:0 6px;font-size:11px;margin-left:8px}
button{background:#21262d;color:#c9d1d9;border:1px solid #30363d;border-radius:6px;padding:3px 10px;cursor:pointer}
</style></head><body>
<header>
<h1>PLS router</h1>
<span class="pill">mode <b id="mode">?</b></span>
<span class="pill">sub <b id="sub">?</b></span>
<span class="pill">local <b id="local">0</b></span>
<span class="pill">cloud <b id="cloud">0</b></span>
<span class="pill">pii files <b id="pii">0</b></span>
<button id="pause">pause</button><button id="clear">clear</button>
<a href="/debug/export"><button>export</button></a>
</header>
<table><thead><tr><th>time</th><th>event</th><th>lane</th><th>path / file</th><th>detail</th></tr></thead><tbody id="rows"></tbody></table>
<script>
const rows=document.getElementById('rows');let paused=false,n={local:0,cloud:0,pii:0};
const esc=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
document.getElementById('pause').onclick=e=>{paused=!paused;e.target.textContent=paused?'resume':'pause'};
document.getElementById('clear').onclick=()=>{rows.innerHTML=''};
function add(e){
 const t=new Date(e.ts*1000).toLocaleTimeString();
 const tr=document.createElement('tr');
 const lane=e.lane||(e.event==='local'||e.event==='local_done'?'local':'');
 if(lane==='local')n.local++; if(lane==='cloud')n.cloud++;
 if(e.event==='pii_file'){n.pii++;}
 document.getElementById('local').textContent=n.local;
 document.getElementById('cloud').textContent=n.cloud;
 document.getElementById('pii').textContent=n.pii;
 if(e.mode)document.getElementById('mode').textContent=e.mode;
 if(e.sub!==undefined)document.getElementById('sub').textContent=e.sub;
 const detail={...e};delete detail.ts;delete detail.event;delete detail.lane;delete detail.path;delete detail.file;
 const cls=e.event==='pii_file'?'file':e.reason==='no-subscription'?'blocked':e.event==='hint'?'warn':lane;
 const text=e.counts?'<span class=pii>'+esc(JSON.stringify(e.counts))+'</span> <span class=dim>'+esc(e.action)+'</span>':esc(e.message||JSON.stringify(detail));
 tr.innerHTML=`<td class=dim>${esc(t)}</td><td>${esc(e.event)}</td><td class=${esc(cls)}>${esc(lane)}</td>`+
   `<td class=${esc(cls)}>${esc(e.path||e.file||'')}</td>`+
   `<td>${text}<button class=copy title="copy json">copy</button></td>`;
 tr.querySelector('.copy').onclick=ev=>{
   navigator.clipboard.writeText(JSON.stringify(e)).then(()=>{
     ev.target.textContent='copied';setTimeout(()=>ev.target.textContent='copy',900);});
 };
 rows.prepend(tr);
}
fetch('/debug/state').then(r=>r.json()).then(s=>{
 document.getElementById('mode').textContent=s.mode;
 document.getElementById('sub').textContent=s.sub;
 for(const e of s.events.reverse())add(e);
 const es=new EventSource('/debug/events');
 es.onmessage=m=>{if(!paused)add(JSON.parse(m.data))};
});
</script></body></html>"""


@app.get("/favicon.ico")
def favicon():
    return Response(b"", media_type="image/x-icon", status_code=204)


@app.get("/debug")
def debug_page():
    if not DEBUG:
        return JSONResponse({"error": {"message": "debug console off (start with PLS_DEBUG=1)"}}, status_code=404)
    return Response(DEBUG_PAGE, media_type="text/html")


@app.get("/debug/state")
def debug_state():
    return {"mode": read_config()["mode"], "sub": premium_cache["value"],
            "events": list(EVENTS)}


@app.get("/debug/export")
def debug_export():
    body = "\n".join(json.dumps(event) for event in EVENTS)
    return Response(
        body + "\n",
        media_type="application/x-ndjson",
        headers={"content-disposition": 'attachment; filename="pls-events.ndjson"'},
    )


@app.get("/debug/events")
async def debug_events():
    queue: asyncio.Queue = asyncio.Queue()
    SUBSCRIBERS.add(queue)

    async def stream():
        try:
            while True:
                yield "data: " + json.dumps(await queue.get()) + "\n\n"
        finally:
            SUBSCRIBERS.discard(queue)

    return StreamingResponse(stream(), media_type="text/event-stream")


@app.api_route("/{path:path}", methods=["GET", "POST"])
async def route(path: str, request: Request):
    if path in ("v1/models", "local/v1/models"):
        return {"object": "list", "data": [{"id": MODEL_ID, "object": "model", "owned_by": "wally"}]}

    if TOKEN and not secrets.compare_digest(request.headers.get("x-pls-token", ""), TOKEN):
        return JSONResponse(
            {"error": {"message": "missing or wrong x-pls-token"}}, status_code=401
        )

    body: dict = {}
    if request.method == "POST":
        try:
            body = await request.json()
        except ValueError:
            body = {}
        if not isinstance(body, dict):
            body = {}

    config = read_config()
    policy = config["policy"]
    STATS["mode"] = config["mode"]
    STATS["policy"] = policy

    lane = "local" if path.startswith("local/") else "cloud"
    upstream_path = path[len("local/"):] if lane == "local" else path

    sub = await premium()
    reason = "path"
    if not sub:
        lane = "cloud"
        reason = "no-subscription"
        STATS["premium_blocked"] += 1
    elif config["mode"] == "cloud":
        lane = "cloud"
        reason = "mode"

    messages = body.get("messages") or []
    log({"event": "route", "path": path, "lane": lane, "mode": config["mode"],
         "policy": policy, "reason": reason, "sub": sub, "messages": len(messages)})

    if lane == "local":
        return await local(upstream_path, body, request, config)

    # Cloud mode, or an account without the local lane: the cloud gets the
    # request exactly as the harness sent it. So does a side call with no
    # tools (dsh's session-title request): it cannot delegate, and an
    # instruction appended to it ends up in the title.
    if not sub or config["mode"] == "cloud" or not body.get("tools"):
        return await cloud(upstream_path, body, request)

    await ensure_tasks(body, policy)
    record = pending_task(body)
    shaped = apply_policy(body, policy)

    pii = scan_pii(shaped.get("messages") or [])
    if pii["total"]:
        STATS["pii_files"] += len(pii["files"])
        STATS["pii_hits"] += pii["total"]
        for file_path, counts in pii["files"].items():
            log({"event": "pii_file", "file": file_path, "counts": counts})
        shaped["messages"] = redact_messages(shaped["messages"])

    # Privacy checks every reply; cost only the reply to a prompt it delegated.
    guard = policy == "privacy" or bool(record and record.get("summary"))
    if record:
        log({"event": "instructed", "task": record["id"], "policy": policy,
             "summary": record["summary"], "private": record["private"]})
    return await cloud(upstream_path, shaped, request, pii, guard=guard,
                       policy=policy, record=record)


def _with_model(payload: bytes) -> bytes:
    """Rewrite the upstream model id to the one model id the router exposes."""
    try:
        parsed = json.loads(payload)
    except Exception:
        return payload
    parsed["model"] = MODEL_ID
    return json.dumps(parsed).encode()


def _unauthorized(payload: bytes) -> bytes:
    """Add the one thing the gateway's 401 does not say: how to fix it."""
    try:
        parsed = json.loads(payload)
    except Exception:
        parsed = {"error": {"message": payload.decode(errors="replace")}}
    message = (parsed.get("error") or {}).get("message") or "unauthorized"
    parsed["error"] = {
        "message": f"{message} — run `wally account login` to sign in again",
    }
    return json.dumps(parsed).encode()


def calls_from_json(payload: dict) -> dict[int, dict]:
    """Tool calls in a buffered completion, shaped like `parse_calls`."""
    calls: dict[int, dict] = {}
    for choice in payload.get("choices") or []:
        for index, call in enumerate((choice.get("message") or {}).get("tool_calls") or []):
            function = call.get("function") or {}
            calls[index] = {"name": function.get("name") or "",
                            "arguments": function.get("arguments") or ""}
    return calls


def synthetic_json(record: dict) -> JSONResponse:
    """`synthetic_call` for a client that asked for a single JSON answer."""
    calls = {0: {"name": SUBAGENT_TOOL, "arguments": ""}}
    patch_call_args(calls, record)
    return calls_json(calls)


def calls_json(calls: dict[int, dict]) -> JSONResponse:
    """Held calls as one JSON completion, after a rewrite."""
    message = {"role": "assistant", "content": None, "tool_calls": [
        {"id": f"call_pls_{index}", "type": "function", "function": call}
        for index, call in calls.items()
    ]}
    return JSONResponse({
        "id": "chatcmpl-pls-patch",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": MODEL_ID,
        "choices": [{"index": 0, "message": message, "finish_reason": "tool_calls"}],
    })


def guard_verdict(names: set[str], policy: str, record: dict | None) -> bool:
    """Whether a cloud reply with these tool calls may reach the harness.

    `record` is the task of a prompt no assistant turn has answered yet.
    """
    if policy == "privacy":
        if not names <= {SUBAGENT_TOOL}:
            return False
        return bool(names) if record else True
    if record and record.get("summary"):
        return SUBAGENT_TOOL in names
    return True


def blocked_reply() -> dict:
    """What the harness gets when the parent keeps reaching for a tool it may not use."""
    return {
        "id": "chatcmpl-pls-blocked",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": MODEL_ID,
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant",
            "content": "[router] That action needs a local subagent under the privacy "
                       "policy, so it was not run. Ask again and it will be delegated.",
        }}],
    }


async def cloud(path: str, body: dict, request: Request,
                pii: dict | None = None, guard: bool = False,
                policy: str = DEFAULT_POLICY, record: dict | None = None,
                attempt: int = 0):
    config_cloud_model = read_config()["cloud_model"]
    creds = await cloud_credentials()
    console = creds.get("console_url", "")
    if not console or not creds.get("access_token"):
        STATS["errors"] += 1
        return JSONResponse(
            {"error": {"message": "not signed in: no cloud credentials in the wally profile"}},
            status_code=503,
        )
    if config_cloud_model:
        body = {**body, "model": config_cloud_model}
    url = f"{console.rstrip('/')}/{path}"
    headers = {
        "authorization": f"Bearer {creds['access_token']}",
        "content-type": "application/json",
    }
    if pii and pii.get("total"):
        headers["x-pls-pii"] = json.dumps(pii["files"], separators=(",", ":"))[:4000]
    stream = bool(body.get("stream"))

    async def send(token: str):
        return await http.send(
            http.build_request(
                "POST", url, json=body,
                headers={**headers, "authorization": f"Bearer {token}"},
            ),
            stream=stream,
        )

    started = time.monotonic()

    def elapsed() -> float:
        return round((time.monotonic() - started) * 1000, 1)

    try:
        response = await send(creds["access_token"])
        if response.status_code == 401:
            await response.aclose()
            creds = await refresh_credentials(creds)
            response = await send(creds["access_token"])
    except Exception as error:
        STATS["errors"] += 1
        message = str(error) or type(error).__name__
        log({"event": "cloud_error", "error": message})
        return JSONResponse({"error": {"message": message}}, status_code=502)

    STATS["requests"] += 1
    STATS["last"] = {"lane": "cloud", "path": path, "status": response.status_code}
    # headers_ms is the provider's time to start answering: everything the
    # model does before its first token, plus the network.
    log({"event": "cloud", "path": path, "status": response.status_code,
         "stream": stream, "content_type": response.headers.get("content-type"),
         "keys": sorted(body), "headers_ms": elapsed(), "attempt": attempt,
         "guard": guard})

    # An error, or a gateway that answered with JSON despite `stream`, is not
    # an event stream. Relaying it as one makes dsh report a transport error
    # ("Stream ended without finish_reason") instead of the real status.
    content_type = response.headers.get("content-type", "")
    event_stream = response.status_code < 400 and "text/event-stream" in content_type

    async def settle(calls: dict[int, dict], raw: bytes | None):
        """Accept, patch, re-ask or synthesise, for either body shape."""
        names = {call["name"] for call in calls.values()}
        if guard_verdict(names, policy, record):
            if SUBAGENT_TOOL in names and patch_call_args(calls, record):
                log({"event": "guard", "outcome": "patched", "attempt": attempt})
                if raw is not None:
                    return calls_stream(calls)
                return calls_json(calls)
            log({"event": "guard", "outcome": "accepted", "attempt": attempt,
                 "saw": sorted(names)})
            if raw is not None:
                return Response(raw, media_type="text/event-stream")
            return None

        STATS["guard_retries"] += 1
        log({"event": "guard", "outcome": "rejected", "attempt": attempt,
             "saw": sorted(names)})
        if attempt >= GUARD_RETRIES:
            STATS["guard_synth"] += 1
            if record and record.get("summary"):
                log({"event": "guard", "outcome": "synthesised", "task": record["id"]})
                return synthetic_call(record) if raw is not None else synthetic_json(record)
            log({"event": "guard", "outcome": "blocked", "saw": sorted(names)})
            reply = blocked_reply()
            return as_stream(reply) if raw is not None else JSONResponse(reply)

        saw = ", ".join(sorted(names)) or "no tool call"
        task_line = f" and begin its prompt with `{TASK_MARKER} {record['id']}`" if record else ""
        wrong = (f"{ROUTER_TAG} Wrong: you answered with {saw}. Call the "
                 f"`{SUBAGENT_TOOL}` tool now, nothing else{task_line}.")
        nudged = {**body, "messages": [*body.get("messages", []), {
            "role": "user", "content": wrong,
        }]}
        return await cloud(path, nudged, request, pii, guard=True,
                           policy=policy, record=record, attempt=attempt + 1)

    if not event_stream:
        payload = await response.aread()
        await response.aclose()
        log({"event": "cloud_done", "path": path, "total_ms": elapsed(), "attempt": attempt})
        if response.status_code == 401:
            payload = _unauthorized(payload)
            return Response(payload, status_code=401, media_type="application/json")
        if response.status_code >= 400:
            return Response(payload, status_code=response.status_code,
                            media_type="application/json")
        try:
            parsed = json.loads(payload)
        except Exception:
            parsed = None
        if guard and isinstance(parsed, dict):
            calls = calls_from_json(parsed)
            settled = await settle(calls, None)
            if settled is not None:
                return settled
            payload = _with_model(payload)
            return Response(payload, status_code=response.status_code,
                            media_type="application/json")
        try:
            usage = (parsed or {}).get("usage") or {}
            STATS["cloud_tokens"] += int(usage.get("total_tokens") or 0)
        except Exception:
            pass
        payload = _with_model(payload)
        return Response(payload, status_code=response.status_code,
                        media_type="application/json")

    def renamed(piece: bytes) -> bytes:
        if not config_cloud_model:
            return piece
        return piece.replace(f'"model":"{config_cloud_model}"'.encode(),
                             f'"model":"{MODEL_ID}"'.encode())

    async def guarded_stream():
        """Stream text as it arrives; hold tool calls until the guard decides.

        Text cannot run anything, so it goes straight to the harness. From the
        first tool-call delta on, every event is held, and the guard judges the
        calls once the reply ends: accepted calls are released (or rewritten),
        rejected ones are dropped. A rejected reply that showed no text yet is
        asked again; one that already showed text ends with the router's own
        subagent call, or a note when there is no task to delegate.
        """
        pending = b""
        held: list[bytes] = []
        holding = False
        streamed = False
        first_ms = None
        try:
            async for piece in response.aiter_bytes():
                if first_ms is None:
                    first_ms = elapsed()
                pending += renamed(piece)
                while b"\n\n" in pending:
                    event, pending = pending.split(b"\n\n", 1)
                    event += b"\n\n"
                    if not holding and b'"tool_calls"' in event:
                        holding = True
                    if holding or not carries_text(event):
                        held.append(event)
                        continue
                    for earlier in held:
                        yield earlier
                    held.clear()
                    yield event
                    streamed = True
            if pending:
                held.append(pending)
        finally:
            await response.aclose()
            log({"event": "cloud_done", "path": path, "first_byte_ms": first_ms,
                 "total_ms": elapsed(), "attempt": attempt, "streamed_text": streamed})

        raw = b"".join(held)
        calls = parse_calls(raw)
        names = {call["name"] for call in calls.values()}
        if guard_verdict(names, policy, record):
            if SUBAGENT_TOOL in names and patch_call_args(calls, record):
                log({"event": "guard", "outcome": "patched", "attempt": attempt})
                if not streamed:
                    yield sse(chunk("chatcmpl-pls-patch", int(time.time()),
                                    {"role": "assistant"})).encode()
                for event in call_events(calls):
                    yield event.encode()
                return
            log({"event": "guard", "outcome": "accepted", "attempt": attempt,
                 "saw": sorted(names)})
            yield raw
            return

        STATS["guard_retries"] += 1
        log({"event": "guard", "outcome": "rejected", "attempt": attempt,
             "saw": sorted(names), "streamed_text": streamed})
        if not streamed and attempt < GUARD_RETRIES:
            saw = ", ".join(sorted(names)) or "no tool call"
            task_line = (f" and begin its prompt with `{TASK_MARKER} {record['id']}`"
                         if record else "")
            wrong = (f"{ROUTER_TAG} Wrong: you answered with {saw}. Call the "
                     f"`{SUBAGENT_TOOL}` tool now, nothing else{task_line}.")
            nudged = {**body, "messages": [*body.get("messages", []), {
                "role": "user", "content": wrong,
            }]}
            retry = await cloud(path, nudged, request, pii, guard=True,
                                policy=policy, record=record, attempt=attempt + 1)
            async for part in body_parts(retry):
                yield part
            return

        STATS["guard_synth"] += 1
        if not streamed:
            yield sse(chunk("chatcmpl-pls-patch", int(time.time()),
                            {"role": "assistant"})).encode()
        if record and record.get("summary"):
            log({"event": "guard", "outcome": "synthesised", "task": record["id"]})
            synthetic = {0: {"name": SUBAGENT_TOOL, "arguments": ""}}
            patch_call_args(synthetic, record)
            for event in call_events(synthetic):
                yield event.encode()
            return
        log({"event": "guard", "outcome": "blocked", "saw": sorted(names)})
        note = blocked_reply()["choices"][0]["message"]["content"]
        for event in text_events(("\n\n" if streamed else "") + note):
            yield event.encode()

    if guard:
        return StreamingResponse(guarded_stream(), media_type="text/event-stream")

    async def relay():
        # Raw bytes, not lines: a provider may split one `data:` event across
        # several lines, and re-framing per line would cut it in half and lose
        # the finish chunk.
        seen_finish = False
        first_ms = None
        try:
            # aiter_bytes, not aiter_raw: raw yields still-compressed bytes when
            # the gateway gzips, and we do not forward its content-encoding.
            async for piece in response.aiter_bytes():
                if first_ms is None:
                    first_ms = elapsed()
                if b"finish_reason" in piece and b"finish_reason\":null" not in piece:
                    seen_finish = True
                yield renamed(piece)
            if not seen_finish:
                log({"event": "cloud_no_finish", "path": path})
        finally:
            await response.aclose()
            log({"event": "cloud_done", "path": path, "first_byte_ms": first_ms,
                 "total_ms": elapsed(), "attempt": attempt})

    return StreamingResponse(relay(), media_type="text/event-stream")



async def local(path: str, body: dict, request: Request, config: dict):
    global waiting

    summary, messages = resolve_child(list(body.get("messages") or []), config["policy"])
    body = {**body, "messages": messages}

    overflow = False
    queued = False
    if local_sem.locked():
        if waiting >= QUEUE_MAX:
            overflow = True
        else:
            waiting += 1
            queued = True
            STATS["queued"] += 1
    # Under privacy a subagent never overflows to the cloud, or its task
    # reaches it after all; it waits for the slot instead.
    if overflow and (config["policy"] == "privacy" or summary == "local_only"):
        overflow = False
    if overflow:
        STATS["overflow"] += 1
        response = await cloud(path, body, request)
        response.headers["x-pls-overflow"] = "1"
        return response

    try:
        async with local_sem:
            if queued:
                waiting = max(0, waiting - 1)
                queued = False
            return await local_inner(path, body, request, config, summary)
    finally:
        # Cancelled while still queued: nothing else gives the slot back.
        if queued:
            waiting = max(0, waiting - 1)


async def local_inner(path: str, body: dict, request: Request, config: dict, summary: str):
    key = session_key(request, body)
    local_sessions.add(key)
    # A key can serve several child runs; a fresh run starts with a shorter
    # transcript than the previous one ended with, so the cap restarts there.
    depth = len(body.get("messages") or [])
    count, last_depth = turns.get(key, (0, 0))
    count = count + 1 if depth > last_depth else 1
    turns[key] = (count, depth)
    STATS["local_turns"] += 1

    messages = [dict(m) for m in body.get("messages") or []]
    body = {**body, "messages": messages}
    if config["policy"] == "privacy" and isinstance(body.get("tools"), list):
        # `send_message` delivers text to the parent untouched, around the
        # summary-type filter on the final answer; under privacy the final
        # answer is the only way out.
        body["tools"] = [t for t in body["tools"]
                         if (t.get("function") or {}).get("name") not in CHILD_DENIED]
    inject_child_rules(body, summary)
    client_stream = bool(body.get("stream"))
    body = {**body, "stream": False}
    capped = count >= config["max_turns"]
    if capped:
        STATS["cap_hits"] += 1
        messages.append({"role": "user", "content": WRAP_UP})
        body = {**body, "messages": messages, "tool_choice": "none"}

    try:
        port = await ensure_model_server()
        response = await http.post(
            f"http://127.0.0.1:{port}/{path.lstrip('/')}",
            json=body,
            headers={"x-session-affinity": key},
        )
    except Exception as error:
        STATS["errors"] += 1
        log({"event": "local_error", "error": str(error)})
        return JSONResponse({"error": {"message": str(error)}}, status_code=502)

    if response.status_code != 200:
        STATS["errors"] += 1
        return Response(response.content, status_code=response.status_code,
                        media_type="application/json")

    try:
        result = response.json()
    except Exception:
        STATS["errors"] += 1
        log({"event": "local_bad_body", "status": response.status_code,
             "body": response.text[:400]})
        return Response(response.content, status_code=502,
                        media_type="application/json")
    choice = (result.get("choices") or [{}])[0]
    message = choice.get("message") or {}
    calls = message.get("tool_calls") or []
    usage = result.get("usage") or {}
    STATS["local_tokens"] += int(usage.get("total_tokens") or 0)

    # The summary type only applies to a final answer; a tool call must pass
    # through intact or the child cannot continue.
    if not calls and message.get("content"):
        if summary == "local_only":
            message["content"] = WITHHELD
            STATS["disclosure"]["local_only"] += 1
        else:
            # A short answer summarises into something vaguer than itself.
            if len(message["content"]) > 400:
                message["content"] = await summarise(message["content"])
            message["content"] = short_paths(redact(message["content"]))
            STATS["disclosure"]["summary"] += 1
        choice["message"] = message

    log({"event": "local_done", "turns": count, "capped": capped,
         "summary": summary, "prompt_tokens": usage.get("prompt_tokens"),
         "completion_tokens": usage.get("completion_tokens"),
         "calls": len(calls)})
    STATS["requests"] += 1
    STATS["last"] = {
        "lane": "local", "path": path, "summary": summary,
        "turns": count, "capped": capped,
        "prompt_tokens": usage.get("prompt_tokens"),
        "completion_tokens": usage.get("completion_tokens"),
    }
    log({"event": "local", **STATS["last"]})

    if client_stream:
        want_usage = bool((body.get("stream_options") or {}).get("include_usage"))
        return as_stream(result, want_usage)
    return JSONResponse(result)


# --------------------------------------------------------------------------
# lifecycle
# --------------------------------------------------------------------------


def cleanup() -> None:
    global shutting_down
    if shutting_down:
        return
    shutting_down = True
    kill_model_server()
    try:
        PID_PATH.unlink()
    except Exception:
        pass


async def watch_parent() -> None:
    while True:
        await asyncio.sleep(2)
        alive = True
        if explicit_parent:
            try:
                os.kill(explicit_parent, 0)
            except ProcessLookupError:
                alive = False
            except PermissionError:
                pass
        elif parent_pid != 1:
            alive = os.getppid() == parent_pid
        if not alive:
            log({"event": "parent_gone"})
            cleanup()
            os._exit(0)


def on_signal(signum, _frame):
    cleanup()
    os._exit(0)


def main() -> None:
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    if not CONFIG_PATH.exists():
        CONFIG_PATH.write_text(json.dumps({"mode": DEFAULT_MODE, "policy": DEFAULT_POLICY,
                                           "max_turns": DEFAULT_MAX_TURNS}))
    load_tasks()
    PID_PATH.write_text(str(os.getpid()))
    atexit.register(cleanup)
    signal.signal(signal.SIGTERM, on_signal)
    signal.signal(signal.SIGINT, on_signal)

    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", 0))
    sock.listen(128)
    port = sock.getsockname()[1]
    print(json.dumps({"port": port}), flush=True)

    async def serve():
        asyncio.create_task(watch_parent())
        server = uvicorn.Server(uvicorn.Config(app, log_level="warning", lifespan="off"))
        await server.serve(sockets=[sock])

    asyncio.run(serve())


if __name__ == "__main__":
    main()
