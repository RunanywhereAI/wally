"""MLX PII masking model — bidirectional Qwen3 backbone + two heads.

Adapted from github.com/ppl-ai/pplx-pii-masking, branch
chuan-wang-perplexity/pii-model-quantization (PR #2), mlx_pii/model.py.
The torch `MlxTorchAdapter` and the torch Viterbi import are DROPPED — this build
is mlx-only (torch-free); Viterbi decoding lives in the numpy port `viterbi.py`.
`MlxPiiModel.__call__` mirrors the reference forward exactly.
"""
from __future__ import annotations

import json
from pathlib import Path

import mlx.core as mx
import mlx.nn as nn

from pii_backbone import BiQwen3, build_model_args

QUANTIZABLE_SUFFIXES = ("_proj",)


class MlxPiiModel(nn.Module):
    def __init__(self, backbone_cfg: dict, num_token_labels: int = 37, hidden_size: int = 1024) -> None:
        super().__init__()
        self.backbone = BiQwen3(build_model_args(backbone_cfg))
        self.token_cls_head = nn.Linear(hidden_size, num_token_labels)
        self.sensitivity_head = nn.Linear(hidden_size, 1)

    def __call__(self, input_ids: mx.array, attention_mask: mx.array):
        h = self.backbone(input_ids, attention_mask).astype(mx.float32)  # [B,T,H]

        token_logits = self.token_cls_head(h)  # [B,T,37]

        m = attention_mask.astype(mx.float32)[..., None]  # [B,T,1]
        pooled = (h * m).sum(axis=1) / mx.maximum(m.sum(axis=1), 1.0)  # [B,H]
        sensitivity_logit = self.sensitivity_head(pooled)  # [B,1]

        return token_logits, sensitivity_logit


def _quant_predicate(path: str, module: nn.Module) -> bool:
    # Quantize only the backbone projection Linears; keep embeddings, norms, heads in fp.
    return path.startswith("backbone.") and path.endswith(QUANTIZABLE_SUFFIXES)


def build_and_quantize(backbone_cfg: dict, num_token_labels: int, hidden_size: int,
                       bits: int, group_size: int) -> MlxPiiModel:
    model = MlxPiiModel(backbone_cfg, num_token_labels, hidden_size)
    if bits in (4, 8):
        nn.quantize(model, group_size=group_size, bits=bits, class_predicate=_quant_predicate)
    return model


def load_model(artifact_dir: str | Path):
    """Load the MLX artifact. Returns (model, cfg). Mirrors `load_adapter` minus torch."""
    artifact_dir = Path(artifact_dir)
    cfg = json.loads((artifact_dir / "config.json").read_text())
    model = build_and_quantize(
        cfg["backbone"], cfg["num_token_labels"], cfg["hidden_size"],
        cfg["bits"], cfg["group_size"],
    )
    model.load_weights(str(artifact_dir / "weights.safetensors"))
    model.eval()
    mx.eval(model.parameters())
    return model, cfg
