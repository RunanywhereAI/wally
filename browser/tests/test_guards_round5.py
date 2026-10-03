"""Round-5 review (adversarial, guards only) and checkout mode: sticky once any
checkout-like page is reached, so a label the tiers miss cannot commit."""

import asyncio
from types import SimpleNamespace

import pytest

import wally_browser  # noqa: F401
from wally_browser import tools as tools_module
from wally_browser.agent import EveAgent
from wally_browser.guards import Element, check_page, details_form_reason, never_type_reason, refused_value
from wally_browser.policy import Decision, Observation
from wally_browser.profile import Profile, key_fits_field
from wally_browser.tools import GuardContext, build_tools


def field(name="", **kw):
    kw.setdefault("tag", "input")
    return Element(index=1, name=name, **kw)


# 1 / 8. book-a-table and /book/ pages, and details forms, are checkout-like
@pytest.mark.parametrize("url,title", [
    ("https://www.dineout.example/delhi/foo/book-a-table", "Book a table at Foo"),
    ("https://www.example.in/book/8812", "Foo"),
    ("https://www.example.in/cart", "Your bag"),
])
def test_booking_urls_start_checkout_mode(url, title):
    assert check_page(url, title, []).checkout


def test_a_guest_and_whatsapp_form_is_a_details_form():
    fields = [Element(index=1, tag="input", name="Guest"), Element(index=2, tag="input", name="WhatsApp number")]
    assert details_form_reason(fields)
    assert check_page("https://foo.example/x", "Foo", fields).checkout


def test_a_search_form_is_not_a_details_form():
    search = [Element(index=1, tag="input", name="From"), Element(index=2, tag="input", name="To"),
              Element(index=3, tag="input", name="Departure date")]
    assert details_form_reason(search) is None
    assert check_page("https://www.google.com/travel/flights", "Google Flights", search).checkout is None


def test_checkout_mode_asks_for_every_click_whatever_its_label(monkeypatch):
    async def none(session, node):
        return None

    monkeypatch.setattr(tools_module, "live_text", none)
    context = GuardContext(checkout="a form with 2 detail fields")
    tools = build_tools(ask=lambda q: None, context=context)
    node = SimpleNamespace(node_name="BUTTON", attributes={}, ax_node=SimpleNamespace(role="button", name="Done"),
                           parent_node=None, children_nodes=[], shadow_roots=[], node_type=1, node_value="",
                           target_id="t", backend_node_id=1, session_id=None, frame_id=None)

    class Session:
        async def get_element_by_index(self, index):
            return node

    result = asyncio.run(tools.registry.execute_action("click", {"index": 9}, browser_session=Session()))
    assert result.is_done and "did not confirm" in result.error
    assert context.confirm_reason().startswith("checkout mode")


# 2. the text model never fills personal-looking values or framed fields
class FakeText:
    def __init__(self, value):
        self.value = value

    def field_text(self, *args):
        return self.value


def value_for(target, text_value, elements=None):
    fake = SimpleNamespace(_wally_profile=Profile(values={}), _wally_text=FakeText(text_value), _wally_plan="",
                           _wally_notes=[], _wally_guard=GuardContext())
    obs = Observation(goal="book", url="https://foo.example/", title="Foo", elements=elements or [target])
    decision = Decision(operation="TYPE", target=target)
    record = SimpleNamespace(text_ms=0)
    return asyncio.run(EveAgent._value_for(fake, decision, obs, record))


def test_a_personal_looking_model_value_goes_to_the_person():
    assert value_for(Element(index=1, tag="textarea", name="Write a message"), "asha@example.com") == (None, "")


def test_a_model_value_for_a_framed_field_goes_to_the_person():
    framed = Element(index=1, tag="textarea", name="Write a message", frame_url="https://widget.intercom.io/x")
    assert value_for(framed, "Hello") == (None, "")


def test_a_destination_still_comes_from_the_model():
    assert value_for(Element(index=1, tag="input", name="To"), "Bangalore") == ("Bangalore", "text-model")


# 3. a select is rated by the option browser-use will really pick
def test_a_pay_option_reached_by_its_value_is_refused(monkeypatch):
    async def shown(session, node, wanted):
        return "Flexi — pay ₹1,200 now"

    monkeypatch.setattr(tools_module, "option_text", shown)
    tools = build_tools(ask=lambda q: None, context=GuardContext())
    node = SimpleNamespace(node_name="SELECT", attributes={}, ax_node=SimpleNamespace(role="combobox", name="Fare"),
                           parent_node=None, children_nodes=[], shadow_roots=[], node_type=1, node_value="",
                           target_id="t", backend_node_id=1, session_id=None, frame_id=None)

    class Session:
        async def get_element_by_index(self, index):
            return node

    result = asyncio.run(tools.registry.execute_action("select_dropdown", {"index": 5, "text": "flexi"},
                                                       browser_session=Session()))
    assert result.error and "stops before payment" in result.error


# 4 / 5 / 6. values: UPI handles, OTP codes, card numbers in any shape
@pytest.mark.parametrize("value", ["asha@icici", "asha@sbi", "asha@kotak", "9876543210@axisb", "asha@ikwik",
                                   "asha@freecharge"])
def test_every_upi_handle_is_refused(value):
    assert refused_value(value, field())


def test_an_email_is_not_a_upi_id():
    assert refused_value("asha@example.com", field("Email", input_type="email")) is None


@pytest.mark.parametrize("value,element", [
    ("482 913", field()), ("482913", field(input_type="tel", attributes={"aria-label": "Verify mobile"})),
    ("4829", field(input_type="tel")),
])
def test_codes_are_refused_however_they_are_written(value, element):
    assert refused_value(value, element)


@pytest.mark.parametrize("element", [field(attributes={"maxlength": "1"}), field(attributes={"aria-label": "Digit 1"})])
def test_one_digit_code_boxes_are_never_typed(element):
    assert never_type_reason(element)


@pytest.mark.parametrize("value", ["4111.1111.1111.1111", "4111/1111/1111/1111", "Card: 4111 1111 1111 1111"])
def test_card_numbers_in_any_shape_are_refused(value):
    assert refused_value(value, field())


@pytest.mark.parametrize("value,element", [("560001", field("PIN code")), ("9876543210", field("Mobile", input_type="tel")),
                                           ("2", field("Adults"))])
def test_real_numbers_still_go_in(value, element):
    assert refused_value(value, element) is None


# 7. answering any prompt counts as having had the window
def test_answering_ask_user_counts_as_using_the_browser():
    context = GuardContext()
    tools = build_tools(ask=lambda q: "done, continue", context=context)
    asyncio.run(tools.registry.execute_action("ask_user", {"question": "What next?"}))
    assert context.person_used_browser and "signed in" in context.confirm_reason()


# 9. numbered co-traveller slots are asked about, not filled from the profile
@pytest.mark.parametrize("label", ["passengers 1 first name", "guest #2 first name", "first name (room 2)",
                                   "adult 2 first name"])
def test_numbered_traveller_slots_do_not_take_profile_values(label):
    assert not key_fits_field("first_name", label)
