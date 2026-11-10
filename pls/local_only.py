"""Load the PLS local model with the same window and prefix cache, then answer once.

Uses PLS_MAX_KV, PLS_MAX_OUT and the APC block settings from model_server.py.
The model path comes from PLS_MODEL or ~/.config/wally/pls/config.json.
"""

import json
import os
import time
from pathlib import Path

import mlx.core as mx
import mlx_vlm
from mlx_vlm.apc import APCManager

MAX_KV = int(os.environ.get("PLS_MAX_KV", "65536"))
MAX_OUT = int(os.environ.get("PLS_MAX_OUT", "8192"))
APC_BLOCKS = int(os.environ.get("PLS_APC_BLOCKS", "8192"))
APC_CHECKPOINTS = int(os.environ.get("PLS_APC_CHECKPOINTS", "32"))
APC_CHECKPOINT_INTERVAL = int(os.environ.get("PLS_APC_CHECKPOINT_INTERVAL", "4096"))


def model_path() -> str:
    if os.environ.get("PLS_MODEL"):
        return os.environ["PLS_MODEL"]
    config = Path.home() / ".config" / "wally" / "pls" / "config.json"
    return json.loads(config.read_text())["model_path"]


def gb(n: int) -> str:
    return f"{n / 1024**3:.2f} GB"


def main() -> None:
    path = model_path()
    print(f"model {path}", flush=True)
    print(f"max_kv {MAX_KV} max_out {MAX_OUT} apc_blocks {APC_BLOCKS} "
          f"checkpoints {APC_CHECKPOINTS} interval {APC_CHECKPOINT_INTERVAL}", flush=True)
    mx.set_cache_limit(0)
    started = time.monotonic()
    model, processor = mlx_vlm.load(path)
    apc = APCManager(num_blocks=APC_BLOCKS, overrides={
        "checkpoint_entries": APC_CHECKPOINTS,
        "checkpoint_interval_tokens": APC_CHECKPOINT_INTERVAL,
    })
    print(f"loaded in {time.monotonic() - started:.1f}s  mlx {gb(mx.get_active_memory())}", flush=True)

    prompt = os.environ.get("PLS_PROMPT", "Reply with one word: ok")
    messages = [{"role": "user", "content": prompt}]
    text = mlx_vlm.apply_chat_template(
        processor, model.config, messages, add_generation_prompt=True,
    )
    tokenizer = getattr(processor, "tokenizer", processor)
    prompt_tokens = len(tokenizer.encode(text))
    if prompt_tokens >= MAX_KV:
        raise SystemExit(f"prompt is {prompt_tokens} tokens; window is {MAX_KV}")
    started = time.monotonic()
    out = mlx_vlm.generate(
        model, processor, text,
        max_tokens=min(64, MAX_OUT),
        temperature=0.0,
        verbose=False,
        apc_manager=apc,
    )
    answer = out.text if hasattr(out, "text") else str(out)
    cached = getattr(out, "cached_tokens", 0) or 0
    print(f"prompt_tokens {prompt_tokens} cached {cached} "
          f"gen {time.monotonic() - started:.1f}s  mlx {gb(mx.get_active_memory())}", flush=True)
    print(answer.strip(), flush=True)


if __name__ == "__main__":
    main()
