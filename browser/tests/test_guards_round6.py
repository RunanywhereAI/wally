"""Round-6 review (adversarial, guards only): clicks converged under checkout mode; these
are the typing-path findings and the stickiness gap, pinned."""

import asyncio
import contextvars
from types import SimpleNamespace

import pytest

import wally_browser  # noqa: F401
from wally_browser import tools as tools_module
from wally_browser.agent import EveAgent
from wally_browser.elements import _frame_url
from wally_browser.guards import Element, check_page, control_tier, ControlTier, refused_value
from wally_browser.policy import Decision, Observation
from wally_browser.profile import Profile, key_fits_field
from wally_browser.tools import GuardContext, build_tools, disable_page_typing


def field(name="", **kw):
    kw.setdefault("tag", "input")
    return Element(index=1, name=name, **kw)


def node(name, tag="BUTTON", role="button", attributes=None):
    return SimpleNamespace(node_name=tag, attributes=attributes or {}, ax_node=SimpleNamespace(role=role, name=name),
                           parent_node=None, children_nodes=[], shadow_roots=[], node_type=1, node_value="",
                           target_id="t", backend_node_id=1, session_id=None, frame_id=None)


class Session:
    def __init__(self, n):
        self.n = n

    async def get_element_by_index(self, index):
        return self.n


@pytest.fixture(autouse=True)
def unreadable_live_text(monkeypatch):
    async def none(session, n):
        return None

    monkeypatch.setattr(tools_module, "live_text", none)


# 1. a frame injected without a src is still someone else's frame
def test_a_srcless_iframe_counts_as_a_frame():
    frame = SimpleNamespace(node_name="IFRAME", attributes={"name": "intercom-messenger-frame"}, parent_node=None)
    inside = SimpleNamespace(parent_node=frame)
    assert _frame_url(inside) == "about:blank"


def agent_value(target, profile, value_key, used=None):
    fake = SimpleNamespace(_wally_profile=profile, _wally_text=None, _wally_plan="", _wally_notes=[],
                           _wally_guard=GuardContext(), _wally_used_keys=used if used is not None else {})
    obs = Observation(goal="book", url="https://shop.example/", title="Shop", elements=[target])
    decision = Decision(operation="TYPE", target=target, value_key=value_key)
    return asyncio.run(EveAgent._value_for(fake, decision, obs, SimpleNamespace(text_ms=0)))


def test_a_profile_email_never_goes_into_a_srcless_widget_frame():
    widget_email = Element(index=7, tag="input", name="Email", frame_url="about:blank")
    assert agent_value(widget_email, Profile(values={"email": "traveller@example.com"}), "email") == (None, "")


# 2. another traveller's slot, however it is named, and the one-field-per-detail rule
@pytest.mark.parametrize("label", ["passenger (2) first name", "traveller-2 first name", "second passenger first name",
                                   "additional driver first name", "co-traveller first name"])
def test_other_traveller_slots_do_not_take_profile_values(label):
    assert not key_fits_field("first_name", label)


def test_a_detail_goes_into_one_field_per_run():
    profile = Profile(values={"first_name": "Test"})
    used = {}
    first = Element(index=3, tag="input", name="First name")
    second = Element(index=17, tag="input", name="First name")  # e.g. passenger 2, named only in a heading
    assert agent_value(first, profile, "first_name", used) == ("Test", "profile:first_name")
    assert agent_value(first, profile, "first_name", used) == ("Test", "profile:first_name")  # same field again
    assert agent_value(second, profile, "first_name", used) == (None, "")


# 3. OTP codes: no allow-listing by substring, 7-8 digit codes too
@pytest.mark.parametrize("value,element", [("482913", field("Account verification")),
                                           ("48291375", field("Verification")), ("4829", field("Discount code"))])
def test_codes_are_refused_in_lookalike_fields(value, element):
    assert refused_value(value, element)


def test_an_eight_digit_landline_still_goes_into_a_phone_field():
    assert refused_value("24356789", field("Landline", input_type="tel")) is None


# 4. card numbers with spaced or unusual separators
@pytest.mark.parametrize("value", ["4111 - 1111 - 1111 - 1111", "4111 / 1111 / 1111 / 1111", "4111–1111–1111–1111"])
def test_card_numbers_with_any_separators_are_refused(value):
    assert refused_value(value, field())


# 5. a y/N answered at the guard keeps confirmation on for the rest of the run
def test_a_guard_answer_counts_as_using_the_browser():
    context = GuardContext()
    tools = build_tools(ask=lambda q: "n", context=context)
    asyncio.run(tools.registry.execute_action("click", {"index": 2}, browser_session=Session(node("Book now"))))
    assert context.person_used_browser and context.confirm_reason()


# 6. ARIA options are rated by the item browser-use will click; unreadable options ask
def test_an_aria_pay_option_reached_by_data_value_is_refused(monkeypatch):
    async def shown(session, n, wanted):
        return "Standard – auto-renew ₹999/month"

    monkeypatch.setattr(tools_module, "option_text", shown)
    tools = build_tools(ask=lambda q: None, context=GuardContext())
    result = asyncio.run(tools.registry.execute_action(
        "select_dropdown", {"index": 4, "text": "standard"},
        browser_session=Session(node("Fare", tag="DIV", role="listbox"))))
    assert result.error and "stops before payment" in result.error


def test_an_option_that_cannot_be_read_asks(monkeypatch):
    async def unreadable(session, n, wanted):
        return None

    monkeypatch.setattr(tools_module, "option_text", unreadable)
    asked = []
    tools = build_tools(ask=lambda q: asked.append(q), context=GuardContext())
    asyncio.run(tools.registry.execute_action("select_dropdown", {"index": 4, "text": "x"},
                                              browser_session=Session(node("Size", tag="SELECT", role="combobox"))))
    assert asked and "could not be read" in asked[0]


# 8. no click inside a typing event (browser-use's fallback clicks before it types)
def test_the_typing_fallback_cannot_click():
    from browser_use.browser.watchdogs.default_action_watchdog import DefaultActionWatchdog

    disable_page_typing()
    assert DefaultActionWatchdog.on_TypeTextEvent.__name__ == "on_TypeTextEvent"
    tools_module._TYPING.set(True)
    try:
        with pytest.raises(RuntimeError, match="no click while typing"):
            asyncio.run(DefaultActionWatchdog._click_element_node_impl(None, None))
    finally:
        tools_module._TYPING.set(False)


# 9. (low) European order and booking words, and checkout URLs
@pytest.mark.parametrize("name", ["Kostenpflichtig bestellen", "Jetzt bestellen"])
def test_german_order_buttons_are_payment(name):
    assert control_tier(Element(index=1, tag="button", name=name)) is ControlTier.PAYMENT


@pytest.mark.parametrize("name", ["Prenota ora", "Tisch reservieren"])
def test_european_booking_buttons_need_the_person(name):
    assert control_tier(Element(index=1, tag="button", name=name)) is not ControlTier.NONE


@pytest.mark.parametrize("url", ["https://www.shop.de/kasse/uebersicht", "https://www.shop.de/warenkorb",
                                 "https://www.site.fr/panier", "https://www.site.it/pagamento"])
def test_european_checkout_urls_start_checkout_mode(url):
    assert check_page(url, "Shop", []).checkout
