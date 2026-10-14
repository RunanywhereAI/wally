"""Stops the model cannot overrule. A match means the action is not executed."""

import re

PAY = re.compile(
    r"\b(pay|place order|buy now|purchase|complete purchase|complete booking|confirm and pay|confirm payment|submit payment|charge)\b",
    re.I,
)
PAGE_MONEY = re.compile(r"\b(pay|payment|charge|card)\b", re.I)
SENSITIVE = re.compile(
    r"\b(card|cvv|cvc|otp|upi|password|pin|expiry|expiration|security code|net banking)\b",
    re.I,
)
POSTAL = re.compile(r"\b(pin\s*code|pincode|postal)\b", re.I)
SHORT_COMMIT = re.compile(r"^(yes|confirm|book|reserve)$", re.I)
SENSITIVE_AUTOCOMPLETE = {"cc-number", "cc-csc", "cc-exp", "one-time-code", "current-password", "new-password"}


def sensitive_fill(action) -> bool:
    """A card, password, or OTP field. A postal PIN code is an address, not a credential."""
    label = action.get("label") or ""
    tokens = set((action.get("autocomplete") or "").lower().split())
    if "postal-code" in tokens or POSTAL.search(label):
        return False
    if any(token.startswith("cc-") or token in SENSITIVE_AUTOCOMPLETE for token in tokens):
        return True
    return bool(SENSITIVE.search(label))


def refuses(action, page) -> bool:
    """A payment control, a sensitive field, or a bare confirm sitting on payment text."""
    label = action.get("label") or ""
    kind = action.get("kind")
    if kind in {"click", "select"} and PAY.search(label):
        return True
    if kind == "fill" and sensitive_fill(action):
        return True
    text = (page or {}).get("text") or ""
    return bool(SHORT_COMMIT.match(label.strip()) and PAGE_MONEY.search(text))
