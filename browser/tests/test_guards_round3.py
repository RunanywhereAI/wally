"""Round-3 review (adversarial, guards only): every concrete bypass pinned, plus
the structural rules that stop what labels cannot."""

import asyncio
from types import SimpleNamespace

import pytest

import wally_browser  # noqa: F401
from wally_browser import tools as tools_module
from wally_browser.elements import _dialog_context
from wally_browser.guards import ControlTier, Element, check_page, control_tier, is_typeable
from wally_browser.profile import key_fits_field
from wally_browser.run import fresh_profile_dir
from wally_browser.tools import GuardContext, build_tools, clean_text


def button(name, **kw):
    kw.setdefault("tag", "button")
    return Element(index=1, name=name, **kw)


# 1. booking verbs and price-only buttons
@pytest.mark.parametrize("name", ["Schedule Event", "Request UberGo", "Register", "Get tickets", "RSVP",
                                  "Cancel my booking"])
def test_booking_verbs_need_the_person(name):
    assert control_tier(button(name)) is not ControlTier.NONE


@pytest.mark.parametrize("name", ["Rent movie HD ₹119", "₹99", "$4.99"])
def test_price_buttons_are_payment(name):
    assert control_tier(button(name)) is ControlTier.PAYMENT


# 3 / 5. dialogs: any button in a money dialog, "Cancel" in a fee dialog
@pytest.mark.parametrize("name,context", [
    ("Yes, continue", "Pay ₹4,500 from your wallet?"),
    ("OK", "Your card ending 4242 will be charged ₹999 every month."),
    ("Continue", "Your plan will auto-renew at ₹299."),
])
def test_affirmations_in_money_dialogs_are_payment(name, context):
    assert control_tier(button(name, context_text=context)) is ControlTier.PAYMENT


def test_cancel_in_a_cancellation_fee_dialog_needs_the_person():
    cancel = button("Cancel", context_text="Cancel this booking? A ₹3,000 fee applies.")
    assert control_tier(cancel) is not ControlTier.NONE


def test_a_negative_answer_in_a_money_dialog_still_asks_rather_than_passing():
    assert control_tier(button("No thanks", context_text="Pay ₹4,500 now?")) is ControlTier.COMMIT


def test_custom_modals_without_role_dialog_give_context():
    text = SimpleNamespace(node_type=3, node_value="Pay ₹4,500 from your saved card?", children_nodes=[],
                           shadow_roots=[])
    modal = SimpleNamespace(node_type=1, node_name="DIV", attributes={"class": "booking-modal open"},
                            children_nodes=[text], shadow_roots=[], parent_node=None)
    yes = SimpleNamespace(node_type=1, node_name="BUTTON", attributes={}, parent_node=modal,
                          children_nodes=[], shadow_roots=[])
    assert "saved card" in _dialog_context(yes)


# 7. hash-routed checkout steps
def test_a_hash_routed_checkout_is_a_checkout_page():
    assert check_page("https://app.example.in/#/checkout/payment", "Example", []).checkout


# 9. a role on a button does not make it a text field
@pytest.mark.parametrize("element", [
    Element(index=1, tag="button", role="combobox", name="From"),
    Element(index=1, tag="div", role="textbox", name="Fare card"),
    Element(index=1, tag="a", role="searchbox", name="Search"),
])
def test_aria_roles_on_buttons_are_not_typeable(element):
    assert not is_typeable(element)


def test_newlines_never_reach_the_keyboard():
    assert clean_text("Flat 4\nMG Road\r\n\tBlock B") == "Flat 4 MG Road Block B"


# 10. the traveller's details stay in the traveller's own fields
@pytest.mark.parametrize("key,label", [
    ("email", "get deals in your inbox: enter your email"), ("full_name", "gift recipient name"),
    ("date_of_birth", "place of birth"), ("passport_number", "passport expiry date"),
    ("first_name", "adult 2 first name"), ("phone", "emergency contact phone"),
])
def test_profile_values_stay_out_of_other_peoples_and_marketing_fields(key, label):
    assert not key_fits_field(key, label)


@pytest.mark.parametrize("key,label", [
    ("email", "email address"), ("date_of_birth", "date of birth"), ("passport_number", "passport number"),
    ("first_name", "first name"),
])
def test_profile_values_still_fill_the_travellers_own_fields(key, label):
    assert key_fits_field(key, label)


# 2. a fresh profile per run
def test_each_run_gets_a_fresh_browser_profile():
    first, second = fresh_profile_dir(), fresh_profile_dir()
    assert first != second and not any(first.iterdir()) and not any(second.iterdir())


# The structural rules, through the real action guard.
class Node:
    def __init__(self, name, tag="BUTTON", attributes=None, ax_role="button"):
        self.node_name, self.attributes = tag, attributes or {}
        self.ax_node = SimpleNamespace(role=ax_role, name=name)
        self.parent_node, self.children_nodes, self.shadow_roots = None, [], []
        self.node_type, self.node_value = 1, ""
        self.target_id, self.backend_node_id, self.session_id, self.frame_id = "t", 1, None, None


class Session:
    def __init__(self, node):
        self.node = node

    async def get_element_by_index(self, index):
        return self.node


@pytest.fixture(autouse=True)
def unreadable_live_text(monkeypatch):
    async def none(session, node):
        return None

    monkeypatch.setattr(tools_module, "live_text", none)


def run_action(tools, name, params, node):
    return asyncio.run(tools.registry.execute_action(name, params, browser_session=Session(node)))


def test_after_a_personal_detail_is_typed_every_click_needs_the_person():
    context = GuardContext()
    tools = build_tools(ask=lambda q: None, context=context)
    email = Node("Email", tag="INPUT", attributes={"type": "email"}, ax_role="textbox")
    assert context.personal_typed is False
    # The guard lets the input through (it is the traveller's own email field) and records that a
    # personal detail is now on the page; the typing itself needs a real browser, so it errors here.
    with pytest.raises(RuntimeError):
        run_action(tools, "input", {"index": 2, "text": "traveller@example.com"}, email)
    assert context.personal_typed is True
    result = run_action(tools, "click", {"index": 3}, Node("Continue"))  # a label the tiers call NONE
    assert result.is_done and "did not confirm" in result.error


def test_a_payment_option_in_a_dropdown_is_refused_and_any_option_on_a_checkout_page_asks():
    tools = build_tools(ask=lambda q: None, context=GuardContext(checkout="checkout step"))
    combobox = Node("Payment option", tag="DIV", ax_role="combobox")
    paid = run_action(tools, "select_dropdown", {"index": 4, "text": "Pay ₹4,500 from wallet balance"}, combobox)
    assert paid.error and "stops before payment" in paid.error
    other = run_action(tools, "select_dropdown", {"index": 4, "text": "Window seat"}, combobox)
    assert other.is_done and "did not confirm" in other.error


def test_in_attach_mode_even_cancel_needs_the_person():
    tools = build_tools(ask=lambda q: None, context=GuardContext(attach=True))
    result = run_action(tools, "click", {"index": 5}, Node("Cancel"))
    assert result.is_done and "did not confirm" in result.error


def test_an_unreadable_element_inside_a_frame_needs_the_person():
    asked = []
    tools = build_tools(ask=lambda q: asked.append(q), context=GuardContext())
    framed = Node("Next")
    frame = SimpleNamespace(node_name="IFRAME", attributes={"src": "https://booking-widget.example/x"},
                            parent_node=None)
    framed.parent_node = frame
    result = run_action(tools, "click", {"index": 6}, framed)
    assert result.is_done and "did not confirm" in result.error
    assert asked and "could not be read" in asked[0]
