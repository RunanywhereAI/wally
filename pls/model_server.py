"""OpenAI-compatible server over mlx-vlm, spawned and owned by pls.py.

Text only. Tool calls are parsed from the Qwen XML form the chat template
emits. The context cap is enforced by trimming the prompt cache, because
`max_kv_size` is a no-op on models whose `make_cache()` takes no size.

Binds port 0 and prints `{"port": N}` on stdout once listening, so the router
never has to guess a port.
"""

import asyncio
import json
import os
import re
import socket
import sys
import time
import uuid
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
from typing import Any

import mlx.core as mx
import mlx_vlm
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, StreamingResponse
import uvicorn

MODEL_PATH = os.environ["PLS_MODEL"]
MODEL_ID = os.environ.get("PLS_MODEL_ID", "Hybird-Wally-0.1")
MAX_KV = int(os.environ.get("PLS_MAX_KV", "57344"))
MAX_OUT = int(os.environ.get("PLS_MAX_OUT", "8192"))
PROMPT_CACHE_MAX = int(os.environ.get("PLS_PROMPT_CACHE_MAX", "8"))

app = FastAPI()
model = None
processor = None

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
    "trims": 0,
    "last": None,
}

PROMPT_CACHES: "OrderedDict[str, list]" = OrderedDict()

TOOL_RE = re.compile(r"<tool_call>\s*<function=([^>\n]+)>(.*?)</function>\s*</tool_call>", re.S)
PARAM_RE = re.compile(r"<parameter=([^>\n]+)>\n?(.*?)\n?</parameter>", re.S)


def log(event: dict) -> None:
    print(json.dumps(event), file=sys.stderr, flush=True)


def cache_key(request: Request) -> str:
    for name in ("x-session-affinity", "x-session-id", "x-opencode-session-id"):
        value = request.headers.get(name)
        if value:
            return value
    return "default"


def trim_cache(cache) -> None:
    """Keep the live KV under the cap by dropping the oldest half."""
    try:
        used = max((c.size() for c in cache if hasattr(c, "size")), default=0)
    except Exception:
        return
    if used <= MAX_KV:
        return
    keep = MAX_KV // 2
    for c in cache:
        trimmable = getattr(c, "is_trimmable", None)
        if not hasattr(c, "trim") or (trimmable is not None and not trimmable()):
            continue
        try:
            c.trim(c.size() - keep)
        except Exception:
            pass
    STATS["trims"] += 1


def take_cache(key: str):
    cache = PROMPT_CACHES.get(key)
    if cache is not None:
        PROMPT_CACHES.move_to_end(key)
    return cache


def put_cache(key: str, cache) -> None:
    PROMPT_CACHES[key] = cache
    PROMPT_CACHES.move_to_end(key)
    while len(PROMPT_CACHES) > PROMPT_CACHE_MAX:
        PROMPT_CACHES.popitem(last=False)


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
    s["prompt_caches"] = len(PROMPT_CACHES)
    return s


@app.post("/v1/chat/completions")
async def chat(request: Request):
    body: dict[str, Any] = await request.json()
    messages = body.get("messages") or []
    tools = body.get("tools")
    stream = bool(body.get("stream"))
    want_usage = bool((body.get("stream_options") or {}).get("include_usage"))
    max_tokens = min(int(body.get("max_tokens") or MAX_OUT), MAX_OUT)
    temperature = float(body.get("temperature", 0.7))

    prompt = build_prompt(messages, tools)
    kwargs = dict(
        max_tokens=max_tokens,
        temperature=temperature,
        verbose=False,
    )

    key = cache_key(request)
    cache = take_cache(key)
    if cache is None:
        try:
            cache = model.language_model.make_cache()
        except AttributeError:
            from mlx_vlm.models.cache import make_prompt_cache

            cache = make_prompt_cache(model.language_model)
        put_cache(key, cache)
    trim_cache(cache)
    kwargs["prompt_cache"] = cache

    loop = asyncio.get_running_loop()
    cid = "chatcmpl-" + uuid.uuid4().hex[:24]
    created = int(time.time())
    started = time.monotonic()
    STATS["in_flight"] += 1

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

            first = {
                "id": cid,
                "object": "chat.completion.chunk",
                "created": created,
                "model": MODEL_ID,
                "choices": [{"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}],
            }
            yield f"data: {json.dumps(first)}\n\n"

            it = await loop.run_in_executor(
                POOL, lambda: mlx_vlm.stream_generate(model, processor, prompt, **kwargs)
            )
            for chunk in it:
                last = chunk
                piece = chunk.text if hasattr(chunk, "text") else str(chunk)
                pt = getattr(chunk, "prompt_tokens", pt) or pt
                ct = getattr(chunk, "generation_tokens", ct) or ct
                if not piece:
                    continue
                if not ttft:
                    ttft = (time.monotonic() - started) * 1000
                acc += piece

                if in_call:
                    held += piece
                    for out in flush_held():
                        yield f"data: {json.dumps(out)}\n\n"
                    if "</tool_call>" in held or not held.lstrip().startswith("<tool_call>"):
                        in_call = held.lstrip().startswith("<tool_call>")
                    continue

                if "<tool_call>" in piece:
                    head, _, tail = piece.partition("<tool_call>")
                    if head:
                        yield f"data: {json.dumps(content_delta(head))}\n\n"
                    in_call = True
                    held = "<tool_call>" + tail
                    for out in flush_held():
                        yield f"data: {json.dumps(out)}\n\n"
                    if "</tool_call>" in held:
                        in_call = False
                    continue

                yield f"data: {json.dumps(content_delta(piece))}\n\n"

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

        return StreamingResponse(sse(), media_type="text/event-stream")

    except Exception as error:
        STATS["errors"] += 1
        log({"event": "error", "id": cid, "error": f"{type(error).__name__}: {error}"})
        raise
    finally:
        STATS["in_flight"] -= 1


@app.on_event("startup")
async def load():
    global model, processor
    loop = asyncio.get_running_loop()

    def _load():
        mx.set_cache_limit(0)
        return mlx_vlm.load(MODEL_PATH)

    model, processor = await loop.run_in_executor(POOL, _load)
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
