"""The hard stops. Plain code over the element table: no model output can
override anything here, and nothing here reads a model probability.

Two tiers of controls:

* PAYMENT: a control that commits money (pay, place order, buy, subscribe, ...).
  Never clicked, with or without the user's say-so. Reaching one ends the run.
* COMMIT: a control that books, confirms or reserves without paying yet. Never
  clicked by the agent on its own; the person is asked each time.

Labels cannot be read perfectly (other languages, icons, CSS-drawn text), so
the label tiers are not the only line: on a page with checkout signals, and on
the person's own Chrome, every click needs the person's yes (tools.py), and
the payment page ends the run whatever its buttons say.

Text is normalised before matching (Unicode NFKC, zero-width characters and
soft hyphens removed, identifiers split on camelCase / _ / - / digits), and a
short label is also matched with its spaces squashed ("P a y" → "pay").
"""

from __future__ import annotations

import re
import unicodedata
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
    # The element's whole text (name plus every descendant, shadow roots included), never
    # clipped to the 80-character display name. Head and tail are kept when it is long.
    full_text: str = field(default="", compare=False, hash=False)
    # Text of the nearest dialog / modal ancestor ("Pay ₹4,500 from your saved card?") so a
    # bare "Yes" inside it is read in context.
    context_text: str = field(default="", compare=False, hash=False)

    @property
    def visible_parts(self) -> list[str]:
        """What a person reads on or about the element, each normalised on its own."""
        a = self.attributes
        raw = [self.full_text, self.name, self.placeholder, a.get("aria-label", ""), a.get("title", ""),
               a.get("alt", ""), a.get("value", "") if self.input_type in ("submit", "button", "image", "") else ""]
        return [p for p in (normalize(r) for r in raw if r and r.strip()) if p]

    @property
    def identifier_parts(self) -> list[str]:
        """What the page's code calls it (id, name, class, test ids, href path), split into words."""
        a = self.attributes
        identifiers = [a.get("id", ""), a.get("name", ""), a.get("class", ""), a.get("formaction", ""),
                       a.get("data-testid", ""), a.get("data-test", ""),
                       a.get("for", "")]  # <label for="placeOrderBtn"> activates that control
        href = a.get("href", "")
        if href:
            try:
                parts = urlsplit(href)
                identifiers.append(f"{parts.path} {parts.query}")
            except ValueError:
                identifiers.append(href)
        return [p for p in (normalize(split_identifier(i)) for i in identifiers if i and i.strip()) if p]

    @property
    def text_parts(self) -> list[str]:
        return self.visible_parts + self.identifier_parts

    @property
    def label_text(self) -> str:
        return " ".join(self.text_parts)


class ControlTier(Enum):
    NONE = "none"
    COMMIT = "commit"
    PAYMENT = "payment"


# --- text normalisation -----------------------------------------------------

_INVISIBLE = dict.fromkeys(map(ord, "­​‌‍‎‏⁠﻿᠎"), None)


def normalize(text: str) -> str:
    """NFKC (fullwidth Ｐａｙ → pay), invisible characters dropped, lowercased, spaces collapsed."""
    text = unicodedata.normalize("NFKC", text or "").translate(_INVISIBLE).lower()
    return " ".join(text.split())


def split_identifier(text: str) -> str:
    """payNow → pay Now, place-order → place order, card_cvv → card cvv, otp1 → otp 1."""
    text = re.sub(r"([a-z])([A-Z])", r"\1 \2", text or "")
    text = re.sub(r"([A-Za-z])(\d)", r"\1 \2", text)
    text = re.sub(r"(\d)([A-Za-z])", r"\1 \2", text)
    return re.sub(r"[_\-./=&?#:]+", " ", text)


def squash(text: str) -> str:
    """Letters and digits only: 'p ay', 'p-a-y' and 'pay100' all start with 'pay'."""
    return re.sub(r"[\W_]+", "", text)


# --- hosts --------------------------------------------------------------------

# Payment gateways, 3-D Secure and wallets: their frames or pages mean "payment from here on".
PAYMENT_GATEWAY_HOSTS = (
    "razorpay.com", "rzp.io", "juspay.in", "juspay.io", "hyperswitch.io", "payu.in", "payumoney.com",
    "billdesk.com", "ccavenue.com", "stripe.com", "adyen.com", "adyenpayments.com", "paypal.com",
    "paytm.in", "paytm.com", "paytmpayments.com", "cashfree.com", "phonepe.com", "braintreegateway.com",
    "checkout.com", "worldpay.com", "cybersource.com", "authorize.net", "klarna.com", "airpay.co.in",
    "easebuzz.in", "instamojo.com", "pay.google.com", "payments.amazon.in", "amazonpay.in", "sbiepay.sbi",
    "shopifyinc.com", "cardinalcommerce.com", "3dsecure.io", "payglocal.in", "mobikwik.com",
    "freecharge.in", "squareup.com", "mollie.com", "2checkout.com", "paddle.com",
    # Not stripe.network: its m.stripe.network frame is fraud detection on every page of a
    # Stripe shop, not a payment form.
)

BOT_CHECK_HOSTS = (
    "google.com/recaptcha", "recaptcha.net", "hcaptcha.com", "challenges.cloudflare.com",
    "arkoselabs.com", "funcaptcha.com", "geetest.com", "perimeterx.net", "px-cloud.net",
)
BOT_CHECK_TITLES = re.compile(r"just a moment|attention required|verify you are (a )?human|are you a robot|"
                              r"security check|access denied", re.I)


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


# A link that pays: a host whose only job is taking payment, or a checkout/pay path on a gateway.
PAYMENT_LINK_HOSTS = ("rzp.io", "checkout.razorpay.com", "api.razorpay.com", "checkout.stripe.com", "buy.stripe.com",
                      "pay.google.com", "secure.payu.in", "securegw.paytmpayments.com", "securegw.paytm.in",
                      "pay.amazon.com", "payments.amazon.in", "checkout.paypal.com", "www.sbiepay.sbi")
_PAYMENT_LINK_PATH = re.compile(r"checkout|/pay\b|/pay/|payment|/session|/invoice|/link/|webapps|cgi-bin/webscr",
                                re.I)
# Hidden helper frames a gateway loads on ordinary pages: not a payment form.
_GATEWAY_HELPER_FRAME = re.compile(r"js\.stripe\.com/v\d/(controller|m-outer|m-inner)|stripecdn\.com|"
                                   r"/hcaptcha|invisible", re.I)


def is_payment_link(url: str) -> bool:
    if not url:
        return False
    if _host_matches(url, PAYMENT_LINK_HOSTS):
        return True
    return is_payment_gateway(url) and bool(_PAYMENT_LINK_PATH.search(urlsplit(url).path or ""))


def is_payment_frame(url: str) -> bool:
    return is_payment_gateway(url) and not _GATEWAY_HELPER_FRAME.search(url)


def is_bot_check_url(url: str) -> bool:
    return bool(url) and _host_matches(url, BOT_CHECK_HOSTS)


# --- fields never typed into ---------------------------------------------------

SENSITIVE_AUTOCOMPLETE = ("cc-", "one-time-code", "current-password", "new-password")

# (category, pattern): the category decides whether the field also marks the payment page.
SENSITIVE_PATTERNS: tuple[tuple[str, re.Pattern], ...] = tuple((cat, re.compile(p, re.I)) for cat, p in (
    ("card", r"card ?(number|num|no|holder|#)|name (as )?on (the )?card|as on card|credit ?card|debit ?card|"
             r"\bcvv|\bcvc|\bcsc\b|\bcvn\b|\bcid\b|security ?code|card ?security|\bccn\b|\bcc ?(num|number|no)\b|"
             r"\bccnum|\bcardnum|expir|valid ?(thru|through|till|until|upto)|\bexp\b|\bexp ?(month|year|date|mm|yy)|"
             r"\bmm ?/ ?yy\b|(x{4}[ -]?){3}x{4}|\d{4} ?\d{4} ?\d{4} ?\d{4}|karten ?nummer|numéro de carte|"
             r"número de (la )?tarjeta|कार्ड|सीवीवी"),
    ("upi", r"\bupi\b|\bvpa\b|@ok(axis|sbi|hdfcbank|icici)|@ybl|@paytm|यूपीआई"),
    ("bank", r"net ?banking|\bifsc\b|account ?(number|no)|\biban\b|\bswift\b|routing ?number|sort ?code"),
    ("otp", r"\botp\b|one ?time|verification ?code|\b(enter|sms|email|phone|mobile)( the)? code\b|"
            r"\b\d ?digit code|\bdigit \d+ of \d+|\bcode (sent|we sent)|ओटीपी"),
    # Masked placeholders ("•••", "***") read as a secret, but not by themselves as a payment form.
    ("pin", r"\bpin\b|\bmpin|\batm ?pin|\btpin|passcode|password|•{3}|\*{3}|पासवर्ड|पिन"),
    ("id", r"aadhaar|आधार"),
))
_BARE_EXPIRY = re.compile(r"\s*(mm|yy|yyyy|mm ?/ ?yy(yy)?|month|year|exp\.?)\s*", re.I)
PAYMENT_CATEGORIES = {"card", "upi", "bank", "gateway"}


def sensitive_field(element: Element) -> tuple[str, str] | None:
    """(category, why) when the agent must not type into this element, else None."""
    if element.input_type == "password":
        return "pin", "password field"
    token = normalize(element.autocomplete)
    if any(token.startswith(p) or f" {p}" in f" {token}" for p in SENSITIVE_AUTOCOMPLETE):
        category = "otp" if "one-time" in token else "pin" if "password" in token else "card"
        return category, f"autocomplete={element.autocomplete}"
    if is_payment_frame(element.frame_url):
        return "gateway", f"inside a payment gateway frame ({host_of(element.frame_url)})"
    text = element.label_text
    for category, pattern in SENSITIVE_PATTERNS:
        match = pattern.search(text)
        if match:
            return category, f"label mentions '{match.group(0).strip()}'"
    visible = element.name or element.placeholder or element.attributes.get("aria-label", "")
    if visible and _BARE_EXPIRY.fullmatch(normalize(visible)) and "birth" not in text and "dob" not in text:
        return "card", f"a bare '{visible.strip()}' field reads as a card expiry"
    return None


def never_type_reason(element: Element) -> str | None:
    found = sensitive_field(element)
    return found[1] if found else None


_CARD_NUMBER = re.compile(r"(?:\d[ \-]?){13,19}")


def looks_like_card_number(value: str) -> bool:
    digits = re.sub(r"\D", "", value or "")
    return 13 <= len(digits) <= 19 and bool(_CARD_NUMBER.fullmatch((value or "").strip()))


# --- controls ------------------------------------------------------------------

_PAY = (
    r"\bpay\b|\bpaynow\b|\bpay ?\d|\bmake (a )?payment\b|\bplace (your |the )?order\b|\bpurchase\b|\bbuy\b|"
    r"\border now\b|\b(submit|confirm|complete|finali[sz]e) (your |the |my )?order\b|"
    r"\bcomplete (the )?(purchase|payment|booking)\b|\bconfirm payment\b|\bsubmit payment\b|"
    r"\bauthori[sz]e (the )?payment\b|\bcheckout (and|&) pay\b|\bsubscribe\b|\bupgrade( now)?\b|\bdonate\b|"
    r"\brecharge( now)?\b|\btop ?up\b|\badd money\b|\bsend money\b|\bplace (a )?bid\b|\bbid now\b|"
    r"\bcharged?\b|\bdebit(ed)?\b|\bdeduct(ed)?\b|\bauto-? ?renew|\bautopay\b|\bsubscription\b|\brent\b.*\d|"
    r"^(₹|rs\.?|inr|\$|€|£|usd)\s*[\d,]+(\.\d+)?$|"
    # Material icon ligatures joined by underscores are distinctive. A bare "payments" is not
    # (it is also a nav link); an icon-only pay button on a checkout page is confirmed anyway.
    r"shopping_cart_checkout|^credit_card$|^account_balance_wallet$|"
    # Hindi, German, French, Spanish, Portuguese
    r"भुगतान|खरीद|ऑर्डर करें|zahlungspflichtig|jetzt kaufen|\bkaufen\b|bezahlen|\bpayer\b|acheter|commander|"
    r"\bpagar\b|comprar|realizar pedido|finalizar compra"
)
_COMMIT = (
    r"\bbook\b|\bbooking\b|\bconfirm\b|\breserve\b|\breservation\b|\bcheckout\b|\bcheck out\b|\bhold\b|"
    r"\bsubmit\b|\bagree and continue\b|\baccept and continue\b|\bschedule\b|\brequest\b|\bregister\b|"
    r"\brsvp\b|\bget tickets?\b|\benrol+\b|\bapply( now)?\b|\bclaim\b|\bsign ?up\b|\bjoin\b|"
    r"\bcancel (my |the |this )?(booking|trip|order|reservation|ticket|subscription)\b|\bfee\b|"
    r"बुक करें|पुष्टि|buchen|bestätigen|réserver|confirmer|reservar|confirmar"
)
PAYMENT_CONTROL = re.compile(_PAY, re.I)
COMMIT_CONTROL = re.compile(_COMMIT, re.I)
# On ids, classes and href paths: only unambiguous pay/order wording ("/payments" is an info page).
IDENTIFIER_PAYMENT = re.compile(r"\bpay\b|\bpay ?now\b|\bplace ?order\b|\bbuy ?now\b|\b(submit|confirm|complete) ?order\b|"
                                r"\bcomplete ?purchase\b|\bcheckout ?pay\b|\bmake ?payment\b|\bconfirm ?payment\b", re.I)
_PAY_SQUASHED = re.compile(r"^(pay|paynow|payrs|payinr|buynow|buy|placeorder|purchase|ordernow|"
                           r"confirmandpay|completepurchase)\d*$")
# A label that is ONLY a navigation phrase moves toward payment without committing.
NAVIGATES_TO_PAYMENT = re.compile(
    r"\s*(continue|proceed|go|next)(:)?\s+(to\s+)?(the\s+)?(payment|checkout|payment page|review)(\s+page)?"
    r"\s*[>→»]*\s*", re.I)
# Inside a money or commit dialog ("Pay ₹4,500 from your wallet?", "Cancel this booking? A fee
# applies."), every button is read in that context: "Yes, continue", "OK" and even "Cancel" can be
# the commit. Only a clearly negative answer in a money dialog drops to "ask the person".
NEGATIVE = re.compile(r"\s*(no|no thanks|not now|close|keep|keep (my |the )?(booking|plan)|back|go back|dismiss|"
                      r"maybe later|don'?t|×|x)( ?[,!.].*)?\s*", re.I)


def control_tier(element: Element) -> ControlTier:
    """How dangerous a click on this element is. Every text the element carries is
    checked on its own: a pay word in ANY of them makes it PAYMENT, and no navigation
    phrase elsewhere can lower that."""
    if is_payment_frame(element.frame_url) or is_payment_link(element.attributes.get("href", "")):
        return ControlTier.PAYMENT
    visible, identifiers = element.visible_parts, element.identifier_parts
    parts = visible + identifiers
    if any(PAYMENT_CONTROL.search(part) for part in visible):
        return ControlTier.PAYMENT
    if any(IDENTIFIER_PAYMENT.search(part) for part in identifiers):
        return ControlTier.PAYMENT
    if any(len(part) <= 40 and _PAY_SQUASHED.match(squash(part)) for part in parts):
        return ControlTier.PAYMENT
    context = normalize(element.context_text)
    if context:
        negative = any(NEGATIVE.fullmatch(part) for part in visible)
        if PAYMENT_CONTROL.search(context):
            return ControlTier.COMMIT if negative else ControlTier.PAYMENT
        if COMMIT_CONTROL.search(context):
            return ControlTier.COMMIT
    committing = [part for part in parts if COMMIT_CONTROL.search(part)]
    if committing and not all(NAVIGATES_TO_PAYMENT.fullmatch(part) for part in committing):
        return ControlTier.COMMIT
    return ControlTier.NONE


# --- pages -----------------------------------------------------------------------

_TYPEABLE_TAGS = {"input", "select", "textarea"}
_CHECKOUT_WORDS = re.compile(r"checkout|payment|\bpay\b|review|booking|\border\b|\bcart\b|summary|itinerary|"
                             r"traveller|passenger|add ?ons|\bseat|\bfare\b", re.I)


@dataclass(frozen=True)
class PageCheck:
    """What the rules say about a whole page."""

    payment_page: str | None = None  # why this is the payment page, or None
    bot_check: str | None = None  # why a person has to solve something, or None
    checkout: str | None = None  # why every click here needs the person's yes, or None


_NOT_TEXT_INPUTS = {"submit", "button", "image", "reset", "checkbox", "radio", "file", "hidden", "range", "color"}


def is_typeable(element: Element) -> bool:
    """A real text field: <input> of a text kind, <textarea>, or contenteditable. An ARIA role on a
    <button> or <div> is not enough: typing a space or Enter into a focused button clicks it, and
    an <input type=submit> is a button."""
    if element.tag == "input":
        return element.input_type not in _NOT_TEXT_INPUTS
    if element.tag == "textarea":
        return True
    return element.tag not in ("button", "a", "label") and element.attributes.get("contenteditable") in ("", "true")


def is_selectable(element: Element) -> bool:
    """A dropdown: a native <select>, or an ARIA combobox/listbox (whose options get clicked)."""
    return element.tag == "select" or element.role in ("combobox", "listbox")


def check_page(url: str, title: str, elements: list[Element], frame_urls: list[str] = ()) -> PageCheck:
    """Rule-based page classification. A model may add a stop; it may never remove one."""
    payment = None
    if is_payment_gateway(url):
        payment = f"the page is on a payment gateway ({host_of(url)})"
    for frame in frame_urls:
        if payment is None and is_payment_frame(frame):
            payment = f"a payment gateway frame is on the page ({host_of(frame)})"
    if payment is None:
        for element in elements:
            if not (is_typeable(element) or is_selectable(element) or element.role in ("textbox", "searchbox")):
                continue
            found = sensitive_field(element)
            if found and found[0] in PAYMENT_CATEGORIES:
                payment = f"payment field [{element.index}] ({found[1]})"
                break
    bot = None
    if BOT_CHECK_TITLES.search(title or ""):
        bot = f"the page title reads '{title}'"
    for frame in (*frame_urls, *(e.frame_url for e in elements)):
        # reCAPTCHA v3 and invisible hCaptcha load hidden frames on ordinary pages; they block nothing.
        if bot is None and is_bot_check_url(frame) and "invisible" not in frame.lower():
            bot = f"a bot-check frame is on the page ({host_of(frame) or frame})"
    checkout = None
    try:
        parts = urlsplit(url)
        path = f"{parts.path} {parts.query} {parts.fragment}"
    except ValueError:
        path = url
    where = normalize(split_identifier(path) + " " + (title or ""))
    match = _CHECKOUT_WORDS.search(where)
    if match:
        checkout = f"the page reads as a checkout step ('{match.group(0)}')"
    else:
        for element in elements:
            tier = control_tier(element)
            if tier is not ControlTier.NONE:
                checkout = f"[{element.index}] reads as a {tier.value} control"
                break
            if is_typeable(element) and sensitive_field(element):
                checkout = f"[{element.index}] is a sensitive field"
                break
    return PageCheck(payment_page=payment, bot_check=bot, checkout=checkout)


def dialog_should_accept(dialog_type: str) -> bool:
    """JavaScript dialogs: only a plain alert is accepted (it is informational).
    confirm(), prompt() and beforeunload are dismissed, so a "Confirm payment?"
    or "Leave this page?" never goes through on the agent's behalf. This
    replaces browser-use's PopupsWatchdog, which accepts confirm() and
    beforeunload."""
    return dialog_type == "alert"
