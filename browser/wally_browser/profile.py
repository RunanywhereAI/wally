"""The traveller profile: the only source of personal details the agent types.

`~/.config/wally/traveller.toml` (or `--profile FILE`):

    [traveller]
    first_name = "Asha"
    last_name = "Rao"
    date_of_birth = "1990-04-12"
    seat_preference = "window"

Only the keys below are accepted. A card, CVV, UPI, bank or OTP field is
refused by name, so the agent has no payment value to type even if a page
asked for one. Unknown keys are refused too, so a typo cannot silently drop a
detail the person meant to give.
"""

from __future__ import annotations

import os
import re
import tomllib
from dataclasses import dataclass
from pathlib import Path

# Profile key -> what it means, as offered to the decision model's value heads.
FIELDS: dict[str, str] = {
    "title": "title or salutation (Mr, Ms, Dr)",
    "first_name": "first or given name",
    "middle_name": "middle name",
    "last_name": "last name or surname",
    "full_name": "full name as on the ID",
    "gender": "gender",
    "date_of_birth": "date of birth (YYYY-MM-DD)",
    "nationality": "nationality",
    "passport_number": "passport number",
    "passport_expiry": "passport expiry date (YYYY-MM-DD)",
    "passport_country": "passport issuing country",
    "email": "email address",
    "phone": "mobile phone number",
    "phone_country_code": "phone country code (+91)",
    "frequent_flyer_airline": "frequent flyer airline",
    "frequent_flyer_number": "frequent flyer number",
    "seat_preference": "seat preference (window, aisle)",
    "meal_preference": "meal preference",
    "home_city": "home city",
}

# Refused outright, with a reason, whatever the key is called.
_PAYMENT_KEY = re.compile(r"card|cvv|cvc|csc|upi|vpa|pin|otp|bank|ifsc|account|iban|swift|aadhaar|password|"
                          r"wallet|paypal|netbanking|billing", re.I)
_DATE = re.compile(r"^\d{4}-\d{2}-\d{2}$")


# A profile value is typed only into a field whose label says it is that kind of field. eve's
# value head proposes the key; this rule, in code, decides whether the value may go there, so a
# passport number never lands in a promo-code or search box.
KEY_FIELDS: dict[str, re.Pattern] = {key: re.compile(pattern, re.I) for key, pattern in {
    "title": r"\btitle\b|salutation|prefix",
    "first_name": r"first|given|\bfname|forename",
    "middle_name": r"middle",
    "last_name": r"last|surname|family|\blname",
    "full_name": r"full ?name|\bname\b",
    "gender": r"gender|\bsex\b",
    "date_of_birth": r"birth|\bdob\b|\bbday",
    "nationality": r"nationality|citizenship",
    "passport_number": r"passport",
    "passport_expiry": r"passport.*expir|expir.*passport",
    "passport_country": r"passport.*(country|issu)|issu(ing|ed).*country",
    "email": r"e-?mail",
    "phone": r"phone|mobile|\btel\b|contact ?(number|no)",
    "phone_country_code": r"country ?code|\bdial|\bisd\b",
    "frequent_flyer_airline": r"frequent|loyalty|airline",
    "frequent_flyer_number": r"frequent|loyalty|member(ship)? ?(number|no|id)",
    "seat_preference": r"\bseat",
    "meal_preference": r"\bmeal",
    "home_city": r"\bcity\b",
}.items()}
AUTOCOMPLETE_KEYS = {"given-name": "first_name", "additional-name": "middle_name", "family-name": "last_name",
                     "name": "full_name", "email": "email", "tel": "phone", "tel-national": "phone",
                     "bday": "date_of_birth", "honorific-prefix": "title", "sex": "gender"}


def key_fits_field(key: str, label_text: str, autocomplete: str = "") -> bool:
    """Whether a profile value of kind `key` may be typed into a field with this label."""
    token = autocomplete.split(" ")[-1].lower() if autocomplete else ""
    if token in AUTOCOMPLETE_KEYS:
        return AUTOCOMPLETE_KEYS[token] == key
    rule = KEY_FIELDS.get(key)
    return bool(rule and rule.search(label_text))


class ProfileError(ValueError):
    pass


@dataclass(frozen=True)
class Profile:
    values: dict[str, str]
    path: str = ""

    def get(self, key: str) -> str | None:
        value = self.values.get(key)
        if value is None and key == "full_name":
            parts = [self.values.get(k, "") for k in ("first_name", "middle_name", "last_name")]
            joined = " ".join(p for p in parts if p)
            return joined or None
        return value

    def has(self, key: str) -> bool:
        return self.get(key) is not None

    def keys(self) -> list[str]:
        """Keys with a value, in FIELDS order (full_name counts when derivable)."""
        return [key for key in FIELDS if self.has(key)]


def default_path() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "wally" / "traveller.toml"


def parse(document: dict, path: str = "") -> Profile:
    table = document.get("traveller")
    if table is None:
        raise ProfileError(f"{path or 'profile'}: no [traveller] table")
    if not isinstance(table, dict):
        raise ProfileError(f"{path or 'profile'}: [traveller] must be a table")
    extra = sorted(set(document) - {"traveller"})
    if extra:
        raise ProfileError(f"{path or 'profile'}: unknown top-level keys: {', '.join(extra)}")
    values: dict[str, str] = {}
    for key, value in table.items():
        if _PAYMENT_KEY.search(key):
            raise ProfileError(
                f"{path or 'profile'}: '{key}' looks like payment or account data; wally never stores or types it"
            )
        if key not in FIELDS:
            raise ProfileError(f"{path or 'profile'}: unknown key '{key}' (known: {', '.join(FIELDS)})")
        if not isinstance(value, str) or not value.strip():
            raise ProfileError(f"{path or 'profile'}: '{key}' must be a non-empty string")
        if key in ("date_of_birth", "passport_expiry") and not _DATE.match(value):
            raise ProfileError(f"{path or 'profile'}: '{key}' must be YYYY-MM-DD")
        values[key] = value.strip()
    return Profile(values=values, path=path)


def load(path: str | os.PathLike | None = None) -> Profile:
    """The profile at `path`, or the default one, or an empty profile when the
    default file does not exist. A named file that is missing is an error."""
    explicit = path is not None
    target = Path(path) if explicit else default_path()
    if not target.exists():
        if explicit:
            raise ProfileError(f"{target}: no such file")
        return Profile(values={}, path="")
    try:
        document = tomllib.loads(target.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as error:
        raise ProfileError(f"{target}: {error}") from None
    return parse(document, str(target))
