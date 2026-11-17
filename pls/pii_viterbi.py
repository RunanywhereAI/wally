"""Constrained BIOES Viterbi decoder — numpy port.

Ported from github.com/ppl-ai/pplx-pii-masking (model/viterbi.py, commit 81746c3).
Same algorithm as the torch `ViterbiDecoder`: BIOES transition-validity mask,
learnable `b_bias`/`e_bias`, O/B/S start states, O/E/S end states, S=single and
B..I..E=multi span extraction, whitespace strip, span score = sigmoid(mean token
logit). torch/nn are replaced by numpy so this runs in the mlx-lm venv (no torch).
"""
from __future__ import annotations

import numpy as np

from pii_labels import BIOES_LABELS, ID2LABEL, LABEL2ID, PII_TYPES

_NEG_INF = float("-inf")


def _sigmoid(x: float) -> float:
    return float(1.0 / (1.0 + np.exp(-x)))


def build_transition_mask() -> np.ndarray:
    n = len(BIOES_LABELS)
    mask = np.zeros((n, n), dtype=bool)

    end_states = {LABEL2ID["O"]}
    begin_states = {LABEL2ID["O"]}

    for pii_type in PII_TYPES:
        b = LABEL2ID[f"B-{pii_type}"]
        i = LABEL2ID[f"I-{pii_type}"]
        e = LABEL2ID[f"E-{pii_type}"]
        s = LABEL2ID[f"S-{pii_type}"]

        end_states.add(e)
        end_states.add(s)
        begin_states.add(b)
        begin_states.add(s)

        mask[b, i] = True
        mask[b, e] = True
        mask[i, i] = True
        mask[i, e] = True

    for from_state in end_states:
        for to_state in begin_states:
            mask[from_state, to_state] = True

    return mask


def _build_transition_scores(num_labels: int, b_bias: float, e_bias: float) -> np.ndarray:
    mask = build_transition_mask()
    scores = np.full((num_labels, num_labels), _NEG_INF, dtype=np.float64)
    scores[mask] = 0.0

    for pii_type in PII_TYPES:
        b = LABEL2ID[f"B-{pii_type}"]
        e = LABEL2ID[f"E-{pii_type}"]
        for from_s in range(num_labels):
            if scores[from_s, b] > _NEG_INF:
                scores[from_s, b] += b_bias
        for to_s in range(num_labels):
            if scores[e, to_s] > _NEG_INF:
                scores[e, to_s] += e_bias

    return scores


def _strip_span_whitespace(text: str, spans: list) -> list:
    out = []
    for (start, end, label, score) in spans:
        new_start, new_end = start, end
        while new_start < new_end and text[new_start] in (" ", "\t", "\n"):
            new_start += 1
        while new_end > new_start and text[new_end - 1] in (" ", "\t", "\n"):
            new_end -= 1
        if new_start < new_end:
            out.append((new_start, new_end, label, score))
    return out


def _path_to_spans(path, logits, offset_mapping):
    spans = []
    T = len(path)
    t = 0
    while t < T:
        label = ID2LABEL[path[t]]
        char_start, char_end = offset_mapping[t]

        if label.startswith("S-"):
            pii_type = label[2:]
            score = _sigmoid(float(logits[t, path[t]]))
            if char_start < char_end:
                spans.append((char_start, char_end, pii_type, score))
            t += 1

        elif label.startswith("B-"):
            pii_type = label[2:]
            span_start = char_start
            span_end = char_end
            tok_scores = [float(logits[t, path[t]])]
            t += 1
            while t < T:
                inner = ID2LABEL[path[t]]
                if inner == f"I-{pii_type}":
                    _, span_end = offset_mapping[t]
                    tok_scores.append(float(logits[t, path[t]]))
                    t += 1
                elif inner == f"E-{pii_type}":
                    _, span_end = offset_mapping[t]
                    tok_scores.append(float(logits[t, path[t]]))
                    t += 1
                    break
                else:
                    break
            if span_start < span_end:
                score = _sigmoid(float(np.mean(tok_scores)))
                spans.append((span_start, span_end, pii_type, score))
        else:
            t += 1
    return spans


def decode(logits: np.ndarray, offset_mapping, text: str,
           b_bias: float = 0.0, e_bias: float = 0.0, num_labels: int = 37) -> list:
    """Decode a single sequence. `logits` is [T, num_labels]. Returns a list of
    (start, end, label, score) tuples with whitespace-stripped char offsets."""
    logits = np.asarray(logits, dtype=np.float64)
    T, C = logits.shape
    assert C == num_labels

    trans = _build_transition_scores(num_labels, b_bias, e_bias)

    viterbi_scores = np.full((T, C), _NEG_INF, dtype=np.float64)
    backpointers = np.zeros((T, C), dtype=np.int64)

    start_mask = np.full((C,), _NEG_INF, dtype=np.float64)
    for label, idx in LABEL2ID.items():
        if label == "O" or label.startswith("B-") or label.startswith("S-"):
            start_mask[idx] = 0.0
    viterbi_scores[0] = logits[0] + start_mask

    for t in range(1, T):
        scores_t = viterbi_scores[t - 1][:, None] + trans  # [C_prev, C_next]
        best_idx = np.argmax(scores_t, axis=0)  # [C_next]
        best_prev = scores_t[best_idx, np.arange(C)]
        viterbi_scores[t] = logits[t] + best_prev
        backpointers[t] = best_idx

    end_mask = np.full((C,), _NEG_INF, dtype=np.float64)
    for label, idx in LABEL2ID.items():
        if label == "O" or label.startswith("E-") or label.startswith("S-"):
            end_mask[idx] = 0.0
    final_scores = viterbi_scores[T - 1] + end_mask
    best_last = int(np.argmax(final_scores))

    path = [best_last]
    for t in range(T - 1, 0, -1):
        path.append(int(backpointers[t, path[-1]]))
    path.reverse()

    spans = _path_to_spans(path, logits, offset_mapping)
    return _strip_span_whitespace(text, spans)
