"""Stops the model cannot overrule. A match means the action is not executed."""

import re

PAY = re.compile(
    r"\b(pay|place order|buy now|purchase|complete purchase|complete booking|confirm and pay|confirm payment)\b",
    re.I,
)
SENSITIVE = re.compile(r"\b(card|cvv|cvc|otp|upi|password|pin|expiry|net banking)\b", re.I)
SHORT_COMMIT = re.compile(r"^(yes|confirm|book|reserve)$", re.I)
SENSITIVE_AUTOCOMPLETE = {"cc-number", "cc-csc", "cc-exp", "one-time-code", "current-password", "new-password"}


def refuses(action, page) -> bool:
    """A payment control, a sensitive field, or a bare confirm sitting on payment text."""
    label = action.get("label") or ""
    kind = action.get("kind")
    if kind in {"click", "select"} and PAY.search(label):
        return True
    if kind == "fill" and (
        SENSITIVE.search(label) or action.get("autocomplete") in SENSITIVE_AUTOCOMPLETE
    ):
        return True
    text = (page or {}).get("text") or ""
    return bool(SHORT_COMMIT.match(label.strip()) and PAY.search(text))
