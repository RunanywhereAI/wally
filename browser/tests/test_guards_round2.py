"""Round-2 review (adversarial, guards only): every concrete bypass the reviewers
produced, pinned. Each case returned NONE / None / False before the fix."""

import asyncio
from types import SimpleNamespace

import pytest

import wally_browser  # noqa: F401
from wally_browser import tools as tools_module
from wally_browser.elements import _all_text, element_from_node, label
from wally_browser.guards import (
    ControlTier,
    Element,
    check_page,
    control_tier,
    is_payment_gateway,
    is_typeable,
    never_type_reason,
)
from wally_browser.profile import key_fits_field
from wally_browser.tools import GuardContext, build_tools


def button(name="", index=1, **kw):
    kw.setdefault("tag", "button")
    return Element(index=index, name=name, **kw)


def field(index=1, **kw):
    kw.setdefault("tag", "input")
    return Element(index=index, **kw)


# 1. Non-English, Unicode tricks and wider money words
@pytest.mark.parametrize("name", [
    "भुगतान करें", "अभी खरीदें", "Zahlungspflichtig bestellen", "Payer", "Pagar", "Paynow", "Pay100",
    "Pur­chase", "P​ay", "Ｐａｙ", "Subscribe", "Upgrade now", "Donate", "Recharge now", "Top up wallet",
    "Send money", "Place bid",
])
def test_money_words_in_any_form_are_payment(name):
    assert control_tier(button(name)) is ControlTier.PAYMENT


@pytest.mark.parametrize("name", ["Réserver", "Reservar", "Buchen", "बुक करें", "Confirmar"])
def test_book_words_in_other_languages_are_commit(name):
    assert control_tier(button(name)) is ControlTier.COMMIT


# 2. ids and names carry the meaning
@pytest.mark.parametrize("attributes", [
    {"id": "place-order-btn", "name": "placeOrder"}, {"id": "btnPayNow"}, {"id": "submit_order"},
])
def test_a_continue_button_named_for_payment_is_payment(attributes):
    assert control_tier(button("Continue", attributes=attributes)) is ControlTier.PAYMENT


@pytest.mark.parametrize("element", [
    field(input_type="tel", placeholder="•••", attributes={"name": "card_cvv"}),
    field(attributes={"id": "txtCVV"}),
    field(placeholder="example@okaxis", attributes={"name": "upi_id"}),
    field(attributes={"name": "vpa_address"}),
    field(attributes={"name": "atm_pin"}),
    field(input_type="number", attributes={"name": "otp1"}),
    field(attributes={"id": "otpInput"}),
    field(tag="select", attributes={"name": "card_exp_month"}),
])
def test_payment_fields_named_by_id_are_never_typed(element):
    assert never_type_reason(element) is not None


# 3. more card / OTP / expiry labels, and profile values kept out of payment forms
@pytest.mark.parametrize("element", [
    field(attributes={"aria-label": "Digit 1 of 6"}), field(name="Enter the code"), field(name="Valid until"),
    field(name="Exp."), field(name="Name (as on card)"), field(name="Kartennummer"), field(name="कार्ड नंबर"),
    field(name="CID"), field(placeholder="XXXX XXXX XXXX XXXX"), field(placeholder="Card #", attributes={"name": "ccn"}),
])
def test_more_payment_labels_are_never_typed(element):
    assert never_type_reason(element) is not None


def test_profile_values_stay_out_of_card_and_wallet_forms():
    assert not key_fits_field("full_name", "name (as on card)")
    assert not key_fits_field("phone", "paytm wallet mobile number")
    assert not key_fits_field("email", "billing email", "cc-email")


# 4. input into a button is refused (browser-use would click it)
def test_a_submit_input_is_not_a_field():
    assert not is_typeable(field(input_type="submit", value="Confirm booking"))
    assert is_typeable(field(input_type="text"))


class Node:
    def __init__(self, name, tag="BUTTON", attributes=None, ax_role="button"):
        self.node_name, self.attributes = tag, attributes or {}
        self.ax_node = SimpleNamespace(role=ax_role, name=name)
        self.parent_node, self.children_nodes, self.shadow_roots = None, [], []
        self.node_type, self.node_value = 1, ""
        self.target_id, self.backend_node_id = "t", 1


class Session:
    def __init__(self, node):
        self.node = node

    async def get_element_by_index(self, index):
        return self.node


def act(name, params, node, context=None, ask=lambda q: None):
    tools = build_tools(ask=ask, context=context or GuardContext())
    return asyncio.run(tools.registry.execute_action(name, params, browser_session=Session(node)))


def test_input_into_a_confirm_booking_submit_is_refused():
    node = Node("Confirm booking", tag="INPUT", attributes={"type": "submit", "value": "Confirm booking"})
    result = act("input", {"index": 3, "text": "x"}, node)
    assert result.error and "not a text field" in result.error


# 5. any card / UPI / bank field marks the payment page, role=textbox included
@pytest.mark.parametrize("element", [
    field(name="Security code"), field(name="MM/YY"), field(name="Valid thru"), field(name="VPA"),
    field(name="Net banking"), field(placeholder="1234 5678 9012 3456"),
    Element(index=1, tag="div", role="textbox", name="Card number"),
])
def test_single_payment_fields_mark_the_payment_page(element):
    assert check_page("https://merchant.example/checkout", "Checkout", [element]).payment_page


# 6. text the guards used to miss
def test_split_letters_and_image_alt_and_label_for_are_read():
    assert control_tier(button("P ay", full_text="P ay")) is ControlTier.PAYMENT
    assert control_tier(field(input_type="image", attributes={"alt": "Pay Now"})) is ControlTier.PAYMENT
    assert control_tier(Element(index=1, tag="label", name="Continue",
                                attributes={"for": "placeOrderBtn"})) is ControlTier.PAYMENT
    assert control_tier(button("shopping_cart_checkout", full_text="shopping_cart_checkout")) is ControlTier.PAYMENT


def test_a_link_to_a_commit_url_needs_the_person():
    assert control_tier(Element(index=1, tag="a", name="Continue",
                                attributes={"href": "/order/place?confirm=1"})) is not ControlTier.NONE


def test_shadow_root_text_and_long_tails_are_read():
    text = SimpleNamespace(node_type=3, node_value="Pay now", children_nodes=[], shadow_roots=[])
    shadow = SimpleNamespace(node_type=11, node_value="", children_nodes=[text], shadow_roots=[])
    host = SimpleNamespace(node_type=1, node_value="", children_nodes=[], shadow_roots=[shadow])
    assert "Pay now" in _all_text(host)
    long_card = Element(index=1, tag="div", role="button", name="Fare rules",
                        full_text=("fare rules " * 300)[:1500] + " … " + "Pay ₹4,500")
    assert control_tier(long_card) is ControlTier.PAYMENT


# 7. an in-page confirm modal's "Yes" is read in context
def test_yes_inside_a_pay_modal_is_payment():
    yes = button("Yes", context_text="Pay ₹4,500 from your saved card?")
    assert control_tier(yes) is ControlTier.PAYMENT
    ok = button("OK", context_text="Confirm your booking for 2 travellers?")
    assert control_tier(ok) is ControlTier.COMMIT


# 8. the click is checked against what the element says now
def test_a_button_that_re_rendered_into_pay_is_caught(monkeypatch):
    async def live(session, node):
        return "Pay ₹4,500"

    monkeypatch.setattr(tools_module, "live_text", live)
    result = act("click", {"index": 2}, Node("Continue"))
    assert result.error and "stops before payment" in result.error


# 9. more gateways, and links to them
@pytest.mark.parametrize("url", [
    "https://rzp.io/l/abc", "https://pay.google.com/gp/p/ui/pay", "https://checkout.pci.shopifyinc.com/build",
    "https://centinelapi.cardinalcommerce.com/V1/Cruise", "https://securegw.paytmpayments.com/theia",
    "https://payments.amazon.in/x", "https://www.sbiepay.sbi/x", "https://api.hyperswitch.io/x",
])
def test_more_gateways_are_payment(url):
    assert is_payment_gateway(url)


def test_a_link_to_a_gateway_is_payment():
    assert control_tier(Element(index=1, tag="a", name="Continue",
                                attributes={"href": "https://rzp.io/l/x"})) is ControlTier.PAYMENT


# 10. card numbers and addresses are masked in labels
@pytest.mark.parametrize("element", [
    field(placeholder="Card #", value="4111111111111111", attributes={"name": "ccn"}),
    field(name="Notes", value="3782 822463 10005"),
    field(name="Notes", value="4111-1111-1111-1111"),
    field(name="Flat, House no., Building", autocomplete="address-line1", value="12 MG Road"),
    field(name="Landmark", value="Near the park"),
])
def test_sensitive_values_are_masked(element):
    assert label(element).endswith("· filled") and element.value not in label(element)


# The structural line: on a checkout page and on the person's own Chrome, every click asks.
def test_on_a_checkout_page_an_unreadable_button_still_needs_the_person(monkeypatch):
    async def no_live(session, node):
        return None

    monkeypatch.setattr(tools_module, "live_text", no_live)
    icon = Node("", attributes={"class": "cta"})  # e.g. CSS-drawn "Pay ₹4,500" that cannot be read
    result = act("click", {"index": 7}, icon, context=GuardContext(checkout="the page reads as a checkout step"))
    assert result.is_done and result.success is False and "did not confirm" in result.error


def test_in_attach_mode_every_click_and_every_typed_value_needs_the_person(monkeypatch):
    async def no_live(session, node):
        return None

    monkeypatch.setattr(tools_module, "live_text", no_live)
    assert "did not confirm" in act("click", {"index": 1}, Node("Search"), GuardContext(attach=True)).error
    typed = act("input", {"index": 2, "text": "Goa"}, Node("To", tag="INPUT", ax_role="textbox"),
                GuardContext(attach=True))
    assert typed.error and "did not confirm typing" in typed.error
