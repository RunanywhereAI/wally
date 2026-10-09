"""The hard stops: what is never typed into, never clicked, and when a run stops."""

import pytest

from wally_browser.guards import (
    ControlTier,
    Element,
    check_page,
    control_tier,
    dialog_should_accept,
    never_type_reason,
)


def field(index=1, **kw):
    kw.setdefault("tag", "input")
    return Element(index=index, **kw)


def button(name, index=1, **kw):
    return Element(index=index, tag="button", name=name, **kw)


@pytest.mark.parametrize("element", [
    field(input_type="password", name="Password"),
    field(autocomplete="cc-number", name="Number"),
    field(autocomplete="cc-exp", name="MM/YY"),
    field(autocomplete="section-payment cc-csc", name="Code"),
    field(autocomplete="one-time-code", name="Code"),
    field(name="Card Number"),
    field(name="CVV"),
    field(placeholder="Expiry (MM/YY)"),
    field(name="Enter UPI ID"),
    field(name="UPI PIN"),
    field(name="Enter OTP"),
    field(name="One-time password"),
    field(name="Name on card", attributes={"name": "card_holder"}),
    field(name="Name", frame_url="https://api.razorpay.com/v1/checkout/embedded"),
    field(name="Holder", frame_url="https://js.stripe.com/v3/elements-inner-card.html"),
])
def test_never_type_into_payment_or_secret_fields(element):
    assert never_type_reason(element) is not None


@pytest.mark.parametrize("element", [
    field(name="First name", autocomplete="given-name"),
    field(name="Email", input_type="email", autocomplete="email"),
    field(name="Where to?", input_type="text"),
    field(name="Passport number"),
    field(name="Date of birth", input_type="date"),
])
def test_traveller_fields_may_be_typed(element):
    assert never_type_reason(element) is None


@pytest.mark.parametrize("name", [
    "Pay", "Pay now", "PAY NOW", "Pay ₹4,532", "Pay Rs. 4532", "Pay securely", "Make payment",
    "Place order", "Purchase", "Buy now", "Complete booking", "Complete purchase", "Confirm and pay",
    "Confirm & Pay", "Confirm payment", "Submit payment",
])
def test_payment_controls_are_never_clicked(name):
    assert control_tier(button(name)) is ControlTier.PAYMENT


@pytest.mark.parametrize("name", ["Book now", "BOOK", "Confirm booking", "Reserve", "Confirm", "Checkout"])
def test_commit_controls_need_the_user(name):
    assert control_tier(button(name)) is ControlTier.COMMIT


@pytest.mark.parametrize("name", [
    "Search", "Continue", "Continue to payment", "Proceed to payment", "Select", "Window seat 12A",
    "Show flights", "Next", "Add baggage",
])
def test_ordinary_and_navigating_controls_are_allowed(name):
    assert control_tier(button(name)) is ControlTier.NONE


def test_a_button_inside_a_gateway_frame_is_payment_whatever_it_says():
    assert control_tier(button("Continue", frame_url="https://checkout.razorpay.com/v1/x")) is ControlTier.PAYMENT


def test_card_fields_make_the_payment_page():
    page = check_page("https://www.example-air.in/payment", "Payment", [
        field(1, name="Card number", autocomplete="cc-number"), button("Pay ₹4,532", 2)])
    assert page.payment_page and ("card" in page.payment_page.lower() or "cc-" in page.payment_page)


def test_a_gateway_frame_makes_the_payment_page():
    page = check_page("https://www.example-air.in/review", "Review", [],
                      frame_urls=["https://api.razorpay.com/v1/checkout/public"])
    assert page.payment_page and "razorpay" in page.payment_page


def test_a_gateway_origin_makes_the_payment_page():
    assert check_page("https://secure.payu.in/_payment", "PayU", []).payment_page


def test_a_passenger_form_is_not_the_payment_page():
    page = check_page("https://www.example-air.in/passengers", "Traveller details", [
        field(1, name="First name"), field(2, name="Email", input_type="email"), button("Continue", 3)])
    assert page.payment_page is None and page.bot_check is None


@pytest.mark.parametrize("frame", [
    "https://www.google.com/recaptcha/api2/anchor?k=x", "https://newassets.hcaptcha.com/captcha/v1/x",
    "https://challenges.cloudflare.com/cdn-cgi/challenge-platform/x",
])
def test_bot_check_frames_are_handed_to_the_user(frame):
    assert check_page("https://www.example-air.in/", "Flights", [], frame_urls=[frame]).bot_check


def test_a_cloudflare_interstitial_title_is_a_bot_check():
    assert check_page("https://www.example-air.in/", "Just a moment...", []).bot_check


@pytest.mark.parametrize("kind,accept", [("alert", True), ("confirm", False), ("prompt", False),
                                         ("beforeunload", False)])
def test_only_plain_alerts_are_accepted(kind, accept):
    assert dialog_should_accept(kind) is accept


# Review findings (code-review at high effort on M1), each probe pinned.

@pytest.mark.parametrize("name", [
    "Place order and proceed to payment", "Proceed to Pay", "Continue to pay", "Tap to pay", "₹4,500 Pay",
    "Submit order", "Order now", "Confirm order", "Pay later",
])
def test_review_probes_that_must_be_payment(name):
    assert control_tier(button(name)) is ControlTier.PAYMENT


def test_a_navigation_title_cannot_lower_a_pay_button():
    assert control_tier(button("Pay now", attributes={"title": "Go to payment"})) is ControlTier.PAYMENT


@pytest.mark.parametrize("name", ["Confirm booking & continue to payment", "Book and proceed to checkout"])
def test_a_navigation_phrase_inside_a_commit_label_stays_commit(name):
    assert control_tier(button(name)) is ControlTier.COMMIT


@pytest.mark.parametrize("name", ["Proceed to checkout", "Continue to payment →", "Go to review"])
def test_a_label_that_is_only_navigation_is_allowed(name):
    assert control_tier(button(name)) is ControlTier.NONE


def test_a_pay_word_past_the_display_clip_is_seen():
    long_name = ("Total 45,000 INR for 2 adults incl. taxes, fees and convenience charges (see fare rules) "
                 "Pay now")
    element = Element(index=1, tag="div", role="button", name=long_name[:79] + "…", full_text=long_name)
    assert control_tier(element) is ControlTier.PAYMENT


@pytest.mark.parametrize("element", [
    field(name="MM", tag="select"), field(name="YY", tag="select"), field(name="", attributes={"name": "exp_month"}),
    field(name="", attributes={"name": "ccnum"}), field(name="", attributes={"name": "cc_number"}),
    field(name="Card", placeholder="1234 5678 9012 3456"), field(name="Enter code"),
    field(name="Enter the 6-digit code sent to your phone"),
])
def test_review_probes_that_must_never_be_typed(element):
    assert never_type_reason(element) is not None


def test_a_date_of_birth_month_field_is_not_mistaken_for_card_expiry():
    assert never_type_reason(field(name="Month", attributes={"name": "birth_month"})) is None
