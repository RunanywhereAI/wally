"""Bidirectional Qwen3 backbone in MLX.

Vendored from github.com/ppl-ai/pplx-pii-masking, branch
chuan-wang-perplexity/pii-model-quantization (PR #2), mlx_pii/backbone.py.
mlx-only; no torch.

Local deviation: the pad mask uses the dtype's minimum finite value instead
of a fixed -1e9. In float16 that constant overflows to -inf, and 0 * -inf
is NaN for every valid token.
"""
from __future__ import annotations

import mlx.core as mx
import mlx.nn as nn
from mlx_lm.models.qwen3 import ModelArgs, TransformerBlock


def build_model_args(backbone_cfg: dict) -> ModelArgs:
    rope_theta = backbone_cfg.get("rope_theta") or backbone_cfg.get(
        "rope_parameters", {}
    ).get("rope_theta", 1000000.0)
    return ModelArgs(
        model_type="qwen3",
        hidden_size=backbone_cfg["hidden_size"],
        num_hidden_layers=backbone_cfg["num_hidden_layers"],
        intermediate_size=backbone_cfg["intermediate_size"],
        num_attention_heads=backbone_cfg["num_attention_heads"],
        rms_norm_eps=backbone_cfg["rms_norm_eps"],
        vocab_size=backbone_cfg["vocab_size"],
        num_key_value_heads=backbone_cfg["num_key_value_heads"],
        max_position_embeddings=backbone_cfg["max_position_embeddings"],
        rope_theta=float(rope_theta),
        head_dim=backbone_cfg["head_dim"],
        tie_word_embeddings=backbone_cfg.get("tie_word_embeddings", True),
        rope_scaling=None,
    )


class BiQwen3(nn.Module):
    """Qwen3 encoder with bidirectional attention. Module/param names match HF."""

    def __init__(self, args: ModelArgs) -> None:
        super().__init__()
        self.args = args
        self.embed_tokens = nn.Embedding(args.vocab_size, args.hidden_size)
        self.layers = [TransformerBlock(args) for _ in range(args.num_hidden_layers)]
        self.norm = nn.RMSNorm(args.hidden_size, eps=args.rms_norm_eps)

    def __call__(self, input_ids: mx.array, attention_mask: mx.array | None = None) -> mx.array:
        h = self.embed_tokens(input_ids)

        mask = None
        if attention_mask is not None:
            # Additive, bidirectional, padding-aware: 0 for valid keys, the
            # dtype minimum for pad.
            # Shape [B, 1, 1, L] broadcasts over heads and query positions.
            keep = attention_mask.astype(h.dtype)[:, None, None, :]
            mask = (1.0 - keep) * mx.finfo(h.dtype).min

        for layer in self.layers:
            h = layer(h, mask, None)

        return self.norm(h)
