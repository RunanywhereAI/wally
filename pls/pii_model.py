"""Spans from the local PII model.

The weights are ``perplexity-ai/pplx-pii-masking-MLX``. A request returns
``{"spans": [{"start", "end", "category"}], "sensitivity": float}``. This
module returns the spans and ignores ``sensitivity``. The model is loaded on
the first call. Importing this module does not load it.
"""

from __future__ import annotations

import asyncio
import os
import threading
from pathlib import Path

WINDOW_OVERLAP_TOKENS = 256
SEGMENT_CHARS = 200_000
SEGMENT_OVERLAP_CHARS = 1_000

_lock = threading.Lock()
_runtime: tuple | None = None


def model_dir() -> Path:
    """The installed catalog snapshot. Wally sets ``PLS_PII_MODEL`` to it.

    There is no dev-folder fallback. A missing directory fails the mask.
    """
    override = os.environ.get("PLS_PII_MODEL")
    if not override:
        raise FileNotFoundError("PLS_PII_MODEL is unset; pull pplx-pii")
    path = Path(override)
    if not (path / "weights.safetensors").is_file():
        raise FileNotFoundError(f"PII weights are not at {path / 'weights.safetensors'}")
    return path


def _load():
    global _runtime
    if _runtime is not None:
        return _runtime
    with _lock:
        if _runtime is not None:
            return _runtime
        snapshot = model_dir()
        import mlx.core as mx
        import pii_viterbi
        import pii_weights
        from transformers import AutoTokenizer

        mx.set_cache_limit(0)
        model, cfg = pii_weights.load_model(snapshot)
        tokenizer = AutoTokenizer.from_pretrained(snapshot, trust_remote_code=False)
        _runtime = (model, tokenizer, cfg, pii_viterbi, mx)
        return _runtime


def _merge_spans(spans: list) -> list:
    merged = []
    for span in sorted(spans, key=lambda item: (item[0], item[1], item[2])):
        if merged:
            last = merged[-1]
            if span[0] < last[1]:
                cat = last[2] if last[3] >= span[3] else span[2]
                merged[-1] = (last[0], max(last[1], span[1]), cat, max(last[3], span[3]))
                continue
        merged.append(span)
    return sorted(merged, key=lambda item: (item[0], item[1]))


def _classify_window(seg_text, input_ids, attention_mask, offset_mapping, model, cfg, viterbi, mx):
    import numpy as np

    token_logits, sensitivity_logit = model(mx.array([input_ids]), mx.array([attention_mask]))
    mx.eval(token_logits, sensitivity_logit)
    logits = np.array(token_logits.astype(mx.float32))[0]
    spans = viterbi.decode(
        logits,
        offset_mapping,
        seg_text,
        b_bias=float(cfg.get("viterbi_b_bias", 0.0)),
        e_bias=float(cfg.get("viterbi_e_bias", 0.0)),
        num_labels=int(cfg.get("num_token_labels", 37)),
    )
    logit = float(np.array(sensitivity_logit.astype(mx.float32))[0, 0])
    sensitivity = 1.0 / (1.0 + float(np.exp(-logit)))
    return spans, sensitivity


def _classify_sync(text: str) -> list[dict]:
    model, tokenizer, cfg, viterbi, mx = _load()
    try:
        return _classify_loaded(text, model, tokenizer, cfg, viterbi, mx)
    finally:
        mx.clear_cache()


def _classify_loaded(text, model, tokenizer, cfg, viterbi, mx) -> list[dict]:
    window_len = int(cfg.get("max_seq_len", 4096))
    window_stride = max(window_len - WINDOW_OVERLAP_TOKENS, 1)
    all_spans = []
    seg_start = 0
    while seg_start < len(text):
        seg_text = text[seg_start : seg_start + SEGMENT_CHARS]
        encoded = tokenizer(
            seg_text,
            return_offsets_mapping=True,
            truncation=False,
            padding=False,
        )
        input_ids = encoded["input_ids"]
        attention_mask = encoded["attention_mask"]
        offset_mapping = encoded["offset_mapping"]
        w_start = 0
        while w_start < len(input_ids):
            w_end = min(w_start + window_len, len(input_ids))
            spans, _sensitivity = _classify_window(
                seg_text,
                input_ids[w_start:w_end],
                attention_mask[w_start:w_end],
                offset_mapping[w_start:w_end],
                model,
                cfg,
                viterbi,
                mx,
            )
            all_spans.extend(
                (start + seg_start, end + seg_start, label, score)
                for start, end, label, score in spans
            )
            if w_end == len(input_ids):
                break
            w_start += window_stride
        if seg_start + SEGMENT_CHARS >= len(text):
            break
        seg_start += SEGMENT_CHARS - SEGMENT_OVERLAP_CHARS
    merged = _merge_spans(all_spans)
    return [{"start": start, "end": end, "category": label} for start, end, label, _score in merged]


async def classify(text: str) -> list[dict]:
    """Spans from the local PII model. Tests replace pls.classify_pii.

    On any exception, a non-list return, or a batch that makes mask()
    raise MaskError, the router treats the call as failure.
    """
    return await asyncio.to_thread(_classify_sync, text)
