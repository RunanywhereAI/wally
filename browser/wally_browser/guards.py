"""The hard stops. Plain code over the element table: no model output can
override anything here, and nothing here reads a model probability.

Two tiers of controls:

* PAYMENT: a control that commits money (pay, place order, purchase, ...).
  Never clicked, with or without the user's say-so. Reaching one ends the run
  at the payment page.
* COMMIT: a control that books, confirms or reserves without paying yet
  ("Book now", "Confirm booking"). Never clicked by the agent on its own; the
  user is asked each time, and a non-interactive run stops instead.

Fields the agent never types into: passwords, card / CVV / expiry / UPI / OTP
fields (by autocomplete token or label), and anything inside a payment
gateway's frame.
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from enum import Enum
from urllib.parse import urlsplit


@dataclass(frozen=True)
class Element:
    """One interactive element, reduced to what the policy and the guards read."""

    index: int
    tag: str
    role: str = ""
    name: str = ""
    input_type: str = ""
    autocomplete: str = ""
    placeholder: str = ""
    value: str = ""
    frame_url: str = ""  # src of the nearest enclosing iframe, "" for the top document
    attributes: dict = field(default_factory=dict, compare=False, hash=False)

    @property
    def label_text(self) -> str:
        """Every human-readable name the element carries, lowercased."""
        parts = [self.name, self.placeholder, self.attributes.get("aria-label", ""),
                 self.attributes.get("name", ""), self.attributes.get("id", ""),
                 self.attributes.get("title", ""), self.attributes.get("value", "")
                 if self.tag in ("button", "input") and self.input_type in ("submit", "button", "") else ""]
        return " ".join(p for p in parts if p).lower()


class ControlTier(Enum):
    NONE = "none"
    COMMIT = "commit"
    PAYMENT = "payment"


# Payment gateways whose frames or pages mean "payment details from here on".
PAYMENT_GATEWAY_HOSTS = (
    "razorpay.com", "juspay.in", "payu.in", "payumoney.com", "billdesk.com", "ccavenue.com",
    "stripe.com", "stripe.network", "adyen.com", "adyenpayments.com", "paypal.com", "paytm.in", "paytm.com",
    "cashfree.com", "phonepe.com", "braintreegateway.com", "checkout.com", "worldpay.com",
    "cybersource.com", "authorize.net", "klarna.com", "airpay.co.in", "easebuzz.in", "instamojo.com",
)

# Bot checks a person has to solve.
BOT_CHECK_HOSTS = (
    "google.com/recaptcha", "recaptcha.net", "hcaptcha.com", "challenges.cloudflare.com",
    "arkoselabs.com", "funcaptcha.com", "geetest.com", "perimeterx.net", "px-cloud.net",
)
BOT_CHECK_TITLES = re.compile(r"just a moment|attention required|verify you are (a )?human|are you a robot|"
                              r"security check|access denied", re.I)

# Autocomplete tokens and labels of fields never typed into.
SENSITIVE_AUTOCOMPLETE = ("cc-", "one-time-code", "current-password", "new-password")
SENSITIVE_LABEL = re.compile(
    r"card[ _-]?(number|num\b|no\b|holder)|name ?on ?(the )?card|credit[ _-]?card|debit[ _-]?card|"
    r"\bcvv|\bcvc|\bcsc\b|security[ _-]?code|"
    r"expir|valid ?(thru|through|till)|\bmm ?/ ?yy|\bupi\b|\bvpa\b|\bpin\b|\bmpin|\botp\b|one[- ]time|"
    r"verification ?code|net ?banking|ifsc|account ?number|aadhaar|password|passcode",
    re.I,
)

_PAY_WORDS = r"(₹|rs\.?|inr|\$|€|£|usd)?\s*[\d,]+(\.\d+)?"
PAYMENT_CONTROL = re.compile(
    rf"^\s*pay\b|\bpay\s*now\b|\bpay\s+{_PAY_WORDS}|\bpay\s+securely\b|\bmake\s+(a\s+)?payment\b|"
    r"\bplace\s+(your\s+)?order\b|\bpurchase\b|\bbuy\s+now\b|\bcomplete\s+(the\s+)?(purchase|payment|order|booking)\b|"
    r"\bconfirm\s+(and|&)\s+pay\b|\bconfirm\s+payment\b|\bsubmit\s+payment\b|\bauthori[sz]e\s+payment\b",
    re.I,
)
COMMIT_CONTROL = re.compile(
    r"\bbook\b|\bbooking\s+confirm|\bconfirm\b|\breserve\b|\breservation\b|\bcheckout\b|\bcheck\s+out\b",
    re.I,
)
# Wording that only navigates toward payment; allowed even though it says "pay".
NAVIGATES_TO_PAYMENT = re.compile(r"\b(continue|proceed|go)\s+to\s+(the\s+)?(payment|checkout)\b", re.I)


def host_of(url: str) -> str:
    try:
        return (urlsplit(url).hostname or "").lower()
    except ValueError:
        return ""


def _host_matches(url: str, hosts: tuple[str, ...]) -> bool:
    lowered = url.lower()
    host = host_of(url)
    for entry in hosts:
        if "/" in entry:
            if entry in lowered:
                return True
        elif host == entry or host.endswith("." + entry):
            return True
    return False


def is_payment_gateway(url: str) -> bool:
    return bool(url) and _host_matches(url, PAYMENT_GATEWAY_HOSTS)


def is_bot_check_url(url: str) -> bool:
    return bool(url) and _host_matches(url, BOT_CHECK_HOSTS)


def never_type_reason(element: Element) -> str | None:
    """Why the agent must not type into this element, or None when it may."""
    if element.input_type == "password":
        return "password field"
    token = element.autocomplete.lower()
    if any(token.startswith(prefix) or f" {prefix}" in f" {token}" for prefix in SENSITIVE_AUTOCOMPLETE):
        return f"autocomplete={element.autocomplete}"
    if is_payment_gateway(element.frame_url):
        return f"inside a payment gateway frame ({host_of(element.frame_url)})"
    match = SENSITIVE_LABEL.search(element.label_text)
    if match:
        return f"label mentions '{match.group(0).strip()}'"
    return None


def control_tier(element: Element) -> ControlTier:
    """How dangerous a click on this element is. Read from its visible name."""
    if is_payment_gateway(element.frame_url):
        return ControlTier.PAYMENT
    text = element.label_text
    if not text:
        return ControlTier.NONE
    if NAVIGATES_TO_PAYMENT.search(text):
        return ControlTier.NONE
    if PAYMENT_CONTROL.search(text):
        return ControlTier.PAYMENT
    if COMMIT_CONTROL.search(text):
        return ControlTier.COMMIT
    return ControlTier.NONE


@dataclass(frozen=True)
class PageCheck:
    """What the rules say about a whole page."""

    payment_page: str | None = None  # why this is the payment page, or None
    bot_check: str | None = None  # why a person has to solve something, or None


def check_page(url: str, title: str, elements: list[Element], frame_urls: list[str] = ()) -> PageCheck:
    """Rule-based page classification. A model may add a stop; it may never remove one."""
    payment = None
    if is_payment_gateway(url):
        payment = f"the page is on a payment gateway ({host_of(url)})"
    for frame in frame_urls:
        if payment is None and is_payment_gateway(frame):
            payment = f"a payment gateway frame is on the page ({host_of(frame)})"
    if payment is None:
        for element in elements:
            if element.tag in ("input", "select", "textarea") and element.input_type != "password":
                reason = never_type_reason(element)
                if reason and ("cc-" in element.autocomplete.lower() or "card" in reason or "cvv" in reason
                               or "cvc" in reason or "upi" in reason or "expir" in reason or "gateway" in reason):
                    payment = f"payment field [{element.index}] ({reason})"
                    break
    bot = None
    if BOT_CHECK_TITLES.search(title or ""):
        bot = f"the page title reads '{title}'"
    for frame in (*frame_urls, *(e.frame_url for e in elements)):
        if bot is None and is_bot_check_url(frame):
            bot = f"a bot-check frame is on the page ({host_of(frame) or frame})"
    return PageCheck(payment_page=payment, bot_check=bot)


def dialog_should_accept(dialog_type: str) -> bool:
    """JavaScript dialogs: only a plain alert is accepted (it is informational).
    confirm(), prompt() and beforeunload are dismissed, so a "Confirm payment?"
    or "Leave this page?" never goes through on the agent's behalf. This
    replaces browser-use's PopupsWatchdog, which accepts confirm() and
    beforeunload."""
    return dialog_type == "alert"
