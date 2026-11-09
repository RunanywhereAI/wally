"""OpenAI-compatible server over mlx-vlm, spawned and owned by pls.py.

Text only. Tool calls are parsed from the Qwen XML form the chat template
emits. KV reuse goes through mlx-vlm's automatic prefix cache (APC), which
matches on token hashes across requests, so a new session that shares a
harness's system prompt and tools skips their prefill. A raw `prompt_cache`
is not reused by mlx-vlm and would re-prefill on top of the old KV. A prompt
whose tokens plus the reply would pass `PLS_MAX_KV` is refused rather than
truncated.

Binds port 0 and prints `{"port": N}` on stdout once listening, so the router
never has to guess a port.
"""

import asyncio
import json
import os
import re
import socket
import sys
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from typing import Any

import mlx.core as mx
import mlx_vlm
from mlx_vlm.apc import APCManager
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
import uvicorn

MODEL_PATH = os.environ["PLS_MODEL"]
MODEL_ID = os.environ.get("PLS_MODEL_ID", "Hybrid-Wally-0.1-local")
# 64K: Hermes refuses a delegation model with a window under 64,000.
MAX_KV = int(os.environ.get("PLS_MAX_KV", "65536"))
MAX_OUT = int(os.environ.get("PLS_MAX_OUT", "8192"))
# APC's defaults hold 2048 x 16 = 32k tokens and two checkpoints, taken every
# 2048 tokens. Hybrid-attention models (the PPLX 35B) reuse only through
# checkpoints, and one 12k subagent context alone takes six of them, so a
# second subagent evicted the first and every turn re-prefilled past the
# shared system prompt. Fewer, wider checkpoints and a higher cap keep several
# live contexts.
APC_BLOCKS = int(os.environ.get("PLS_APC_BLOCKS", "8192"))
APC_CHECKPOINTS = int(os.environ.get("PLS_APC_CHECKPOINTS", "32"))
APC_CHECKPOINT_INTERVAL = int(os.environ.get("PLS_APC_CHECKPOINT_INTERVAL", "4096"))

app = FastAPI()
model = None
processor = None
apc: APCManager | None = None

# MLX streams are thread-local: load and generate on one dedicated thread, which
# also serialises requests.
POOL = ThreadPoolExecutor(max_workers=1)

STATS: dict[str, Any] = {
    "requests": 0,
    "errors": 0,
    "in_flight": 0,
    "prompt_tokens": 0,
    "completion_tokens": 0,
    "peak_context": 0,
    "max_kv": MAX_KV,
    "cached_tokens": 0,
    "context_refusals": 0,
    "last": None,
}

TOOL_RE = re.compile(r"<tool_call>\s*<function=([^>\n]+)>(.*?)</function>\s*</tool_call>", re.S)
PARAM_RE = re.compile(r"<parameter=([^>\n]+)>\n?(.*?)\n?</parameter>", re.S)


def log(event: dict) -> None:
    print(json.dumps(event), file=sys.stderr, flush=True)


def reply_budget(prompt_tokens: int, max_tokens: int, cap: int) -> int:
    """Tokens the reply may use so prompt plus reply stays within `cap`.

    Zero means the prompt alone leaves no room and the request is refused.
    """
    return max(0, min(max_tokens, cap - prompt_tokens))


def count_tokens(prompt: str) -> int:
    tokenizer = getattr(processor, "tokenizer", processor)
    return len(tokenizer.encode(prompt))


def parse_tool_calls(text: str):
    calls = []
    for name, body in TOOL_RE.findall(text):
        args = {k: v for k, v in PARAM_RE.findall(body)}
        calls.append(
            {
                "id": "call_" + uuid.uuid4().hex[:24],
                "type": "function",
                "function": {"name": name.strip(), "arguments": json.dumps(args)},
            }
        )
    return calls


def strip_tool_calls(text: str) -> str:
    return TOOL_RE.sub("", text).strip()


def build_prompt(messages, tools):
    return mlx_vlm.apply_chat_template(
        processor,
        model.config,
        messages,
        tools=tools or None,
        add_generation_prompt=True,
    )


def usage_of(result, prompt_tokens: int, completion_tokens: int) -> dict:
    pt = getattr(result, "prompt_tokens", 0) or prompt_tokens
    ct = getattr(result, "generation_tokens", 0) or completion_tokens
    return {
        "prompt_tokens": pt,
        "completion_tokens": ct,
        "total_tokens": pt + ct,
        "context_used": pt + ct,
        "context_max": MAX_KV,
        "cached_tokens": getattr(result, "cached_tokens", 0) or 0,
        "prompt_tps": round(getattr(result, "prompt_tps", 0.0) or 0.0, 2),
        "generation_tps": round(getattr(result, "generation_tps", 0.0) or 0.0, 2),
        "peak_memory_mb": round((getattr(result, "peak_memory", 0) or 0) / 1e6, 1),
    }


def record(req_id: str, usage: dict, ttft_ms: float, total_ms: float, finish: str) -> None:
    STATS["requests"] += 1
    STATS["prompt_tokens"] += usage["prompt_tokens"]
    STATS["completion_tokens"] += usage["completion_tokens"]
    STATS["cached_tokens"] += usage["cached_tokens"]
    STATS["peak_context"] = max(STATS["peak_context"], usage["context_used"])
    entry = {
        "event": "request",
        "id": req_id,
        "ttft_ms": round(ttft_ms, 1),
        "total_ms": round(total_ms, 1),
        "finish": finish,
        **usage,
    }
    STATS["last"] = entry
    log(entry)


@app.get("/v1/models")
def models():
    return {"object": "list", "data": [{"id": MODEL_ID, "object": "model", "owned_by": "local"}]}


@app.get("/health")
def health():
    return {"ok": model is not None}


@app.get("/stats")
def stats():
    s = dict(STATS)
    s["context_headroom"] = MAX_KV - s["peak_context"]
    return s


@app.post("/v1/chat/completions")
async def chat(request: Request):
    body: dict[str, Any] = await request.json()
    messages = body.get("messages") or []
    tools = body.get("tools")
    stream = bool(body.get("stream"))
    want_usage = bool((body.get("stream_options") or {}).get("include_usage"))
    # opencode 2 sends `max_completion_tokens`; the rest send `max_tokens`.
    asked = body.get("max_completion_tokens") or body.get("max_tokens")
    max_tokens = min(int(asked or MAX_OUT), MAX_OUT)
    temperature = float(body.get("temperature", 0.7))

    prompt = build_prompt(messages, tools)
    kwargs = dict(
        max_tokens=max_tokens,
        temperature=temperature,
        verbose=False,
    )

    prompt_tokens = count_tokens(prompt)
    budget = reply_budget(prompt_tokens, max_tokens, MAX_KV)
    if budget == 0:
        STATS["context_refusals"] += 1
        log({"event": "context_refused", "prompt_tokens": prompt_tokens, "max_kv": MAX_KV})
        return JSONResponse(
            {"error": {
                "message": f"prompt is {prompt_tokens} tokens; this model's context is {MAX_KV}",
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            }},
            status_code=400,
        )
    kwargs["max_tokens"] = budget
    kwargs["apc_manager"] = apc

    loop = asyncio.get_running_loop()
    cid = "chatcmpl-" + uuid.uuid4().hex[:24]
    created = int(time.time())
    started = time.monotonic()
    STATS["in_flight"] += 1
    streaming = False

    try:
        if not stream:
            out = await loop.run_in_executor(
                POOL, lambda: mlx_vlm.generate(model, processor, prompt, **kwargs)
            )
            elapsed = (time.monotonic() - started) * 1000
            text = out.text if hasattr(out, "text") else str(out)
            calls = parse_tool_calls(text)
            usage = usage_of(out, 0, 0)
            record(cid, usage, elapsed, elapsed, "tool_calls" if calls else "stop")
            msg: dict[str, Any] = {
                "role": "assistant",
                "content": strip_tool_calls(text) or None,
            }
            if calls:
                msg["tool_calls"] = calls
            return JSONResponse(
                {
                    "id": cid,
                    "object": "chat.completion",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [
                        {
                            "index": 0,
                            "message": msg,
                            "finish_reason": "tool_calls" if calls else "stop",
                        }
                    ],
                    "usage": usage,
                }
            )

        async def sse():
            acc = ""
            held = ""
            pending = ""
            in_call = False
            call_index = 0
            pt = ct = 0
            ttft = 0.0
            last = None

            def content_delta(text):
                return {
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [
                        {"index": 0, "delta": {"content": text}, "finish_reason": None}
                    ],
                }

            def call_delta(calls):
                nonlocal call_index
                payload = {
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "tool_calls": [
                                    {"index": call_index + i, **c}
                                    for i, c in enumerate(calls)
                                ]
                            },
                            "finish_reason": None,
                        }
                    ],
                }
                call_index += len(calls)
                return payload

            def flush_held():
                nonlocal held
                while "</tool_call>" in held:
                    end = held.index("</tool_call>") + len("</tool_call>")
                    block, held = held[:end], held[end:]
                    calls = parse_tool_calls(block)
                    if calls:
                        yield call_delta(calls)
                    else:
                        yield content_delta(block)

            def leave_call():
                """A call just closed: keep text after it, resume streaming."""
                nonlocal held, pending, in_call
                if held and not held.lstrip().startswith("<tool_call>"):
                    pending = held
                    held = ""
                    in_call = False
                else:
                    in_call = bool(held)

            first = {
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": MODEL_ID,
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}],
            }
            stop = threading.Event()
            try:
                yield f"data: {json.dumps(first)}\n\n"

                # Generation runs on the pool thread; it must not be iterated
                # from here or every chunk waits on the event loop.
                queue: asyncio.Queue = asyncio.Queue(maxsize=8)
                done = object()

                def offer(item) -> None:
                    pending = asyncio.run_coroutine_threadsafe(queue.put(item), loop)
                    while not stop.is_set():
                        try:
                            pending.result(timeout=0.5)
                            return
                        except TimeoutError:
                            continue
                        except Exception:
                            return
                    pending.cancel()

                def pump():
                    try:
                        for item in mlx_vlm.stream_generate(model, processor, prompt, **kwargs):
                            if stop.is_set():
                                return
                            offer(item)
                    except BaseException as error:  # noqa: BLE001 - relayed below
                        offer(error)
                    finally:
                        offer(done)

                loop.run_in_executor(POOL, pump)

                while True:
                    chunk = await queue.get()
                    if chunk is done:
                        break
                    if isinstance(chunk, BaseException):
                        raise chunk
                    last = chunk
                    piece = chunk.text if hasattr(chunk, "text") else str(chunk)
                    pt = getattr(chunk, "prompt_tokens", pt) or pt
                    ct = getattr(chunk, "generation_tokens", ct) or ct
                    if not piece:
                        continue
                    if not ttft:
                        ttft = (time.monotonic() - started) * 1000
                    acc += piece
                    pending += piece

                    if in_call:
                        held += pending
                        pending = ""
                        for out in flush_held():
                            yield f"data: {json.dumps(out)}\n\n"
                        leave_call()
                        continue

                    # The opener can be split across chunks, so a tail the
                    # length of it is always held back before flushing text.
                    cut = pending.rfind("<tool_call>")
                    if cut >= 0:
                        head, tail = pending[:cut], pending[cut:]
                        if head:
                            yield f"data: {json.dumps(content_delta(head))}\n\n"
                        held = tail
                        pending = ""
                        in_call = True
                        for out in flush_held():
                            yield f"data: {json.dumps(out)}\n\n"
                        leave_call()
                        continue
                    keep = len("<tool_call>") - 1
                    if len(pending) > keep:
                        emit = pending[:-keep]
                        pending = pending[-keep:]
                        yield f"data: {json.dumps(content_delta(emit))}\n\n"

                if pending:
                    if in_call:
                        held += pending
                    else:
                        yield f"data: {json.dumps(content_delta(pending))}\n\n"
                    pending = ""
                if held:
                    for out in flush_held():
                        yield f"data: {json.dumps(out)}\n\n"
                    if held:
                        yield f"data: {json.dumps(content_delta(held))}\n\n"

                calls = parse_tool_calls(acc)
                usage = usage_of(last, pt, ct)
                record(cid, usage, ttft, (time.monotonic() - started) * 1000,
                       "tool_calls" if calls else "stop")
                final = {
                    "id": cid,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": MODEL_ID,
                    "choices": [
                        {"index": 0, "delta": {}, "finish_reason": "tool_calls" if calls else "stop"}
                    ],
                }
                yield f"data: {json.dumps(final)}\n\n"
                if want_usage:
                    yield f"data: {json.dumps({'id': cid, 'object': 'chat.completion.chunk', 'model': MODEL_ID, 'choices': [], 'usage': usage})}\n\n"
                yield "data: [DONE]\n\n"
            except Exception as error:
                STATS["errors"] += 1
                log({"event": "error", "id": cid, "stream": True,
                     "error": f"{type(error).__name__}: {error}"})
                raise
            finally:
                stop.set()
                STATS["in_flight"] -= 1

        streaming = True
        return StreamingResponse(sse(), media_type="text/event-stream")

    except Exception as error:
        STATS["errors"] += 1
        log({"event": "error", "id": cid, "error": f"{type(error).__name__}: {error}"})
        raise
    finally:
        if not streaming:
            STATS["in_flight"] -= 1


@app.on_event("startup")
async def load():
    global model, processor, apc
    loop = asyncio.get_running_loop()

    def _load():
        mx.set_cache_limit(0)
        return mlx_vlm.load(MODEL_PATH)

    model, processor = await loop.run_in_executor(POOL, _load)
    apc = APCManager(num_blocks=APC_BLOCKS, overrides={
        "checkpoint_entries": APC_CHECKPOINTS,
        "checkpoint_interval_tokens": APC_CHECKPOINT_INTERVAL,
    })
    log({"event": "apc", "blocks": APC_BLOCKS, "tokens": APC_BLOCKS * apc.block_size,
         "checkpoints": APC_CHECKPOINTS, "checkpoint_interval": APC_CHECKPOINT_INTERVAL})
    log({"event": "loaded", "model": MODEL_ID, "max_kv": MAX_KV, "max_out": MAX_OUT})


def main() -> None:
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", 0))
    sock.listen(128)
    port = sock.getsockname()[1]
    print(json.dumps({"port": port}), flush=True)
    uvicorn.Server(
        uvicorn.Config(app, log_level="warning", lifespan="on")
    ).run(sockets=[sock])


if __name__ == "__main__":
    main()
