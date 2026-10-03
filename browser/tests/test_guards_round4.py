"""Round-4 review (adversarial, guards only): the "details entered" line follows where a
value came from, values are checked as well as fields, and the remaining wording gaps."""

from types import SimpleNamespace

import pytest

import wally_browser  # noqa: F401
from wally_browser.agent import is_details_form, looks_personal, note_value_source
from wally_browser.guards import Element, check_page, never_type_reason, refused_value
from wally_browser.tools import GuardContext


def field(name="", **kw):
    kw.setdefault("tag", "input")
    return Element(index=1, name=name, **kw)


# 1/2/5. the flag follows the value's source, not the field's label
@pytest.mark.parametrize("source,value", [("person", "Asha Rao"), ("profile:title", "Ms"),
                                          ("profile:home_city", "Pune"), ("text-model", "asha@example.com"),
                                          ("text-model", "9876543210")])
def test_profile_person_or_personal_looking_values_mark_details_entered(source, value):
    guard = GuardContext()
    note_value_source(guard, source, value)
    assert guard.personal_typed and guard.confirm_reason()


def test_an_ordinary_model_value_does_not_mark_details_entered():
    guard = GuardContext()
    note_value_source(guard, "text-model", "Bangalore")
    assert not guard.personal_typed


def test_a_reservation_form_with_unlabelled_fields_is_a_details_form():
    unlabeled = [field(attributes={"id": "f1"}), field(attributes={"id": "f2"}), field(attributes={"id": "f3"})]
    assert is_details_form(unlabeled)
    assert is_details_form([field("Guest name")])
    assert not is_details_form([field("Where to?")])
    assert looks_personal("asha@example.com") and looks_personal("+91 98765") and not looks_personal("Goa")


# 4/7. values are checked, not only fields
@pytest.mark.parametrize("value,element", [
    ("4111 1111 1111 1111", field(attributes={"id": "f1"})),
    ("asha@okaxis", field(attributes={"id": "f1"})),
    ("482913", field(attributes={"id": "f3"})),
])
def test_payment_values_are_refused_in_any_field(value, element):
    assert refused_value(value, element)


@pytest.mark.parametrize("value,element", [
    ("560001", field("PIN code")), ("9876543210", field("Mobile", input_type="tel")), ("2", field("Adults")),
    ("Bangalore", field("To")), ("traveller@example.com", field("Email", input_type="email")),
])
def test_ordinary_values_still_go_in(value, element):
    assert refused_value(value, element) is None


def test_a_hyphenated_otp_label_is_never_typed():
    assert never_type_reason(field("Enter 6-digit code"))


# 3. reservation and appointment pages are checkout pages
@pytest.mark.parametrize("url,title", [
    ("https://foo.example/reserve?date=2026-10-05&party=2", "Reserve a table at Foo"),
    ("https://foo.example/appointments/new", "Schedule an appointment"),
    ("https://foo.example/register", "Sign up"),
])
def test_reservation_pages_are_checkout_pages(url, title):
    assert check_page(url, title, []).checkout


# 6. after the person has had the window, every click asks
def test_after_a_hand_off_every_click_needs_the_person():
    guard = GuardContext(person_used_browser=True)
    assert guard.confirm_reason() and "signed in" in guard.confirm_reason()
