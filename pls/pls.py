"""PLS — Proxy LocalHost:Server.

A loopback router that exposes one model id and two lanes:

    /v1/...            cloud
    /local/v1/...      local MLX model, result summarised
    /local-only/v1/... local MLX model, result withheld

The harness is pointed at this process by wally. Mode comes from a JSON file
re-read on every request. The local model server is spawned lazily and killed
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
DELEGATION = (
    "\n\nYou are the cloud half of a hybrid pair. File exploration — reading, "
    "grepping, listing, summarising files — belongs to the `subagent` tool: it "
    "runs on a local model and its result comes back filtered. Do it yourself "
    "only for small things, or when the subagent fails."
    "\n\nBegin every `subagent` prompt with a disclosure marker on its own line:"
    "\n  disclosure: local_only  — nothing but a placeholder comes back; use when the file must not reach the cloud at all."
    "\n  disclosure: title      — a one-line title comes back; use for a file inventory."
    "\n  disclosure: summary    — a short summary comes back; the default for reading content."
    "\n  disclosure: none       — the full result comes back; use when you need exact text."
)

# File tools the parent loses in hybrid, so exploration has to go through the
# subagent instead of relying on the model's judgement.
PARENT_DENIED = {"read", "grep", "glob"}
# The gateway drops tool arguments when `tool_choice` is pinned, so a forced
# call arrives as `subagent {}` and dsh rejects it. Off until it is fixed.
FORCE_DELEGATION = False

# The guard is router-owned, not model-owned: the router says what to call and
# checks the answer. A reply that reaches for a file tool is dropped and asked
# again; after GUARD_RETRIES the router emits the call itself. PII turns also
# get a spoken instruction and a forced mode; a plain work turn keeps the mode
# the model named.
PII_MODE = "local_only"
WORK_MODE = "summary"
GUARD_RETRIES = 2
PII_TOOL = "subagent"
NUDGE_PREFIX = "Wrong: you answered with"
INSTRUCTION_PREFIX = "Route this turn locally:"

WORK_RE = re.compile(
    r"(\b(read|list|find|search|grep|open|summar|review|explain|audit|inspect)\b"
    r"|/[\w.-]+|\bsrc/|\bdocs/)",
    re.I,
)

# The cloud names the disclosure mode on the child prompt, the way PPLX's
# `disclosureMode` field does. First token wins; the marker is stripped.
DISCLOSURE_RE = re.compile(r"^\s*disclosure:\s*(local_only|summary|title|none)\b[ \t]*\n?", re.I)

# Told to the local child. The ceiling is enforced by the router; this keeps
# the model from pasting rows the filter then has to strip.
CHILD_RULES = {
    "title": " Answer with one line, no content.",
    "summary": " Answer in at most 120 words. Keep counts, column names and shapes; "
               "do not paste rows or raw values.",
    "none": " Answer normally; the full result is returned.",
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
    "disclosure": {"summary": 0, "local_only": 0, "title": 0},
    "pii_files": 0,
    "pii_hits": 0,
    "premium_blocked": 0,
    "hints": 0,
    "guard_retries": 0,
    "guard_synth": 0,
    "mode": DEFAULT_MODE,
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


def inject_child_rules(body: dict, disclosure: str) -> None:
    rule = CHILD_RULES.get(disclosure)
    if not rule:
        return
    messages = body.get("messages")
    if not isinstance(messages, list):
        return
    for message in messages:
        if message.get("role") != "system":
            continue
        if isinstance(message.get("content"), str):
            message["content"] += rule
        return


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


def take_disclosure(messages: list, fallback: str) -> tuple[str, list]:
    """Pull a `disclosure:` marker off the child prompt and strip it."""
    for index in range(len(messages) - 1, -1, -1):
        message = messages[index]
        if message.get("role") != "user":
            continue
        content = message.get("content")
        if not isinstance(content, str):
            return fallback, messages
        found = DISCLOSURE_RE.match(content)
        if not found:
            return fallback, messages
        updated = list(messages)
        updated[index] = {**message, "content": content[found.end():]}
        return found.group(1).lower(), updated
    return fallback, messages


def last_is_work(body: dict) -> bool:
    messages = [m for m in body.get("messages") or [] if m.get("role") != "system"]
    last = messages[-1] if messages else None
    return bool(last and last.get("role") == "user" and WORK_RE.search(str(last.get("content") or "")))


def shape_hybrid(body: dict, pii: dict | None = None) -> tuple[int, list, bool, bool]:
    """Remove the parent's file tools and force delegation on a first work turn.

    Returns the tools left on the request and whether `tool_choice` was pinned.
    """
    tools = body.get("tools")
    if not isinstance(tools, list):
        return 0, [], False, False
    names = [t.get("function", {}).get("name") for t in tools]
    kept = [t for t, name in zip(tools, names) if name not in PARENT_DENIED]
    removed = sorted(PARENT_DENIED.intersection(names))
    body["tools"] = kept

    force = False
    by_pii = bool(pii and pii.get("total"))
    trigger = "pii" if by_pii else ("work" if last_is_work(body) else "")
    pinned = isinstance(body.get("tool_choice"), dict)
    if FORCE_DELEGATION and "subagent" in names and not pinned:
        # Every turn where the user just spoke, not only the first: a long
        # session would otherwise never be pinned again.
        messages = [m for m in body.get("messages") or [] if m.get("role") != "system"]
        last = messages[-1] if messages else None
        if (
            last is not None
            and last.get("role") == "user"
            and (by_pii or WORK_RE.search(str(last.get("content") or "")))
        ):
            body["tool_choice"] = {"type": "function", "function": {"name": "subagent"}}
            force = True
    return len(kept), removed, force, trigger


def inject_delegation(body: dict, pii: dict | None = None) -> bool:
    """Append the hybrid instruction to the system message, once."""
    messages = body.get("messages")
    if not isinstance(messages, list):
        return False
    extra = DELEGATION
    for message in messages:
        if message.get("role") != "system":
            continue
        content = message.get("content")
        if isinstance(content, list):
            text = " ".join(p.get("text", "") for p in content if isinstance(p, dict))
            if DELEGATION.strip() in text:
                return False
            content.append({"type": "text", "text": extra})
            return True
        content = content or ""
        if DELEGATION.strip() in content:
            return False
        message["content"] = content + extra
        return True
    return False


def pii_instruction(pii: dict, mode: str) -> str:
    """The turn instruction that makes the cloud delegate a PII file.

    Names the file, what was found in it, and the mode the child must answer
    in, so neither the lane nor the disclosure is left to the model.
    """
    files = list(pii["files"])[:8]
    named = ", ".join(files)
    kinds = sorted({kind for counts in pii["files"].values() for kind in counts})
    plural = "files" if len(files) > 1 else "file"
    verb = "carry" if len(files) > 1 else "carries"
    return (
        f"Route this turn locally: {named} {verb} PII ({', '.join(kinds)}) and "
        f"must be processed locally. Call the `{PII_TOOL}` tool now for this "
        f"{plural}, and begin its prompt with this line:\n  disclosure: {mode}\n"
        f"Then state the task. Answer with the tool call only — a `read`, `grep` "
        f"or `glob` call here puts the file on the cloud."
    )


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


def child_prompt(body: dict, pii: dict, mode: str) -> str:
    """The prompt for a call the router had to emit itself."""
    files = list(pii["files"])[:8]
    task = ""
    for message in reversed(body.get("messages") or []):
        if message.get("role") != "user" or not isinstance(message.get("content"), str):
            continue
        text = message["content"].strip()
        if text.startswith(NUDGE_PREFIX) or text.startswith(INSTRUCTION_PREFIX):
            continue
        task = text
        break
    if not files:
        return f"disclosure: {mode}\n\n{task}"
    return (
        f"disclosure: {mode}\n\n"
        f"Read {', '.join(files)}. It carries PII, so work from the file itself.\n\n"
        f"Task: {task}"
    )


def patch_call_args(calls: dict[int, dict], body: dict, pii: dict, mode: str) -> bool:
    """Fill a subagent call the model left empty or without the mode marker.

    A pinned `tool_choice` makes the gateway strip arguments, so a call can
    arrive as `subagent {}`; dsh rejects that.
    """
    changed = False
    for call in calls.values():
        if call.get("name") != PII_TOOL:
            continue
        try:
            arguments = json.loads(call.get("arguments") or "{}")
        except Exception:
            arguments = {}
        if not isinstance(arguments, dict):
            arguments = {}
        prompt = str(arguments.get("prompt") or "")
        if not prompt:
            arguments["prompt"] = child_prompt(body, pii, mode)
            changed = True
        elif not DISCLOSURE_RE.match(prompt):
            arguments["prompt"] = f"disclosure: {mode}\n\n{prompt}"
            changed = True
        if not arguments.get("description"):
            arguments["description"] = "Read PII file locally"
            changed = True
        call["arguments"] = json.dumps(arguments)
    return changed


def calls_stream(calls: dict[int, dict]) -> Response:
    """Re-emit held calls as one SSE body, after a rewrite."""
    created = int(time.time())
    cid = "chatcmpl-pls-patch"

    async def stream():
        yield sse(chunk(cid, created, {"role": "assistant"}))
        for index, call in calls.items():
            yield sse(chunk(cid, created, {"tool_calls": [{
                "index": index,
                "id": f"call_pls_{index}",
                "type": "function",
                "function": {"name": call["name"], "arguments": call["arguments"]},
            }]}))
        yield sse(chunk(cid, created, {}, "tool_calls"))
        yield "data: [DONE]\n\n"

    return StreamingResponse(stream(), media_type="text/event-stream")


def synthetic_call(body: dict, pii: dict, mode: str) -> Response:
    """Emit the subagent call ourselves when the model will not.

    dsh executes whatever tool call the stream carries, so this is the only
    hard guarantee that a PII file never lands on the cloud.
    """
    calls = {0: {"name": PII_TOOL, "arguments": ""}}
    patch_call_args(calls, body, pii, mode)
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
    mode = data.get("mode")
    if mode not in ("cloud", "hybrid", "local"):
        mode = DEFAULT_MODE
    pii_mode = data.get("pii_mode")
    if pii_mode not in CHILD_RULES:
        pii_mode = PII_MODE
    try:
        max_turns = int(data.get("max_turns", DEFAULT_MAX_TURNS))
    except Exception:
        max_turns = DEFAULT_MAX_TURNS
    return {
        "mode": mode,
        "pii_mode": pii_mode,
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
                "guard_synth"):
        STATS[key] = 0
    STATS["disclosure"] = {"summary": 0, "local_only": 0, "title": 0}
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
    global waiting, model_port

    if path in ("v1/models", "local/v1/models", "local-only/v1/models"):
        return {"object": "list", "data": [{"id": MODEL_ID, "object": "model", "owned_by": "wally"}]}

    if TOKEN and not secrets.compare_digest(request.headers.get("x-pls-token", ""), TOKEN):
        return JSONResponse(
            {"error": {"message": "missing or wrong x-pls-token"}}, status_code=401
        )

    body: dict = {}
    if request.method == "POST":
        try:
            body = await request.json()
        except Exception:
            body = {}

    config = read_config()
    STATS["mode"] = config["mode"]

    disclosure = "summary"
    lane = "cloud"
    if path.startswith("local-only/"):
        lane, disclosure = "local", "local_only"
    elif path.startswith("local/"):
        lane = "local"

    if config["mode"] == "cloud":
        lane = "cloud"
    elif config["mode"] == "local":
        lane, disclosure = "local", "summary"

    upstream_path = path
    for prefix in ("local-only/", "local/"):
        if upstream_path.startswith(prefix):
            upstream_path = upstream_path[len(prefix):]
            break

    sub = await premium()
    reason = "path"
    if not sub:
        lane = "cloud"
        reason = "no-subscription"
        STATS["premium_blocked"] += 1
    elif config["mode"] == "cloud":
        reason = "mode"
    elif config["mode"] == "local":
        reason = "mode"

    messages = body.get("messages") or []
    pii = scan_pii(messages) if messages else {"files": {}, "total": 0}
    if pii["total"]:
        STATS["pii_files"] += len(pii["files"])
        STATS["pii_hits"] += pii["total"]

    tools = [t.get("function", {}).get("name") for t in (body.get("tools") or [])]
    log({"event": "route", "path": path, "lane": lane, "mode": config["mode"],
         "disclosure": disclosure, "reason": reason, "sub": sub,
         "pii": pii["total"], "pii_files": len(pii["files"]), "tools": tools,
         "messages": len(messages)})
    if lane == "cloud" and sub and config["mode"] == "hybrid" and len(messages) >= 4:
        if session_key(request, body) not in local_sessions:
            log({"event": "hint",
                 "message": "parent working alone: no subagent call yet, so nothing is local. "
                            "Ask it to use the subagent tool, or set mode local."})
            STATS["hints"] += 1

    work = last_is_work(body)
    work = last_is_work(body)
    guard = (
        sub
        and config["mode"] == "hybrid"
        and PII_TOOL in tools
        and (bool(pii["total"]) or work)
    )
    for file_path, counts in pii["files"].items():
        action = "summarise" if lane == "local" else ("delegated" if guard else "sent to cloud")
        log({"event": "pii_file", "file": file_path, "counts": counts,
             "lane": lane, "action": action})

    if lane == "cloud" and sub and config["mode"] == "hybrid":
        if inject_delegation(body, pii):
            log({"event": "injected", "what": "delegation prompt"})
        remaining, removed, forced, trigger = shape_hybrid(body, pii)
        log({"event": "shaped", "tools": remaining, "removed": removed,
             "forced_subagent": forced, "trigger": trigger})

    if guard and lane == "cloud":
        # PII: a turn of its own, so the file and the mode are stated rather
        # than inferred, and the reply is checked against them.
        if pii["total"]:
            body["messages"] = [*messages, {
                "role": "user",
                "content": pii_instruction(pii, config["pii_mode"]),
            }]
            log({"event": "instructed", "files": list(pii["files"])[:8],
                 "mode": config["pii_mode"]})

    if lane == "cloud":
        mode = config["pii_mode"] if pii["total"] else WORK_MODE
        if pii["total"]:
            # The transcript still carries the values it was scanned for.
            body["messages"] = redact_messages(body["messages"])
        return await cloud(upstream_path, body, request, pii, sub,
                           guard=guard, mode=mode)
    return await local(upstream_path, body, request, config, disclosure)


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


def synthetic_json(body: dict, pii: dict, mode: str) -> JSONResponse:
    """`synthetic_call` for a client that asked for a single JSON answer."""
    calls = {0: {"name": PII_TOOL, "arguments": ""}}
    patch_call_args(calls, body, pii, mode)
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


async def cloud(path: str, body: dict, request: Request,
                pii: dict | None = None, sub: bool = True,
                guard: bool = False, mode: str = PII_MODE, attempt: int = 0):
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
    log({"event": "cloud", "path": path, "status": response.status_code,
         "stream": stream, "content_type": response.headers.get("content-type"),
         "keys": sorted(body)})

    # An error, or a gateway that answered with JSON despite `stream`, is not
    # an event stream. Relaying it as one makes dsh report a transport error
    # ("Stream ended without finish_reason") instead of the real status.
    content_type = response.headers.get("content-type", "")
    event_stream = response.status_code < 400 and "text/event-stream" in content_type

    async def settle(calls: dict[int, dict], raw: bytes | None):
        """Accept, patch, re-ask or synthesise, for either body shape."""
        names = {call["name"] for call in calls.values()}
        # A PII turn accepts the subagent call alone. A work turn also accepts
        # bash/edit/write, but never a file tool: dsh would run it up here.
        if pii and pii.get("total"):
            clean = names == {PII_TOOL}
        else:
            clean = bool(names) and not (names & PARENT_DENIED)
        if clean:
            if patch_call_args(calls, body, pii or {"files": {}}, mode):
                log({"event": "guard", "outcome": "patched", "attempt": attempt})
                if raw is not None:
                    return calls_stream(calls)
                return synthetic_json(body, pii or {"files": {}}, mode)
            log({"event": "guard", "outcome": "delegated", "attempt": attempt})
            if raw is not None:
                return Response(raw, media_type="text/event-stream")
            return None

        STATS["guard_retries"] += 1
        log({"event": "guard", "outcome": "rejected", "attempt": attempt,
             "saw": sorted(names)})
        if attempt >= GUARD_RETRIES:
            STATS["guard_synth"] += 1
            log({"event": "guard", "outcome": "synthesised", "saw": sorted(names)})
            if raw is not None:
                return synthetic_call(body, pii or {"files": {}}, mode)
            return synthetic_json(body, pii or {"files": {}}, mode)

        saw = ", ".join(sorted(names)) or "no tool call"
        if pii and pii.get("total"):
            wrong = (f"Wrong: you answered with {saw}. Call the `{PII_TOOL}` tool "
                     f"now, nothing else, and begin its prompt with "
                     f"`disclosure: {mode}`.")
        else:
            wrong = (f"Wrong: you answered with {saw}. The file work belongs to "
                     f"the `{PII_TOOL}` tool — call it now and begin its prompt "
                     f"with `disclosure: {mode}`.")
        nudged = {**body, "messages": [*body.get("messages", []), {
            "role": "user", "content": wrong,
        }]}
        return await cloud(path, nudged, request, pii, sub,
                           guard=True, mode=mode, attempt=attempt + 1)

    if not event_stream:
        payload = await response.aread()
        await response.aclose()
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

    buffered = b""
    if guard:
        async for piece in response.aiter_bytes():
            buffered += piece
        await response.aclose()
        if config_cloud_model:
            buffered = buffered.replace(
                f'"model":"{config_cloud_model}"'.encode(), f'"model":"{MODEL_ID}"'.encode()
            )
        settled = await settle(parse_calls(buffered), buffered)
        return settled if settled is not None else Response(
            buffered, media_type="text/event-stream"
        )

    async def relay():
        # Raw bytes, not lines: a provider may split one `data:` event across
        # several lines, and re-framing per line would cut it in half and lose
        # the finish chunk.
        seen_finish = False
        try:
            # aiter_bytes, not aiter_raw: raw yields still-compressed bytes when
            # the gateway gzips, and we do not forward its content-encoding.
            async for piece in response.aiter_bytes():
                if b"finish_reason" in piece and b"finish_reason\":null" not in piece:
                    seen_finish = True
                if config_cloud_model:
                    piece = piece.replace(
                        f'"model":"{config_cloud_model}"'.encode(),
                        f'"model":"{MODEL_ID}"'.encode(),
                    )
                yield piece
            if not seen_finish:
                log({"event": "cloud_no_finish", "path": path})
        finally:
            await response.aclose()

    return StreamingResponse(relay(), media_type="text/event-stream")


async def local(path: str, body: dict, request: Request, config: dict, disclosure: str):
    global waiting

    overflow = False
    queued = False
    if local_sem.locked():
        if waiting >= QUEUE_MAX:
            overflow = True
        else:
            waiting += 1
            queued = True
            STATS["queued"] += 1
    # A withheld answer must not be answered by the cloud, or the file reaches
    # it after all; that request waits for the slot instead of overflowing.
    if overflow and disclosure == "local_only":
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
            return await local_inner(path, body, request, config, disclosure)
    finally:
        # Cancelled while still queued: nothing else gives the slot back.
        if queued:
            waiting = max(0, waiting - 1)


async def local_inner(path: str, body: dict, request: Request, config: dict, disclosure: str):
    key = session_key(request, body)
    local_sessions.add(key)
    # A key can serve several child runs; a fresh run starts with a shorter
    # transcript than the previous one ended with, so the cap restarts there.
    depth = len(body.get("messages") or [])
    count, last_depth = turns.get(key, (0, 0))
    count = count + 1 if depth > last_depth else 1
    turns[key] = (count, depth)
    STATS["local_turns"] += 1

    messages = list(body.get("messages") or [])
    disclosure, messages = take_disclosure(messages, disclosure)
    body = {**body, "messages": messages}
    inject_child_rules(body, disclosure)
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

    # Disclosure only applies to a final answer; a tool call must pass through
    # intact or the child cannot continue.
    if not calls and message.get("content"):
        if disclosure == "local_only":
            message["content"] = WITHHELD
            STATS["disclosure"]["local_only"] += 1
        elif disclosure == "title":
            first = message["content"].strip().splitlines()[0]
            message["content"] = short_paths(redact(" ".join(first.split()[:12])))
            STATS["disclosure"]["title"] += 1
        elif disclosure == "summary":
            # A short answer summarises into something vaguer than itself.
            if len(message["content"]) > 400:
                message["content"] = await summarise(message["content"])
            message["content"] = short_paths(redact(message["content"]))
            STATS["disclosure"]["summary"] += 1
        choice["message"] = message

    log({"event": "local_done", "turns": count, "capped": capped,
         "disclosure": disclosure, "prompt_tokens": usage.get("prompt_tokens"),
         "completion_tokens": usage.get("completion_tokens"),
         "calls": len(calls)})
    STATS["requests"] += 1
    STATS["last"] = {
        "lane": "local", "path": path, "disclosure": disclosure,
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
        CONFIG_PATH.write_text(json.dumps({"mode": DEFAULT_MODE, "max_turns": DEFAULT_MAX_TURNS}))
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
