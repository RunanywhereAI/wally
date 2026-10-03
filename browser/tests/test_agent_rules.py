"""Agent-side rules from the M1 review: frames, outcomes fed back to eve, and
which profile values may go into which fields."""

from types import SimpleNamespace

import wally_browser  # noqa: F401
from wally_browser.agent import outcome_notes, page_frames
from wally_browser.guards import Element, check_page
from wally_browser.profile import key_fits_field


def node(name="DIV", children=(), src=None, visible=True, size=(300, 200), content=None):
    return SimpleNamespace(node_name=name, attributes={"src": src} if src else {}, is_visible=visible,
                           absolute_position=SimpleNamespace(width=size[0], height=size[1]),
                           children_nodes=list(children), shadow_roots=[], content_document=content)


def test_frames_come_from_this_page_with_visibility():
    tree = node(children=[
        node("IFRAME", src="https://www.google.com/recaptcha/api2/anchor?size=invisible", size=(0, 0)),
        node("IFRAME", src="https://checkout.razorpay.com/v1/x"),
        node(children=[node("IFRAME", src="https://challenges.cloudflare.com/cdn-cgi/x", visible=False)]),
    ])
    frames = page_frames(SimpleNamespace(_root=SimpleNamespace(original_node=tree)))
    assert ("https://checkout.razorpay.com/v1/x", True) in frames
    assert ("https://www.google.com/recaptcha/api2/anchor?size=invisible", False) in frames
    assert ("https://challenges.cloudflare.com/cdn-cgi/x", False) in frames


def test_an_invisible_recaptcha_anchor_is_not_a_bot_check():
    page = check_page("https://shop.example/", "Shop", [],
                      ["https://www.google.com/recaptcha/api2/anchor?ar=1&k=x&size=invisible"])
    assert page.bot_check is None


def test_stripes_fraud_detection_frame_is_not_a_payment_page():
    assert check_page("https://shop.example/", "Shop", [], ["https://m.stripe.network/inner.html"]).payment_page is None


def test_a_declined_click_reaches_eve_and_is_not_offered_again():
    target = Element(index=12, tag="button", name="Book now")
    notes, refused = outcome_notes([SimpleNamespace(
        error="the person did not confirm clicking [12] 'Book now'; do not try it again", extracted_content=None)],
        target)
    assert any("did not confirm" in n for n in notes) and refused == {"Book now"}


def test_the_persons_answer_reaches_eve():
    notes, refused = outcome_notes([SimpleNamespace(
        error=None, extracted_content="The person answered 'Which flight?': 6E 201")], None)
    assert notes == ["The person answered 'Which flight?': 6E 201"] and refused == set()


def test_profile_values_only_go_into_matching_fields():
    assert key_fits_field("first_name", "first name", "given-name")
    assert key_fits_field("passport_number", "passport no.")
    assert key_fits_field("email", "email address")
    assert not key_fits_field("passport_number", "promo code")
    assert not key_fits_field("email", "search destination")
    assert not key_fits_field("phone", "first name", "given-name")
