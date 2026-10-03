"""Guards against real pages' markup, captured with tests/capture_fixture.py through
the agent's own pipeline (browser-use page state → element_from_node). Public
payment-demo and documentation pages only; no personal data in any fixture.

The payment-form demo must stop; the documentation and marketing pages around
it must not trip a false payment stop, payment control or bot check.
"""

import json
from pathlib import Path

import pytest

from wally_browser.guards import ControlTier, Element, check_page, control_tier, is_typeable, never_type_reason

FIXTURES = Path(__file__).parent / "fixtures" / "real"


def load(name):
    document = json.loads((FIXTURES / f"{name}.json").read_text())
    return document, [Element(**e) for e in document["elements"]]


def by_name(elements, text):
    return [e for e in elements if text.lower() in (e.name or "").lower()]


def page_of(document, elements):
    """As the agent checks it: every frame for the payment stop, visible frames for bot checks."""
    payment = check_page(document["url"], document["title"], elements, document["frames"])
    bot = check_page(document["url"], document["title"], [], document["visible_frames"])
    return payment.payment_page, bot.bot_check


def test_a_real_stripe_card_form_is_the_payment_page():
    document, elements = load("stripe-payments-demo")
    payment, bot = page_of(document, elements)
    assert payment, "the card form was not detected as the payment page"
    assert bot is None, bot  # Stripe's hCaptcha frames are invisible
    card = [e for e in elements if e.autocomplete.startswith("cc-")]
    assert card and all(never_type_reason(e) for e in card)
    assert all(control_tier(e) is ControlTier.PAYMENT for e in by_name(elements, "Pay securely"))


def test_the_merchants_own_address_fields_are_still_typeable():
    _, elements = load("stripe-payments-demo")
    merchant = [e for e in elements if is_typeable(e) and not e.frame_url]
    names = {e.name for e in merchant}
    assert {"Name", "Email", "City"} <= names, names
    assert all(never_type_reason(e) is None for e in merchant if e.name in ("Name", "Email", "City", "ZIP"))


@pytest.mark.parametrize("name", ["stripe-checkout-preview", "braintree-hosted-fields"])
def test_marketing_and_demo_index_pages_are_not_payment_pages(name):
    document, elements = load(name)
    payment, bot = page_of(document, elements)
    assert payment is None and bot is None, (payment, bot)


def test_documentation_links_are_not_payment_controls():
    _, elements = load("stripe-elements-examples")
    for text in ("Sign in", "Create account", "register", "Overview"):
        for element in by_name(elements, text):
            assert control_tier(element) is not ControlTier.PAYMENT, (text, element.attributes.get("href"))


def test_invisible_captcha_frames_on_a_docs_page_are_not_a_bot_check():
    document, elements = load("stripe-elements-examples")
    _, bot = page_of(document, elements)
    assert bot is None, bot
