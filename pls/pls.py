"""PLS — Proxy LocalHost:Server.

A loopback router with one endpoint, `/v1/chat/completions`, for the main
chat and every subagent. Each request is classified from its own body:

    subagent   first prompt starts `pls-task: tN`  -> local model server
    side       no tools (a session title)          -> cloud, private text masked
    main       everything else                     -> policy flow, then cloud

Only PLS writes the `pls-task:` line: it stamps every `subagent` call the
parent makes. Under a policy (`privacy` or `cost`) a local decision model,
served by wally over its unix socket, reads each new main-chat prompt and
decides what the cloud parent is told to do: delegate to a local subagent,
and with which summary type, or do it itself. Subagent requests are never
decided, and a subagent cannot launch another one. Mode and policy come from
a JSON file re-read on every request. The local model server is spawned
lazily and killed on exit, including on SIGTERM and when the parent
disappears.

Binds port 0 and prints `{"port": N}` on stdout once listening.
"""

import asyncio
import atexit
import contextvars
import dataclasses
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

import pii_model
from mask_store import MASK_SAFETY, TOKEN_RE, MaskStore, store_for

try:
    import watch
except ImportError:
    watch = None
try:
    import kvc
except ImportError:
    kvc = None

MODEL_ID = "Hybrid-Wally-0.1"
# The harness names subagents with this id so it compacts their context at the
# local model's window. Routing never reads it: the `pls-task:` marker decides.
LOCAL_MODEL_ID = "Hybrid-Wally-0.1-local"
DEFAULT_MODE = "hybrid"
MODES = ("hybrid", "cloud")
DEFAULT_POLICY = "privacy"
POLICIES = ("privacy", "cost")
DEFAULT_MAX_TURNS = 20
QUEUE_MAX = 8
SUMMARY_MAX_TOKENS = 512
# Same budget as DECIDE_TIMEOUT in src/harness/pls.rs, model load included.
# The first prompt of a session usually pays for that load.
DECIDE_TIMEOUT_S = 120
# A tool-less side call (dsh's session title) cannot delegate. A private
# prompt is replaced with this so the title request does not carry it.
PRIVATE_SIDE_MASK = "[private: kept on this device]"

WITHHELD = (
    "[content withheld: this subagent ran under local-only disclosure — its "
    "result was kept on your device]"
)
WRAP_UP = (
    "You have reached the turn limit. Wrap up now: synthesize the best result "
    "available and answer in text only."
)
# --------------------------------------------------------------------------
# harness profiles: what the parent's spawn tool looks like, per harness
# --------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Profile:
    """One harness's subagent surface, as PLS needs to read and write it.

    `prompt_path` is where the child's first user message comes from inside
    the spawn call's arguments: `("prompt",)`, Hermes' `("tasks", "*",
    "goal")` (one child per task), OpenClaw's `("task",)`. `field` is the
    same in words, for the instructions the cloud parent reads.
    """
    id: str
    spawn_tool: str
    prompt_path: tuple[str, ...]
    field: str
    # Filled when the parent's call leaves them empty; the harness rejects a
    # call without them.
    required: tuple[tuple[str, str], ...]
    # Tools a local child loses: anything that launches another child or
    # talks to the parent around the summary filter.
    child_denied: frozenset[str]
    # Tools the parent keeps under privacy: the spawn tool and whatever it
    # needs to collect what children return.
    parent_tools: frozenset[str]
    # Text the harness puts in front of the child's prompt. Tolerated before
    # the marker; `strip_prefix` also removes it from what the child sees.
    child_prefix: re.Pattern | None = None
    strip_prefix: bool = False
    # Text the harness appends to the child's prompt (dsh's report-back note).
    child_note: re.Pattern | None = None
    # `user` messages the harness writes itself, not the person.
    injected: tuple[str, ...] = ("<system-reminder>",)
    # A child's output handed to the parent as a `user` message.
    agent_report: re.Pattern | None = None
    # Spawn arguments the parent may not set: a `model` sends the child
    # wherever it names, including the cloud.
    forbidden_args: tuple[str, ...] = ()
    # Spawn calls with one of these `action` values do not start a child.
    non_spawn_actions: tuple[str, ...] = ()


OPENCLAW_CONTEXT = "<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>"

PROFILES = {
    "deepseek": Profile(
        id="deepseek",
        spawn_tool="subagent",
        prompt_path=("prompt",),
        field="prompt",
        required=(("description", "Run the task on this device"),),
        child_denied=frozenset({"subagent", "subagent_fork", "send_message"}),
        parent_tools=frozenset({"subagent"}),
        # dsh appends this to every child's prompt with no separator, so a
        # prompt that is only the marker arrives as `pls-task: …-t1Your
        # parent agent id is …`. It asks for `send_message`, which a local
        # child does not have.
        child_note=re.compile(
            r"\s*Your parent agent id is \"[^\"]*\"\. Before you finish, send your result.*\Z",
            re.DOTALL),
        injected=("<system-reminder>", "Current runtime context."),
        agent_report=re.compile(
            r"^\s*(Agent [0-9a-f-]{36} sent a message:|Background subagent [0-9a-f-]{36} )"),
    ),
    # opencode 2.x: `subagent`, with the agent named, and a one-line preface
    # on every child prompt.
    "opencode-v2": Profile(
        id="opencode-v2",
        spawn_tool="subagent",
        prompt_path=("prompt",),
        field="prompt",
        required=(("agent", "general"), ("description", "Run the task on this device")),
        child_denied=frozenset({"subagent", "task"}),
        parent_tools=frozenset({"subagent"}),
        child_prefix=re.compile(r"\s*You are a subagent spawned by another session\.[ \t]*\n?"),
        strip_prefix=True,
    ),
    # opencode 1.x, and OpenWork, which pins its engine to 1.x.
    "opencode-v1": Profile(
        id="opencode-v1",
        spawn_tool="task",
        prompt_path=("prompt",),
        field="prompt",
        required=(("subagent_type", "general"), ("description", "Run the task on this device")),
        child_denied=frozenset({"subagent", "task"}),
        parent_tools=frozenset({"task"}),
    ),
    # Hermes runs one child per entry of `tasks`; `action: list|steer|stop`
    # manages running ones and starts nothing.
    "hermes": Profile(
        id="hermes",
        spawn_tool="delegate_task",
        prompt_path=("tasks", "*", "goal"),
        field="goal (each task's `goal`)",
        required=(),
        child_denied=frozenset({"delegate_task", "send_message"}),
        parent_tools=frozenset({"delegate_task"}),
        non_spawn_actions=("list", "steer", "stop"),
    ),
    # OpenClaw wraps the child's task in its own context block, and reports a
    # child's result back the same way.
    "openclaw": Profile(
        id="openclaw",
        spawn_tool="sessions_spawn",
        prompt_path=("task",),
        field="task",
        required=(),
        child_denied=frozenset({"sessions_spawn", "subagents"}),
        parent_tools=frozenset({"sessions_spawn", "sessions_yield", "subagents",
                                "sessions_list", "sessions_history", "agents_wait"}),
        child_prefix=re.compile(
            re.escape(OPENCLAW_CONTEXT) + r".*?<<<END_OPENCLAW_INTERNAL_CONTEXT>>>\s*",
            re.DOTALL),
        agent_report=re.compile(r"^\s*" + re.escape(OPENCLAW_CONTEXT)),
        forbidden_args=("model",),
    ),
}
PROFILE_ALIASES = {"opencode": "opencode-v2", "openwork": "opencode-v1"}
DEFAULT_PROFILE = "deepseek"
# The profile of the request being served; `route` sets it from config.json.
PROFILE: contextvars.ContextVar[Profile] = contextvars.ContextVar(
    "pls_profile", default=PROFILES[DEFAULT_PROFILE])


def profile() -> Profile:
    return PROFILE.get()


def profile_named(name: Any) -> Profile:
    name = PROFILE_ALIASES.get(name, name)
    return PROFILES.get(name) if isinstance(name, str) and name in PROFILES else PROFILES[DEFAULT_PROFILE]


def policy_prompt(policy: str) -> str:
    """The cloud parent's job under `policy`, appended to its system prompt once."""
    tool, field = profile().spawn_tool, profile().field
    if policy == "privacy":
        return (
            "\n\nYou are the orchestrator in a privacy-first pair. Your only jobs are "
            f"to talk with the user, launch the `{tool}` tool, and relay what it "
            "returns. The subagent is the worker: it plans builds and explores files, "
            "folders and the system on the user's device. You never read, write or "
            "run anything yourself, and you have no other tool. When an instruction "
            "carries a `pls-task:` line, copy that line exactly as the first line of "
            f"the subagent's {field}. A result that only says the subagent started "
            "is not its report: wait for the report and do not launch another."
        )
    return (
        "\n\nYou are the planner in a cost-saving pair. Talk with the user and "
        f"plan with them. Hand coding, execution and debugging to the `{tool}` "
        "tool, which runs on the user's device. Do a task yourself only when the "
        "router says it is small. When an instruction carries a `pls-task:` line, "
        f"copy that line exactly as the first line of the subagent's {field}."
    )


# Where a delegated task names itself on the subagent prompt. The router, not
# the cloud model, owns what the id means, so the cloud cannot loosen it.
TASK_MARKER = "pls-task:"
# `<session>-tN`: numbered from t1 in each session, prefixed so a child, which
# carries no session of its own, still names exactly one task. Used with
# `.match`, so it is anchored wherever the search starts.
TASK_RE = re.compile(r"\s*pls-task:\s*((?:[0-9a-f]{6}-)?t\d+)(?!\d)[ \t]*\n?")


def marker_in(text: str) -> re.Match | None:
    """The `pls-task:` line at the start of `text`, after the profile's prefix."""
    prefix = profile().child_prefix
    start = 0
    if prefix is not None and (found := prefix.match(text)):
        start = found.end()
    return TASK_RE.match(text, start)


def find_marker(messages: list) -> tuple[int, re.Match] | None:
    """The message that starts with a `pls-task:` line, if the first prompt
    or a harness message before it does. OpenClaw wraps a child's task in a
    context block that would otherwise read as harness text."""
    index = first_prompt(messages)
    last = index if index is not None else len(messages) - 1
    for position, message in enumerate(messages[: last + 1]):
        if message.get("role") != "user":
            continue
        found = marker_in(message_text(message))
        if found:
            return position, found
    return None


ROUTER_TAG = "[router]"

# The guard is router-owned: the router says what to call and checks the
# answer. A rejected reply is dropped and asked again; after GUARD_RETRIES the
# router emits the subagent call itself.
GUARD_RETRIES = 2

# Told to the local subagent, so it knows who sees its answer. The router
# enforces the same rule on the answer afterwards.
CHILD_RULES = {
    "summary": (
        " When you finish, end with a short summary of what you found or did. "
        "Only that summary goes back to the main agent, so keep file paths, "
        "names and numbers in it, and do not paste file contents or raw values."
    ),
    "local_only": (
        " When you finish, write a detailed report of what you found or did. "
        "Keep file paths, names and numbers, and do not paste file contents. "
        "Personal values in that report are replaced before the main agent sees it."
    ),
}
# Every child gets this line too, whatever its summary type.
CHILD_NO_SPAWN_RULE = (
    " You are a subagent running on this device. You cannot launch subagents "
    "or message other agents; do the whole task yourself."
)

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

PLS_SOCKET = os.environ.get("PLS_SOCKET", "")
# Per-launch secret. The port is loopback but shared, so without it any local
# process could spend the account's cloud quota through this bearer token.
TOKEN = os.environ.get("PLS_TOKEN", "")
REQUEST_ID: contextvars.ContextVar[str] = contextvars.ContextVar("pls_req", default="")
TRACE_CHARS = 16000
privacy_gap_noted = False

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
WATCH_DIR = CONFIG_DIR / "watch"

app = FastAPI()
if watch is not None:
    watch.start(lambda: WATCH_DIR)
http = httpx.AsyncClient(timeout=httpx.Timeout(600.0, connect=10.0))


@app.middleware("http")
async def trace_exchange(request: Request, call_next):
    """Remember each /v1 request and its reply in the watch log."""
    if watch is None or not request.url.path.startswith("/v1/"):
        return await call_next(request)
    req_id = secrets.token_hex(4)
    token = REQUEST_ID.set(req_id)
    request.scope["headers"] = [*request.scope["headers"], (b"x-pls-req", req_id.encode())]
    raw = await request.body()
    try:
        parsed = json.loads(raw) if raw else {}
    except Exception:
        parsed = {}
    if isinstance(parsed, dict):
        log({"event": "in", "req": req_id, "method": request.method,
             "path": request.url.path, "body": trace_body(parsed)})
    try:
        response = await call_next(request)
    finally:
        REQUEST_ID.reset(token)
    return attach_out(response, req_id)

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
    "phone": re.compile(
        r"(?<![\w/])(?:\+\d{1,3}[\s.-]\d{2,4}[\s.-]\d{3,8}"
        r"|\(\d{3}\)[\s.-]*\d{3}[\s.-]\d{4}|\b\d{3}[\s.-]\d{3}[\s.-]\d{4}\b)"
    ),
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


def message_text(message: dict) -> str:
    content = message.get("content")
    if isinstance(content, list):
        return " ".join(p.get("text", "") for p in content if isinstance(p, dict))
    return str(content or "")


def is_injected(message: dict) -> bool:
    """Harness context sent as a `user` message (dsh's <system-reminder> and
    runtime snapshot). Not what the person asked, so routing skips it."""
    return (message.get("role") == "user"
            and message_text(message).lstrip().startswith(profile().injected))


def is_agent_message(message: dict) -> bool:
    """A child's output handed to the parent as a `user` message. Not the
    person speaking, so it is never decided or instructed."""
    report = profile().agent_report
    return (report is not None and message.get("role") == "user"
            and bool(report.match(message_text(message))))


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
# One mask file per harness. A test sets an entry; the router does not replace it.
mask_stores: dict[str, MaskStore] = {}
classify_pii = pii_model.classify
# The mask session of the child request being served. The child's own
# x-session-id is not a store key.
ACTIVE_MASK_SESSION: contextvars.ContextVar[str] = contextvars.ContextVar(
    "pls_mask_session", default="")
# Next task number per session.
SESSION_NEXT: dict[str, int] = {}
# The session of the main-chat request being served, for tasks the router
# creates while relaying the reply (`ad_hoc_record`).
CURRENT_SESSION: contextvars.ContextVar[str] = contextvars.ContextVar(
    "pls_session", default="000000")


def prompt_key(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()[:16]


def session_of(messages: list) -> str:
    """A session is named by its first prompt, which never changes."""
    index = first_prompt(messages)
    text = message_text(messages[index]) if index is not None else ""
    return prompt_key(text)[:6]


def next_task_id(session: str) -> str:
    number = SESSION_NEXT.get(session, 1)
    SESSION_NEXT[session] = number + 1
    return f"{session}-t{number}"


def prompt_turns(messages: list) -> list[list[int]]:
    """Indices of the person's prompts, grouped by turn.

    Prompts with no assistant reply between them are one turn and one task,
    so the parent is never handed two task lines at once.
    """
    turns: list[list[int]] = []
    current: list[int] = []
    for index, message in enumerate(messages):
        if message.get("role") == "assistant":
            if current:
                turns.append(current)
                current = []
        elif is_prompt(message):
            current.append(index)
    if current:
        turns.append(current)
    return turns


def turn_text(messages: list, turn: list[int]) -> str:
    return "\n\n".join(message_text(messages[i]) for i in turn)


def turn_record(messages: list, turn: list[int], session: str) -> dict | None:
    return TASKS.get(f"{session}:{prompt_key(turn_text(messages, turn))}")


def is_pending(messages: list, turn: list[int]) -> bool:
    """Whether `turn` is the newest message no assistant has answered."""
    last = last_turn({"messages": messages})
    return last is not None and last is messages[turn[-1]]


def persisted(record: dict) -> bool:
    """Only privacy records outlive the router: they keep a resumed session
    masked. A cost record is re-decided each launch, so a decision made with
    an older question or model is never reused."""
    return record.get("policy") == "privacy"


def load_tasks() -> None:
    """Restore privacy records so a resumed session stays masked.

    The file holds ``masked`` and ``mask_session``, not the raw prompt. A
    missing in-memory prompt is rebuilt by demasking that pair.
    """
    try:
        data = json.loads(TASKS_PATH.read_text())
    except (OSError, ValueError):
        return
    if not isinstance(data, dict):
        return
    store = None
    for key, record in data.items():
        if isinstance(record, dict) and isinstance(record.get("id"), str) and persisted(record):
            TASKS[key] = record
            session, _, number = record["id"].rpartition("-t")
            if session and number.isdigit():
                SESSION_NEXT[session] = max(SESSION_NEXT.get(session, 1), int(number) + 1)
            masked = record.get("masked")
            mask_session = record.get("mask_session")
            if (record["id"] not in TASK_TEXT and isinstance(masked, str)
                    and isinstance(mask_session, str)):
                if store is None:
                    store = open_mask_store(read_config()["harness"])
                if store is not None:
                    TASK_TEXT[record["id"]] = store.demask(mask_session, masked)


def save_tasks() -> None:
    try:
        TASKS_PATH.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(TASKS_PATH, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as handle:
            json.dump({key: r for key, r in TASKS.items() if persisted(r)}, handle)
        os.chmod(TASKS_PATH, 0o600)
    except OSError as error:
        log({"event": "tasks_save_failed", "error": str(error)})


def new_record(policy: str, verdict: dict | None, session: str) -> dict:
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
    return {"id": next_task_id(session), "policy": policy, "private": private,
            "big": big, "summary": summary, "decided": decided}


async def decide(policy: str, text: str) -> dict | None:
    """Ask wally's decision model about one prompt. None on any failure."""
    if not PLS_SOCKET:
        return None
    payload = json.dumps({"policy": policy, "text": text}).encode()
    try:
        reader, writer = await asyncio.open_unix_connection(PLS_SOCKET)
        writer.write(
            b"POST /decide HTTP/1.1\r\nHost: local\r\ncontent-type: application/json\r\n"
            + f"content-length: {len(payload)}\r\n\r\n".encode() + payload
        )
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(), DECIDE_TIMEOUT_S)
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


async def ensure_tasks(body: dict, policy: str, request: Request | None = None) -> list[dict]:
    """Decide every prompt in the request that has no settled record yet.

    Returns the records created or re-decided by this call. Under privacy the
    decision sees the raw prompt, then the prompt is span-masked once, and
    that result is what ``save_tasks`` writes.
    """
    fresh = []
    messages = body.get("messages") or []
    session = session_of(messages)
    for turn in prompt_turns(messages):
        text = turn_text(messages, turn)
        key = f"{session}:{prompt_key(text)}"
        existing = TASKS.get(key)
        if existing and existing.get("policy") == policy and existing.get("decided"):
            TASK_TEXT.setdefault(existing["id"], text)
            continue
        verdict = await decide(policy, text)
        STATS["decisions"] += 1
        if verdict is None:
            STATS["decision_failures"] += 1
        record = new_record(policy, verdict, session)
        if existing and existing.get("policy") == policy:
            # A retry keeps the id the cloud may already have been told.
            record["id"] = existing["id"]
        if policy == "privacy":
            record["mask_session"] = mask_session_id(request, messages)
            record["masked"] = await mask_cloud_text(text, record["mask_session"])
        delegate = await decide("delegate", text)
        STATS["decisions"] += 1
        if delegate is None:
            STATS["decision_failures"] += 1
        # A failed delegate call still appends, so a file task is not answered
        # in the cloud. Only a clear "no" drops the launch line.
        record["instruct"] = not (delegate and delegate.get("answer") == "no")
        TASKS[key] = record
        TASK_TEXT[record["id"]] = text
        fresh.append(record)
        log({"event": "decided", "task": record["id"], "policy": policy,
             "private": record["private"], "big": record["big"],
             "summary": record["summary"], "decided": record["decided"],
             "instruct": record["instruct"],
             "p_yes": (verdict or {}).get("p_yes")})
    if fresh:
        save_tasks()
    return fresh


def placeholder(record: dict) -> str:
    return (
        "[The user's request is private and stays on this device.] Launch the "
        f"`{profile().spawn_tool}` tool now. Its {profile().field} must be exactly:\n"
        f"{TASK_MARKER} {record['id']}"
    )


def instruction(record: dict) -> str:
    if record.get("summary") is None:
        return f"{ROUTER_TAG} This task is small: do it yourself, no subagent."
    return (
        f"{ROUTER_TAG} Launch the `{profile().spawn_tool}` tool for this task. Begin its "
        f"{profile().field} with the line `{TASK_MARKER} {record['id']}`, then state the task."
    )


def wants_instruction(record: dict) -> bool:
    """A missing or failed delegate verdict still appends the launch line."""
    return record.get("instruct", True) is not False


def rewrite_prompt(message: dict, record: dict) -> dict:
    if record["policy"] == "privacy":
        return {**message, "content": privacy_cloud_text(record, wants_instruction(record))}
    if not wants_instruction(record):
        return message
    extra = "\n\n" + instruction(record)
    content = message.get("content")
    if isinstance(content, list):
        return {**message, "content": [*content, {"type": "text", "text": extra}]}
    return {**message, "content": str(content or "") + extra}


def open_mask_store(harness: str | None = None) -> MaskStore | None:
    """The harness's store. A key a test already set is left in place."""
    harness = profile().id if harness is None else harness
    existing = mask_stores.get(harness)
    if existing is not None:
        return existing
    try:
        created = store_for(harness)
    except ValueError:
        return None
    return mask_stores.setdefault(harness, created)


def mask_session_id(request, messages) -> str:
    """x-session-id when that header is non-empty, else session_of(messages).

    No other header is read.
    """
    if request is not None:
        value = request.headers.get("x-session-id")
        if value:
            return value
    return session_of(messages)


_masking_off_logged = False


def note_masking_off(reason: str) -> None:
    """One log line when a privacy turn cannot be span-masked."""
    global _masking_off_logged
    if _masking_off_logged:
        return
    _masking_off_logged = True
    log({"event": "masking_off", "error": reason})


async def mask_cloud_text(text: str, session_id: str) -> str | None:
    """Masked text, or None on classify failure or MaskError.

    Uses pls.mask_stores[profile().id]. Tests set that entry to a
    MaskStore on a temp file. There is no pls.MASK_STORE. Another
    harness is another dict entry and another file.
    """
    try:
        spans = await classify_pii(text)
    except Exception as error:
        note_masking_off(str(error) or type(error).__name__)
        return None
    if not isinstance(spans, list):
        note_masking_off("classifier returned no span list")
        return None
    spans = [span for span in spans if not _span_is_path(text, span)]
    spans = _merge_spans(spans + _sensitive_path_spans(text))
    store = open_mask_store()
    if store is None:
        note_masking_off("mask store unavailable")
        return None
    try:
        return store.mask(session_id, text, spans)
    except Exception as error:
        note_masking_off(str(error) or type(error).__name__)
        return None


def _span_is_path(text: str, span: dict) -> bool:
    """A workspace path is not personal data. Masking it hides the file from the parent."""
    if not isinstance(span, dict):
        return False
    start, end = span.get("start"), span.get("end")
    if isinstance(start, bool) or isinstance(end, bool):
        return False
    if not isinstance(start, int) or not isinstance(end, int):
        return False
    if start < 0 or end > len(text) or start >= end:
        return False
    piece = text[start:end].strip().strip("`\"'")
    name = piece.rstrip("/").rsplit("/", 1)[-1]
    if _SENSITIVE_FILE.search(name):
        return False
    return piece.startswith(("/", "~/", "./", "../"))


_SENSITIVE_FILE = re.compile(
    r"(?i)(credential|secret|password|passwd|\.pem$|\.env$|id_rsa|api[_-]?key)"
)
_PATH_IN_TEXT = re.compile(
    r"(?<![\w.])(?:~|(?:\./|\.\./)|/)[^\s\"'`<>]*"
)


def _merge_spans(spans: list[dict]) -> list[dict]:
    """Drop a later span that overlaps one already kept."""
    ordered = sorted(spans, key=lambda span: (span.get("start", 0), span.get("end", 0)))
    kept: list[dict] = []
    for span in ordered:
        start, end = span.get("start"), span.get("end")
        if not isinstance(start, int) or not isinstance(end, int):
            continue
        if kept and start < kept[-1]["end"]:
            continue
        kept.append(span)
    return kept


def _sensitive_path_spans(text: str) -> list[dict]:
    """Paths whose file name is a secret, even when the classifier skipped them."""
    spans = []
    for match in _PATH_IN_TEXT.finditer(text):
        raw = match.group(0).rstrip(".,);]")
        name = raw.rstrip("/").rsplit("/", 1)[-1]
        if not _SENSITIVE_FILE.search(name):
            continue
        start = match.start()
        spans.append({"start": start, "end": start + len(raw), "category": "path"})
    return spans


def _remask_content(store, session_id: str, content):
    """Known values become tokens. A failed piece is left as it arrived."""
    if isinstance(content, str):
        return store.remask(session_id, content)
    if isinstance(content, list):
        parts = []
        changed = False
        for part in content:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                text = store.remask(session_id, part["text"])
                if text != part["text"]:
                    changed = True
                    parts.append({**part, "text": text})
                    continue
            parts.append(part)
        return parts if changed else content
    return content


def remask_assistant_text(messages: list, session_id: str) -> list:
    """Mask known values in the chat the cloud will read. The system prompt stays."""
    store = open_mask_store()
    if store is None or not session_id:
        return messages
    updated = []
    for message in messages:
        if message.get("role") == "system":
            updated.append(message)
            continue
        content = _remask_content(store, session_id, message.get("content"))
        updated.append(message if content == message.get("content") else {**message, "content": content})
    return updated


def demask_local_text(text: str, session_id: str) -> str:
    """demask against pls.mask_stores[profile().id].

    Drops a system message whose content is exactly MASK_SAFETY.
    Does not edit other system messages.
    """
    if text == MASK_SAFETY:
        return text
    store = open_mask_store()
    if store is None:
        return text
    return store.demask(session_id, text)


def split_open_token(text: str) -> tuple[str, str]:
    """Hold a trailing `⟦pii:…` that has not closed yet."""
    start = text.rfind("⟦")
    if start < 0 or "⟧" in text[start:]:
        return text, ""
    return text[:start], text[start:]


class ReplyDemask:
    """Restore tokens in a streamed reply, holding one that is split mid-way."""

    def __init__(self, session_id: str):
        self.session_id = session_id
        self.hold = ""

    def feed(self, text: str, final: bool = False) -> str:
        self.hold += text
        if final:
            ready, self.hold = self.hold, ""
        else:
            ready, self.hold = split_open_token(self.hold)
        if not ready:
            return ""
        return demask_local_text(ready, self.session_id)

    def flush(self) -> str:
        return self.feed("", final=True)


def _field_sse(field: str, text: str) -> bytes:
    return sse(chunk("chatcmpl-pls-demask", int(time.time()), {field: text})).encode()


def _content_sse(text: str) -> bytes:
    return _field_sse("content", text)


class ReplyDemasks:
    """Separate restore buffers, so a split token cannot cross fields."""

    def __init__(self, session_id: str):
        self.content = ReplyDemask(session_id)
        self.reasoning = ReplyDemask(session_id)

    def for_field(self, field: str) -> ReplyDemask:
        if field == "reasoning_content":
            return self.reasoning
        return self.content

    def flush_all(self) -> list[tuple[str, str]]:
        return [
            ("content", self.content.flush()),
            ("reasoning_content", self.reasoning.flush()),
        ]


def map_sse_event(event: bytes, demask: ReplyDemasks | None) -> list[bytes]:
    """Demask text deltas. A non-text event flushes a held token first."""
    if demask is None or not event.strip():
        return [event]
    raw = event.strip()
    if not raw.startswith(b"data: ") or raw == b"data: [DONE]":
        return _flush_then(demask, event)
    try:
        payload = json.loads(raw[len(b"data: "):])
    except ValueError:
        return _flush_then(demask, event)
    found = []
    finishing = False
    for choice in payload.get("choices") or []:
        if choice.get("finish_reason"):
            finishing = True
        delta = choice.get("delta")
        if not isinstance(delta, dict) or delta.get("tool_calls"):
            return _flush_then(demask, event)
        text = delta.get("content")
        reasoning = delta.get("reasoning_content")
        if isinstance(text, str) and text:
            found.append((delta, "content"))
        if isinstance(reasoning, str) and reasoning:
            found.append((delta, "reasoning_content"))
    if not found:
        return _flush_then(demask, event)
    emit = finishing
    for delta, field in found:
        ready = demask.for_field(field).feed(delta[field], final=finishing)
        if ready:
            delta[field] = ready
            emit = True
        else:
            delta.pop(field, None)
    if not emit:
        return []
    return [sse(payload).encode()]


def _flush_then(demask: ReplyDemasks, event: bytes) -> list[bytes]:
    out = [
        _field_sse(field, ready)
        for field, ready in demask.flush_all()
        if ready
    ]
    if not out:
        return [event]
    out.append(event)
    return out


def demask_sse_bytes(raw: bytes, session_id: str) -> bytes:
    if not raw or not session_id:
        return raw
    demask = ReplyDemasks(session_id)
    out: list[bytes] = []
    pending = raw
    while b"\n\n" in pending:
        event, pending = pending.split(b"\n\n", 1)
        out.extend(map_sse_event(event + b"\n\n", demask))
    if pending:
        out.extend(map_sse_event(pending, demask))
    for field, ready in demask.flush_all():
        if ready:
            out.append(_field_sse(field, ready))
    return b"".join(out)


def demask_reply(parsed: dict, session_id: str) -> None:
    """Restore tokens in a buffered assistant message. Tool calls stay masked."""
    if not session_id:
        return
    for choice in parsed.get("choices") or []:
        message = choice.get("message")
        if isinstance(message, dict):
            for field in ("content", "reasoning_content"):
                if isinstance(message.get(field), str):
                    message[field] = demask_local_text(message[field], session_id)


def attach_mask_safety(messages: list) -> None:
    """Insert {"role": "system", "content": MASK_SAFETY} at index 0
    when no message has that exact content. A second call inserts nothing.
    """
    if any(message.get("content") == MASK_SAFETY for message in messages):
        return
    messages.insert(0, {"role": "system", "content": MASK_SAFETY})


async def mask_summary(text: str, session_id: str) -> str:
    """Span-mask text onto session_id.

    None from mask_cloud_text returns pls.SUMMARISE_FAILED.
    """
    masked = await mask_cloud_text(text, session_id)
    if masked is None:
        return SUMMARISE_FAILED
    return masked


def privacy_cloud_text(record: dict, instruct: bool) -> str:
    """What a privacy prompt shows the cloud. A failed mask withholds it."""
    masked = record.get("masked")
    if not isinstance(masked, str):
        return placeholder(record)
    if instruct:
        return masked + "\n\n" + instruction(record)
    return masked


def _has_mask_token(messages: list) -> bool:
    for message in messages:
        content = message.get("content")
        if isinstance(content, str) and TOKEN_RE.search(content):
            return True
        if isinstance(content, list):
            for part in content:
                if (isinstance(part, dict) and isinstance(part.get("text"), str)
                        and TOKEN_RE.search(part["text"])):
                    return True
    return False


def _drop_mask_safety(messages: list) -> list:
    return [
        message for message in messages
        if not (message.get("role") == "system" and message.get("content") == MASK_SAFETY)
    ]


def apply_policy(body: dict, policy: str, session_id: str = "") -> dict:
    """The cloud-bound copy of `body` under `policy`. Never mutates `body`.

    Every prompt with a record is rewritten the same way on every request, so
    a private prompt stays masked on later turns and the cloud prefix is stable.
    """
    original = body.get("messages") or []
    # A child's report is already span-masked on the way out of the local
    # lane. Leave that text in place so the cloud parent can read it.
    messages = list(original)
    session = session_of(original)
    for turn in prompt_turns(original):
        record = turn_record(original, turn, session)
        if record is None:
            continue
        pending = is_pending(original, turn)
        if record.get("policy") == "privacy":
            # Every privacy prompt is span-masked. Only the unanswered turn
            # carries the instruction, so the parent never sees two at once.
            for index in turn:
                messages[index] = {
                    **original[index],
                    "content": privacy_cloud_text(
                        record, pending and index == turn[-1] and wants_instruction(record)),
                }
        elif pending:
            messages[turn[-1]] = rewrite_prompt(original[turn[-1]], record)
    shaped = {**body, "messages": messages}

    prompt = policy_prompt(policy)
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

    if policy == "privacy" and _has_mask_token(messages):
        attach_mask_safety(messages)

    if policy == "privacy" and isinstance(body.get("tools"), list):
        shaped["tools"] = [t for t in body["tools"]
                           if (t.get("function") or {}).get("name") in profile().parent_tools]
    if policy == "privacy" and session_id:
        shaped["messages"] = remask_assistant_text(shaped["messages"], session_id)
    return shaped


def mask_private_prompts(body: dict) -> dict:
    """Replace privacy prompts and add nothing else.

    A side call has no tools. The span-masked text is what leaves; a failed
    mask sends the placeholder, not the raw prompt and not PRIVATE_SIDE_MASK.
    """
    original = body.get("messages") or []
    messages = list(original)
    session = session_of(original)
    for turn in prompt_turns(original):
        record = turn_record(original, turn, session)
        if record and record.get("policy") == "privacy":
            pending = is_pending(original, turn)
            for index in turn:
                messages[index] = {
                    **original[index],
                    "content": privacy_cloud_text(
                        record, pending and index == turn[-1] and wants_instruction(record)),
                }
    if _has_mask_token(messages):
        attach_mask_safety(messages)
    return {**body, "messages": messages}


def note_privacy_gap() -> None:
    """One terminal line when privacy cannot keep the turn on device."""
    global privacy_gap_noted
    if privacy_gap_noted:
        return
    privacy_gap_noted = True
    print(
        "privacy: the local model cannot run, so subagent work is withheld "
        "rather than sent to the cloud",
        file=sys.stderr,
        flush=True,
    )


def pending_task(body: dict) -> dict | None:
    """The record of a prompt no assistant turn has answered yet, if any."""
    messages = body.get("messages") or []
    turns = prompt_turns(messages)
    if not turns or not is_pending(messages, turns[-1]):
        return None
    return turn_record(messages, turns[-1], session_of(messages))


def first_prompt(messages: list) -> int | None:
    """Index of the first message the person wrote, if any."""
    for index, message in enumerate(messages):
        if is_prompt(message):
            return index
    return None


def classify(body: dict, headers: Any = None) -> str:
    """`subagent`, `side` or `main`, from the request alone.

    A request is a subagent when any of these hold:
    - it asks for the local model id (dsh, Hermes and OpenClaw name it on
      every child request, from their own config);
    - it carries `x-parent-session-id` (opencode sends it on child requests
      only, whatever model the child asked for);
    - its first prompt starts with a `pls-task:` line. Only the router writes
      one, so this holds whether or not its id is still known.
    """
    if (body.get("model") == LOCAL_MODEL_ID
            or (headers is not None and headers.get("x-parent-session-id"))
            or find_marker(body.get("messages") or [])):
        return "subagent"
    if not body.get("tools"):
        return "side"
    return "main"


def unmark(text: str) -> str:
    """`text` without its `pls-task:` line, and without the harness prefix
    when the profile drops it."""
    found = marker_in(text)
    if found is None:
        return text.strip()
    head = "" if profile().strip_prefix else text[: found.start()]
    return (head + text[found.end():]).strip()


def strip_marker(messages: list) -> list:
    """The messages with the `pls-task:` line taken out of the message that has it.

    For a subagent body that goes to the cloud: the marker means nothing there.
    """
    marked = find_marker(messages)
    if marked is None:
        return messages
    index = marked[0]
    message = messages[index]
    content = message.get("content")
    updated = list(messages)
    if isinstance(content, str):
        updated[index] = {**message, "content": unmark(content)}
    elif isinstance(content, list):
        parts = list(content)
        for position, part in enumerate(parts):
            if (isinstance(part, dict) and isinstance(part.get("text"), str)
                    and marker_in(part["text"])):
                parts[position] = {**part, "text": unmark(part["text"])}
                break
        updated[index] = {**message, "content": parts}
    return updated


def ad_hoc_record(policy: str) -> dict:
    """A task for a `subagent` call the parent made with no pending task.

    Its text is already in the parent's own call, so it is never private.
    Under privacy the answer is span-masked on the way back.
    """
    session = CURRENT_SESSION.get()
    record = {"id": next_task_id(session), "policy": policy,
              "private": False, "big": True,
              "summary": "local_only" if policy == "privacy" else "summary",
              "decided": True, "ad_hoc": True, "mask_session": session}
    TASKS[f"ad-hoc:{record['id']}"] = record
    if persisted(record):
        save_tasks()
    log({"event": "ad_hoc_task", "task": record["id"], "policy": policy,
         "summary": record["summary"]})
    return record


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
    ACTIVE_MASK_SESSION.set("")
    messages = _drop_mask_safety(drop_parent_note(messages))
    marked = find_marker(messages)
    if marked is None:
        log({"event": "child_without_marker"})
        return fallback, messages
    index, found = marked
    task_id = found.group(1)
    record = next((r for r in TASKS.values() if r.get("id") == task_id), None)
    if record is None or record.get("summary") is None:
        log({"event": "child_unknown_task", "task": task_id})
        return fallback, strip_marker(messages)
    mask_session = record.get("mask_session") if isinstance(record.get("mask_session"), str) else ""
    if not mask_session:
        # An ad-hoc child has no session of its own. Its task id is
        # `<chat>-tN`, and the parent restores tokens from that chat.
        prefix, sep, _number = task_id.rpartition("-t")
        if sep and prefix:
            mask_session = prefix
            record["mask_session"] = prefix
    ACTIVE_MASK_SESSION.set(mask_session)
    if (task_id not in TASK_TEXT and isinstance(record.get("masked"), str) and mask_session):
        TASK_TEXT[task_id] = demask_local_text(record["masked"], mask_session)
    text = message_text(messages[index])
    rest = text[found.end():].strip()
    # A private task's text never went to the cloud; a parent that sent only
    # the marker gets its task back too, so no child starts blind.
    if task_id in TASK_TEXT and (record["private"] or not rest):
        rest = TASK_TEXT[task_id] + (f"\n\n{rest}" if rest else "")
    # OpenClaw's context block stays: its runtime reads it. opencode 2's
    # one-line preface goes.
    head = "" if profile().strip_prefix else text[: found.start()]
    content = head + rest
    if mask_session and messages[index].get("role") != "system":
        content = demask_local_text(content, mask_session)
    updated = list(messages)
    updated[index] = {**messages[index], "content": content}
    return record["summary"], updated


def drop_parent_note(messages: list) -> list:
    """The messages with the harness's report-back note (dsh's) cut off the
    first prompt."""
    note = profile().child_note
    index = first_prompt(messages)
    if note is None or index is None or not isinstance(messages[index].get("content"), str):
        return messages
    content = messages[index]["content"]
    cut = note.sub("", content)
    if cut == content:
        return messages
    updated = list(messages)
    updated[index] = {**messages[index], "content": cut}
    return updated


def inject_child_rules(body: dict, summary: str) -> None:
    """Append the child's rules to its system prompt, each at most once.

    A child with no system message gets one, so the no-subagents line always
    reaches it.
    """
    rules = [r for r in (CHILD_RULES.get(summary), CHILD_NO_SPAWN_RULE) if r]
    messages = body.setdefault("messages", [])
    for message in messages:
        if message.get("role") != "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            texts = [p.get("text") for p in content if isinstance(p, dict)]
            message["content"] = [*content, *({"type": "text", "text": r}
                                              for r in rules if r not in texts)]
        else:
            text = str(content or "")
            message["content"] = text + "".join(r for r in rules if r not in text)
        return
    messages.insert(0, {"role": "system", "content": "".join(rules).lstrip()})


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


def call_arguments(call: dict) -> dict:
    """A call's arguments as an object; anything else reads as empty."""
    try:
        arguments = json.loads(call.get("arguments") or "{}")
    except ValueError:
        return {}
    return arguments if isinstance(arguments, dict) else {}


def prompt_slots(arguments: dict, create: bool) -> list[tuple[dict, str]]:
    """Where each child's first prompt lives in a spawn call's arguments, as
    (object, key) pairs. `create` adds the containers a bare call lacks."""
    *path, key = profile().prompt_path
    holders: list = [arguments]
    for position, step in enumerate(path):
        list_next = position + 1 < len(path) and path[position + 1] == "*"
        found: list = []
        for holder in holders:
            if step == "*":
                if isinstance(holder, list):
                    if create and not any(isinstance(item, dict) for item in holder):
                        holder.append({})
                    found.extend(item for item in holder if isinstance(item, dict))
                continue
            value = holder.get(step)
            if create and not isinstance(value, list if list_next else dict):
                value = holder[step] = [] if list_next else {}
            if isinstance(value, (dict, list)):
                found.append(value)
        holders = found
    return [(holder, key) for holder in holders if isinstance(holder, dict)]


def is_spawn(call: dict) -> bool:
    """Whether `call` starts a child (Hermes' `delegate_task` also lists,
    steers and stops running ones)."""
    if call.get("name") != profile().spawn_tool:
        return False
    return call_arguments(call).get("action") not in profile().non_spawn_actions


def known_task(task_id: str) -> dict | None:
    return next((record for record in TASKS.values() if record.get("id") == task_id), None)


def prompt_needs_task(prompt: str) -> bool:
    """A spawn prompt whose `pls-task` id was not issued by this router."""
    found = marker_in(prompt)
    return found is None or known_task(found.group(1)) is None


def patch_call_args(calls: dict[int, dict], record: dict | None) -> bool:
    """Make every spawn call carry a prompt that names its task.

    A pinned `tool_choice` makes the gateway strip arguments, so a call can
    arrive as `subagent {}`; the harness rejects that, so the fields it
    requires are filled. A call without the task line gets it, so the
    subagent side knows its summary type. Arguments the parent may not set
    (OpenClaw's `model`, which would send the child to the cloud) are removed.
    """
    changed = False
    for call in calls.values():
        if not is_spawn(call):
            continue
        arguments = call_arguments(call)
        for name in profile().forbidden_args:
            if name in arguments:
                del arguments[name]
                changed = True
        if record is not None:
            for holder, key in prompt_slots(arguments, create=True):
                prompt = str(holder.get(key) or "")
                found = marker_in(prompt)
                if found is not None and known_task(found.group(1)) is None:
                    prompt = (prompt[: found.start()] + prompt[found.end():]).lstrip()
                    found = None
                if not prompt:
                    holder[key] = subagent_prompt(record)
                    changed = True
                elif found is None:
                    holder[key] = f"{TASK_MARKER} {record['id']}\n\n{prompt}"
                    changed = True
        for name, value in profile().required:
            if not arguments.get(name):
                arguments[name] = value
                changed = True
        call["arguments"] = json.dumps(arguments)
    return changed


def call_prompts(call: dict) -> list[str]:
    """Each child prompt in a spawn call; one empty prompt when it has none."""
    slots = prompt_slots(call_arguments(call), create=False)
    return [str(holder.get(key) or "") for holder, key in slots] or [""]


def session_has_delegate() -> bool:
    """Whether this chat already has a task a subagent was told to run."""
    session = CURRENT_SESSION.get()
    if not session:
        return False
    prefix = f"{session}-t"
    return any(str(record.get("id", "")).startswith(prefix) and record.get("summary")
               for record in TASKS.values())


def drop_spawn_calls(calls: dict[int, dict]) -> None:
    for index, call in list(calls.items()):
        if is_spawn(call):
            del calls[index]


def stamp_subagent_calls(calls: dict[int, dict], record: dict | None, policy: str) -> bool:
    """Make every spawn call in a parent reply carry a `pls-task:` line.

    With one endpoint the marker is the only thing that tells a child request
    from main chat. An unmarked call takes the pending task when it has one
    that delegates, otherwise a fresh `ad_hoc_record`. A second spawn while
    that task already exists is removed instead. Returns whether any call
    changed.
    """
    spawn_calls = [c for c in calls.values() if is_spawn(c)]
    if not spawn_calls:
        return False
    task = record if record and record.get("summary") else None
    if task is not None and len(spawn_calls) > 1:
        kept = False
        for index, call in list(calls.items()):
            if not is_spawn(call):
                continue
            if kept:
                del calls[index]
            else:
                kept = True
        log({"event": "extra_spawn_dropped", "task": task["id"]})
    elif task is None and session_has_delegate() and any(
            prompt_needs_task(prompt) for c in spawn_calls for prompt in call_prompts(c)):
        drop_spawn_calls(calls)
        log({"event": "extra_spawn_dropped", "session": CURRENT_SESSION.get()})
        return False
    elif task is None and any(prompt_needs_task(prompt)
                              for c in spawn_calls for prompt in call_prompts(c)):
        task = ad_hoc_record(policy)
    return patch_call_args(calls, task)


WAIT_FOR_CHILD = (
    "[router] A subagent is already working on this. Wait for its report "
    "and relay that. Do not launch another."
)


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
    calls = {0: {"name": profile().spawn_tool, "arguments": ""}}
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


def log(event: dict) -> None:
    req = REQUEST_ID.get()
    if req and "req" not in event:
        event = {**event, "req": req}
    event = {"ts": round(time.time(), 3), **event}
    print(json.dumps(event), file=sys.stderr, flush=True)
    if watch is not None:
        try:
            watch.record(event)
        except Exception:
            pass


def clip_value(value: Any) -> Any:
    """A debug-panel copy. Long payloads keep a preview instead of the whole body."""
    try:
        text = json.dumps(value, ensure_ascii=False)
    except TypeError:
        text = str(value)
    if len(text) <= TRACE_CHARS:
        return value
    return {"truncated": True, "chars": len(text), "preview": text[:TRACE_CHARS]}


def clip_bytes(raw: bytes) -> Any:
    text = raw.decode("utf-8", errors="replace")
    truncated = len(text) > TRACE_CHARS
    if truncated:
        text = text[:TRACE_CHARS]
    try:
        parsed = json.loads(text)
    except Exception:
        parsed = None
    if parsed is not None and not truncated:
        return parsed
    if truncated:
        return {"truncated": True, "chars": len(raw), "preview": text}
    return text


def trace_body(body: dict) -> dict:
    """What a request carried, without message text. The bearer token never arrives here."""
    kept: dict = {}
    for key in ("model", "stream", "tool_choice", "max_tokens"):
        if key in body:
            kept[key] = body[key]
    messages = body.get("messages")
    if isinstance(messages, list):
        kept["messages"] = [
            {"role": message.get("role"), "chars": len(message_text(message))}
            if isinstance(message, dict) else {"role": "", "chars": 0}
            for message in messages
        ]
    tools = body.get("tools")
    if isinstance(tools, list):
        kept["tools"] = [
            ((tool.get("function") or {}).get("name") or "")
            if isinstance(tool, dict) else ""
            for tool in tools
        ]
    return clip_value(kept)


def attach_out(response, req_id: str):
    """Record the response body once the client has read it."""
    iterator = response.body_iterator

    async def wrapped():
        chunks = []
        size = 0
        async for chunk in iterator:
            data = chunk.encode() if isinstance(chunk, str) else bytes(chunk)
            if size < TRACE_CHARS:
                chunks.append(data[: TRACE_CHARS - size])
                size += len(data)
            yield chunk
        log({"event": "out", "req": req_id, "status": response.status_code,
             "stream": isinstance(response, StreamingResponse),
             "body": clip_bytes(b"".join(chunks))})

    response.body_iterator = wrapped()
    return response


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
        "harness": profile_named(data.get("harness")).id,
        "cloud_limits": model_limits(data.get("cloud_limits")),
    }


# The local lane's window and reply cap: the model server's own defaults.
LOCAL_LIMITS = {
    "context_length": int(os.environ.get("PLS_MAX_KV", "65536")),
    "max_completion_tokens": int(os.environ.get("PLS_MAX_OUT", "8192")),
}


def model_limits(value: Any) -> dict:
    """`{context_length, max_completion_tokens}` from config, positive ints
    only; the local limits stand in for any that is missing."""
    limits = dict(LOCAL_LIMITS)
    if isinstance(value, dict):
        for key in limits:
            number = value.get(key)
            if isinstance(number, int) and not isinstance(number, bool) and number > 0:
                limits[key] = number
    return limits


def read_credentials() -> dict:
    try:
        return json.loads(CREDENTIALS_PATH.read_text())
    except Exception:
        return {}


def write_credentials(creds: dict) -> None:
    """Create the credential file at mode 0600. chmod after write leaves a window."""
    CREDENTIALS_PATH.parent.mkdir(parents=True, exist_ok=True)
    tmp = CREDENTIALS_PATH.with_suffix(".json.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        json.dump(creds, handle)
        handle.write("\n")
    os.replace(tmp, CREDENTIALS_PATH)


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


async def ask_wally(path: str) -> dict | None:
    """One JSON object from wally's socket. None when wally is absent."""
    if not PLS_SOCKET:
        return None
    try:
        reader, writer = await asyncio.open_unix_connection(PLS_SOCKET)
        writer.write(f"GET {path} HTTP/1.1\r\nHost: local\r\n\r\n".encode())
        await writer.drain()
        raw = await asyncio.wait_for(reader.read(8192), 2)
        writer.close()
    except (OSError, TimeoutError):
        return None
    head, _, body = raw.partition(b"\r\n\r\n")
    if not head.startswith(b"HTTP/1.1 200"):
        return None
    try:
        payload = json.loads(body)
    except ValueError:
        return None
    return payload if isinstance(payload, dict) else None


async def cloud_credentials() -> dict:
    """The cloud token. Wally owns refresh when the socket is up.

    A router beside wally must not rotate the refresh token itself: wally's
    `/token` does that and writes the profile. Doing both signs the
    account out when the console rotates the token.
    """
    fresh = await ask_wally("/token")
    if fresh and fresh.get("access_token") and fresh.get("console_url"):
        return {"access_token": fresh["access_token"], "console_url": fresh["console_url"]}
    creds = read_credentials()
    if PLS_SOCKET:
        return creds
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
        env["PLS_MODEL_ID"] = LOCAL_MODEL_ID
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


def token_rejected(request: Request):
    if TOKEN and not secrets.compare_digest(request.headers.get("x-pls-token", ""), TOKEN):
        return JSONResponse(
            {"error": {"message": "missing or wrong x-pls-token"}}, status_code=401
        )
    return None



@app.get("/favicon.ico")
def favicon():
    return Response(b"", media_type="image/x-icon", status_code=204)




def withheld_completion(stream: bool) -> Response:
    """A local answer that never left the device, for a lane we cannot run."""
    result = {
        "id": "chatcmpl-pls-withheld",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": MODEL_ID,
        "choices": [{"index": 0, "finish_reason": "stop", "message": {
            "role": "assistant",
            "content": WITHHELD,
        }}],
    }
    if stream:
        return as_stream(result)
    return JSONResponse(result)


@app.api_route("/{path:path}", methods=["GET", "POST"])
async def route(path: str, request: Request):
    req_id = request.headers.get("x-pls-req", "")
    if req_id:
        REQUEST_ID.set(req_id)
    if path == "v1/models":
        # Limits ride along: Hermes reads a custom endpoint's window from
        # here and otherwise assumes 256K for both ids.
        limits = {MODEL_ID: read_config()["cloud_limits"], LOCAL_MODEL_ID: LOCAL_LIMITS}
        return {"object": "list", "data": [
            {"id": model_id, "object": "model", "owned_by": "wally", **limits[model_id]}
            for model_id in (MODEL_ID, LOCAL_MODEL_ID)
        ]}

    if rejected := token_rejected(request):
        return rejected

    # One API: `/local/v1` and anything else outside `/v1` no longer exist.
    if not path.startswith("v1/"):
        return JSONResponse({"error": {"message": f"no route /{path}"}}, status_code=404)

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
    PROFILE.set(PROFILES[config["harness"]])

    kind = classify(body, request.headers)
    hybrid = config["mode"] == "hybrid"
    lane = "local" if hybrid and kind == "subagent" else "cloud"
    messages = body.get("messages") or []
    log({"event": "route", "path": path, "kind": kind, "lane": lane,
         "mode": config["mode"], "policy": policy, "messages": len(messages)})

    # An explicit cloud mode forwards the harness body. The marker only means
    # something to this router, so it does not travel.
    if not hybrid:
        if kind == "subagent":
            body = {**body, "messages": strip_marker(messages)}
        return await cloud(path, body, request)

    # A subagent is never decided: its task was settled on the parent's turn.
    if kind == "subagent":
        return await local(path, body, request, config)

    # A side call is masked rather than instructed, so a session title does
    # not echo the delegate line. Cost forwards it as the harness sent it.
    if kind == "side":
        if policy == "privacy":
            await ensure_tasks(body, policy, request)
            return await cloud(path, mask_private_prompts(body), request)
        return await cloud(path, body, request)

    CURRENT_SESSION.set(session_of(messages))
    await ensure_tasks(body, policy, request)
    record = pending_task(body)
    session_id = mask_session_id(request, messages) if policy == "privacy" else ""
    shaped = apply_policy(body, policy, session_id)

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
    return await cloud(path, shaped, request, pii, guard=guard,
                       policy=policy, record=record, parent=True,
                       mask_session=session_id)


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


def write_calls_json(payload: dict, calls: dict[int, dict]) -> None:
    """Put rewritten arguments back into a buffered completion, text intact."""
    for choice in payload.get("choices") or []:
        for index, call in enumerate((choice.get("message") or {}).get("tool_calls") or []):
            if index in calls and isinstance(call.get("function"), dict):
                call["function"]["arguments"] = calls[index]["arguments"]


def synthetic_json(record: dict) -> JSONResponse:
    """`synthetic_call` for a client that asked for a single JSON answer."""
    calls = {0: {"name": profile().spawn_tool, "arguments": ""}}
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


def must_spawn(record: dict | None) -> bool:
    """The delegate decision already said this pending task needs a subagent."""
    return bool(record and record.get("summary") and wants_instruction(record))


def guard_verdict(names: set[str], policy: str, record: dict | None) -> bool:
    """Whether a cloud reply with these tool calls may reach the harness.

    `record` is the task of a prompt no assistant turn has answered yet.
    A task the delegate call marked `instruct` false may be answered in text.
    """
    if policy == "privacy":
        if not names <= profile().parent_tools:
            return False
        # The result tools alone do not start the pending task.
        return profile().spawn_tool in names if must_spawn(record) else True
    if must_spawn(record):
        return profile().spawn_tool in names
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
                attempt: int = 0, parent: bool = False, mask_session: str = ""):
    """Send `body` to the cloud and relay its reply.

    `guard` judges the reply's tool calls; `parent` marks a main-chat reply,
    whose `subagent` calls are stamped with a `pls-task:` line whether or not
    it is guarded.
    """
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
            if PLS_SOCKET:
                fresh = await ask_wally("/token?refresh=1")
                if fresh and fresh.get("access_token"):
                    creds = {
                        "access_token": fresh["access_token"],
                        "console_url": fresh.get("console_url") or creds.get("console_url", ""),
                    }
            else:
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
            if profile().spawn_tool in names and stamp_subagent_calls(calls, record, policy):
                log({"event": "guard", "outcome": "patched", "attempt": attempt})
                if raw is not None:
                    return calls_stream(calls)
                return calls_json(calls)
            if profile().spawn_tool in names and not any(is_spawn(c) for c in calls.values()):
                log({"event": "guard", "outcome": "extra_spawn_dropped", "attempt": attempt})
                note = {"id": "chatcmpl-pls-wait", "object": "chat.completion",
                        "created": int(time.time()), "model": MODEL_ID,
                        "choices": [{"index": 0, "finish_reason": "stop", "message": {
                            "role": "assistant", "content": WAIT_FOR_CHILD}}]}
                return as_stream(note) if raw is not None else JSONResponse(note)
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
            if must_spawn(record):
                log({"event": "guard", "outcome": "synthesised", "task": record["id"]})
                return synthetic_call(record) if raw is not None else synthetic_json(record)
            log({"event": "guard", "outcome": "blocked", "saw": sorted(names)})
            reply = blocked_reply()
            return as_stream(reply) if raw is not None else JSONResponse(reply)

        saw = ", ".join(sorted(names)) or "no tool call"
        task_line = f" and begin its prompt with `{TASK_MARKER} {record['id']}`" if record else ""
        wrong = (f"{ROUTER_TAG} Wrong: you answered with {saw}. Call the "
                 f"`{profile().spawn_tool}` tool now, nothing else{task_line}.")
        nudged = {**body, "messages": [*body.get("messages", []), {
            "role": "user", "content": wrong,
        }]}
        return await cloud(path, nudged, request, pii, guard=True,
                           policy=policy, record=record, attempt=attempt + 1,
                           parent=parent, mask_session=mask_session)

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
        if policy == "privacy" and parent and isinstance(parsed, dict):
            demask_reply(parsed, mask_session or mask_session_id(
                request, body.get("messages") or []))
            payload = json.dumps(parsed).encode()
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
        if parent and isinstance(parsed, dict):
            calls = calls_from_json(parsed)
            if stamp_subagent_calls(calls, record, policy):
                write_calls_json(parsed, calls)
                log({"event": "stamped", "attempt": attempt})
                payload = json.dumps(parsed).encode()
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

        Unguarded (a cost parent reply with no delegated task) every call is
        accepted, and only `subagent` arguments are rewritten to carry a mark.
        """
        pending = b""
        held: list[bytes] = []
        holding = False
        streamed = False
        first_ms = None
        reply_session = ""
        if policy == "privacy" and parent:
            reply_session = mask_session or mask_session_id(
                request, body.get("messages") or [])
        demask = ReplyDemasks(reply_session) if reply_session else None
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
                        for out in map_sse_event(earlier, demask):
                            yield out
                    held.clear()
                    for out in map_sse_event(event, demask):
                        if out:
                            streamed = True
                        yield out
            if pending:
                held.append(pending)
        finally:
            await response.aclose()
            log({"event": "cloud_done", "path": path, "first_byte_ms": first_ms,
                 "total_ms": elapsed(), "attempt": attempt, "streamed_text": streamed})

        if demask is not None:
            for field, rest in demask.flush_all():
                if rest:
                    yield _field_sse(field, rest)
                    streamed = True
        raw = b"".join(held)
        if reply_session:
            raw = demask_sse_bytes(raw, reply_session)
        calls = parse_calls(raw)
        names = {call["name"] for call in calls.values()}
        if not guard or guard_verdict(names, policy, record):
            if profile().spawn_tool in names and stamp_subagent_calls(calls, record, policy):
                log({"event": "guard" if guard else "stamped", "outcome": "patched",
                     "attempt": attempt})
                if not streamed:
                    yield sse(chunk("chatcmpl-pls-patch", int(time.time()),
                                    {"role": "assistant"})).encode()
                for event in call_events(calls):
                    yield event.encode()
                return
            if profile().spawn_tool in names and not any(is_spawn(c) for c in calls.values()):
                log({"event": "guard", "outcome": "extra_spawn_dropped", "attempt": attempt})
                if not streamed:
                    yield sse(chunk("chatcmpl-pls-patch", int(time.time()),
                                    {"role": "assistant"})).encode()
                for event in text_events(WAIT_FOR_CHILD):
                    yield event.encode()
                return
            if guard:
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
                     f"`{profile().spawn_tool}` tool now, nothing else{task_line}.")
            nudged = {**body, "messages": [*body.get("messages", []), {
                "role": "user", "content": wrong,
            }]}
            retry = await cloud(path, nudged, request, pii, guard=True,
                                policy=policy, record=record, attempt=attempt + 1,
                                parent=parent, mask_session=mask_session)
            async for part in body_parts(retry):
                yield part
            return

        STATS["guard_synth"] += 1
        if not streamed:
            yield sse(chunk("chatcmpl-pls-patch", int(time.time()),
                            {"role": "assistant"})).encode()
        if must_spawn(record):
            log({"event": "guard", "outcome": "synthesised", "task": record["id"]})
            synthetic = {0: {"name": profile().spawn_tool, "arguments": ""}}
            patch_call_args(synthetic, record)
            for event in call_events(synthetic):
                yield event.encode()
            return
        log({"event": "guard", "outcome": "blocked", "saw": sorted(names)})
        note = blocked_reply()["choices"][0]["message"]["content"]
        for event in text_events(("\n\n" if streamed else "") + note):
            yield event.encode()

    if guard or parent:
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

    # Checked before the private text is restored, so a cost fallback to the
    # cloud carries only what the parent already wrote.
    try:
        await ensure_model_server()
    except (OSError, RuntimeError, ValueError, KeyError) as error:
        log({"event": "local_unavailable", "policy": config["policy"],
             "error": str(error) or type(error).__name__})
        if config["policy"] == "privacy":
            note_privacy_gap()
            return withheld_completion(bool(body.get("stream")))
        return await cloud(path, {**body, "messages": strip_marker(body.get("messages") or [])},
                           request)

    summary, messages = resolve_child(list(body.get("messages") or []), config["policy"])
    if kvc is not None:
        messages = kvc.order(messages, is_injected)
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
            log({"event": "queued", "waiting": waiting})
    # Under privacy a subagent never overflows to the cloud, or its task
    # reaches it after all; it waits for the slot instead.
    if overflow and (config["policy"] == "privacy" or summary == "local_only"):
        overflow = False
    if overflow:
        STATS["overflow"] += 1
        log({"event": "overflow"})
        response = await cloud(path, {**body, "messages": strip_marker(body["messages"])},
                               request)
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


def _forward_local_event(event: dict, dropped_spawn: list) -> dict | None:
    """One model-server chunk, with a child spawn call removed.

    `dropped_spawn` is a one-item flag the finish chunk reads, so a reply
    that only tried to spawn a child ends as a normal stop.
    """
    choice = (event.get("choices") or [{}])[0]
    delta = choice.get("delta") or {}
    calls = delta.get("tool_calls") or []
    denied = profile().child_denied
    if calls:
        kept = [c for c in calls if (c.get("function") or {}).get("name") not in denied]
        if len(kept) != len(calls):
            log({"event": "child_spawn_blocked",
                 "saw": sorted({(c.get("function") or {}).get("name") for c in calls} - {None})})
            if not kept:
                dropped_spawn.append(True)
                return None
            choice = {**choice, "delta": {**delta, "tool_calls": kept}}
            event = {**event, "choices": [choice]}
    if dropped_spawn and choice.get("finish_reason") == "tool_calls":
        event = {**event, "choices": [{**choice, "finish_reason": "stop"}]}
    return event


async def stream_local(port: int, path: str, body: dict, key: str,
                       count: int, capped: bool, summary: str):
    """Relay the model server's stream as each token arrives.

    Buffering the reply and emitting it in one chunk makes the harness record
    a decode window of about a millisecond, so a short subagent answer shows
    up as tens of thousands of tokens per second.
    """
    url = f"http://127.0.0.1:{port}/{path.lstrip('/')}"
    sent = {**body, "stream": True, "stream_options": {"include_usage": True}}

    async def relay():
        usage: dict = {}
        dropped: list = []
        try:
            async with http.stream(
                "POST", url, json=sent, headers={"x-session-affinity": key}
            ) as response:
                if response.status_code != 200:
                    STATS["errors"] += 1
                    yield await response.aread()
                    return
                async for line in response.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    data = line[5:].strip()
                    if data == "[DONE]":
                        yield b"data: [DONE]\n\n"
                        continue
                    try:
                        event = json.loads(data)
                    except json.JSONDecodeError:
                        continue
                    if event.get("usage"):
                        usage = event["usage"]
                    forwarded = _forward_local_event(event, dropped)
                    if forwarded is None:
                        continue
                    yield f"data: {json.dumps(forwarded)}\n\n".encode()
        except Exception as error:
            STATS["errors"] += 1
            log({"event": "local_error", "error": str(error)})
            return
        STATS["local_tokens"] += int(usage.get("total_tokens") or 0)
        STATS["requests"] += 1
        STATS["last"] = {
            "lane": "local", "path": path, "summary": summary,
            "turns": count, "capped": capped,
            "prompt_tokens": usage.get("prompt_tokens"),
            "completion_tokens": usage.get("completion_tokens"),
        }
        log({"event": "local_done", "turns": count, "capped": capped,
             "summary": summary, "prompt_tokens": usage.get("prompt_tokens"),
             "completion_tokens": usage.get("completion_tokens"), "stream": True})
        log({"event": "local", **STATS["last"]})

    return StreamingResponse(relay(), media_type="text/event-stream")


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
    if isinstance(body.get("tools"), list):
        # Removing the tools is the enforcement; the prompt line only explains.
        body["tools"] = [t for t in body["tools"]
                         if (t.get("function") or {}).get("name") not in profile().child_denied]
    inject_child_rules(body, summary)
    client_stream = bool(body.get("stream"))
    if not client_stream:
        body = {**body, "stream": False}
    capped = count >= config["max_turns"]
    if capped:
        STATS["cap_hits"] += 1
        messages.append({"role": "user", "content": WRAP_UP})
        body = {**body, "messages": messages, "tool_choice": "none"}

    try:
        port = await ensure_model_server()
        if client_stream:
            return await stream_local(port, path, body, key, count, capped, summary)
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
    # The tools are gone, but a model can still write such a call in text.
    blocked = [c for c in calls if (c.get("function") or {}).get("name") in profile().child_denied]
    if blocked:
        calls = [c for c in calls if c not in blocked]
        log({"event": "child_spawn_blocked",
             "saw": sorted({(c.get("function") or {}).get("name") for c in blocked})})
        if calls:
            message["tool_calls"] = calls
        else:
            message.pop("tool_calls", None)
            message["content"] = message.get("content") or ""
            choice["finish_reason"] = "stop"
        choice["message"] = message
    usage = result.get("usage") or {}
    STATS["local_tokens"] += int(usage.get("total_tokens") or 0)

    # The summary type only applies to a final answer; a tool call must pass
    # through intact or the child cannot continue.
    if not calls and message.get("content"):
        text = message["content"]
        # Cost keeps a long answer short. Privacy keeps the full report and
        # only records its values, so the chat can show them.
        if summary != "local_only" and config["policy"] != "privacy" and len(text) > 400:
            text = await summarise(text)
        if config["policy"] == "privacy":
            masked = await mask_summary(text, ACTIVE_MASK_SESSION.get())
            if masked == SUMMARISE_FAILED:
                text = masked
            message["content"] = text
        else:
            message["content"] = short_paths(redact(text))
        STATS["disclosure"]["local_only" if summary == "local_only" else "summary"] += 1
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
