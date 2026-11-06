"""PII BIOES label map.

Vendored from github.com/ppl-ai/pplx-pii-masking (model/config.py, commit 81746c3).
Kept 1:1 with the training config so decoded label indices match the checkpoint.
"""
from __future__ import annotations

PII_TYPES = [
    "private_person",
    "private_email",
    "private_phone",
    "private_address",
    "private_url",
    "private_date",
    "account_number",
    "secret",
    "other_pii",
]

# BIOES label list: O + 9 types x {B, I, E, S} = 37
BIOES_LABELS = ["O"] + [
    f"{tag}-{pii_type}"
    for pii_type in PII_TYPES
    for tag in ["B", "I", "E", "S"]
]
LABEL2ID = {label: idx for idx, label in enumerate(BIOES_LABELS)}
ID2LABEL = {idx: label for idx, label in enumerate(BIOES_LABELS)}

assert len(BIOES_LABELS) == 37
