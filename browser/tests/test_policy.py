"""The eve policy's requests stay inside the decision API's limits, and its
answers map back to elements. The decision client is faked."""

import httpx
import pytest

from wally_browser import elements as el
from wally_browser.decisions import DecisionClient, DecisionError, Question, WindowExceeded
from wally_browser.guards import Element
from wally_browser.policy import EvePolicy, Observation, fields_typed_but_still_empty
from wally_browser.profile import Profile


def buttons(n, start=1):
    return [Element(index=i, tag="button", name=f"Button {i}") for i in range(start, start + n)]


class FakeClient:
    """Answers every choice with a chosen option (default: the first), records requests."""

    def __init__(self, picks=None, window_failures=0):
        self.requests = []
        self.picks = picks or {}
        self.window_failures = window_failures

    def ask(self, input_text, questions, temperature=None):
        from wally_browser.decisions import Answer, Result

        self.requests.append((input_text, questions))
        if self.window_failures:
            self.window_failures -= 1
            raise WindowExceeded("longer than the 8185 tokens the model accepts", 400)
        answers = {}
        for q in questions:
            if q.kind == "yes_no":
                answers[q.id] = Answer({"yes": 0.1, "no": 0.9})
                continue
            pick = self.picks.get(q.id)
            chosen = next((o for o in q.options if pick and pick(o)), q.options[0])
            answers[q.id] = Answer({o: (0.9 if o == chosen else 0.1 / (len(q.options) - 1)) for o in q.options})
        return Result(answers=answers, prompt_tokens=100, latency_ms=5, attempts=1)


def obs(elements, **kw):
    return Observation(goal="Find flights to Bangalore", url="https://example.test/", title="Flights",
                       elements=elements, **kw)


def test_every_question_has_2_to_26_options_and_fits_the_window():
    client = FakeClient()
    EvePolicy(client).decide(obs(buttons(300)))
    for input_text, questions in client.requests:
        for q in questions:
            if q.kind == "choice":
                assert 2 <= len(q.options) <= el.MAX_OPTIONS, (q.id, len(q.options))
            assert el.question_tokens(input_text, q.text, q.options) <= el.MAX_QUESTION_TOKENS


def test_more_than_26_targets_take_a_region_round_then_a_target_round():
    target = 137
    client = FakeClient(picks={
        "operation": lambda o: o.startswith("CLICK"),
        "click_target": lambda o: f"[{target}]" in o or (o.startswith("elements [") and
                                                          _in_range(o, target)),
    })
    decision = EvePolicy(client).decide(obs(buttons(300)))
    assert decision.operation == "CLICK"
    assert decision.target is not None and decision.target.index == target
    assert decision.rounds == 2
    assert decision.over_26["click_target"] == 300


def _in_range(option, index):
    first = int(option.split("[", 2)[1].split("]")[0])
    last = int(option.split("–[", 1)[1].split("]")[0])
    return first <= index <= last


def test_a_head_with_one_candidate_is_not_asked():
    client = FakeClient(picks={"operation": lambda o: o.startswith("TYPE")})
    elements = buttons(3) + [Element(index=9, tag="input", input_type="text", name="Where to?")]
    decision = EvePolicy(client).decide(obs(elements))
    asked = {q.id for _, qs in client.requests for q in qs}
    assert "type_target" not in asked
    assert decision.operation == "TYPE" and decision.target.index == 9


def test_a_field_typed_that_is_still_empty_is_not_offered_again():
    box = Element(index=16, tag="input", role="combobox", name="Where to")
    suggestion = Element(index=20, tag="button", role="option", name="Bengaluru (BLR)")
    history = ["typed into [16] Where to"]
    assert [element.index for element in fields_typed_but_still_empty(history, [box, suggestion])] == [16]
    client = FakeClient()
    EvePolicy(client).decide(obs([box, suggestion], history=history))
    state, questions = client.requests[0]
    operation = next(q for q in questions if q.id == "operation")
    assert not any(option.startswith("TYPE") for option in operation.options)
    assert any(option.startswith("CLICK") for option in operation.options)
    assert "did not stick" in state
    assert "Bengaluru" in state


def test_a_button_already_clicked_is_not_offered_again():
    opener = Element(index=3083, tag="button", name="All filters")
    inside = Element(index=4000, tag="button", role="radio", name="Nonstop only",
                     context_text="Filters Stops Nonstop only")
    client = FakeClient()
    decision = EvePolicy(client).decide(obs([opener, inside], history=["chose to click [3083] All filters"]))
    state, questions = client.requests[0]
    operation = next(q for q in questions if q.id == "operation")
    assert any(option.startswith("CLICK") for option in operation.options)
    assert "A dialog is open" in state
    assert decision.operation == "CLICK"
    assert decision.target is not None and decision.target.index == 4000


def test_a_field_that_kept_its_value_can_still_be_typed():
    box = Element(index=16, tag="input", role="combobox", name="Where to", value="Bangalore")
    assert fields_typed_but_still_empty(["typed into [16] Where to"], [box]) == []


def test_payment_fields_are_never_offered_as_type_targets():
    client = FakeClient()
    elements = [Element(index=1, tag="input", name="Card number", autocomplete="cc-number"),
                Element(index=2, tag="input", name="CVV"),
                Element(index=3, tag="input", name="First name"),
                Element(index=4, tag="input", name="Last name")]
    EvePolicy(client).decide(obs(elements))
    offered = [o for _, qs in client.requests for q in qs if q.id == "type_target" for o in q.options]
    assert offered and all("Card" not in o and "CVV" not in o for o in offered)


def test_a_window_refusal_rebuilds_the_request_smaller():
    client = FakeClient(window_failures=2)
    decision = EvePolicy(client).decide(obs(buttons(20)))
    assert decision.window_retries == 2
    assert len(client.requests[2][0]) <= len(client.requests[0][0])


def test_the_value_head_maps_a_field_to_a_profile_key():
    profile = Profile(values={"first_name": "Asha", "last_name": "Rao"})
    client = FakeClient(picks={"operation": lambda o: o.startswith("TYPE"),
                               "type_target": lambda o: "Last name" in o,
                               "value": lambda o: o.startswith("last_name")})
    elements = [Element(index=1, tag="input", name="First name"), Element(index=2, tag="input", name="Last name")]
    decision = EvePolicy(client, profile).decide(obs(elements))
    assert decision.target.index == 2 and decision.value_key == "last_name"


def test_the_decision_client_retries_429_and_flags_a_window_refusal():
    calls = []

    def handler(request):
        calls.append(request)
        if len(calls) == 1:
            return httpx.Response(429, headers={"Retry-After": "0"}, json={"error": {"message": "busy"}})
        return httpx.Response(400, json={"error": {"code": "bad_request", "message":
                              "The input together with this question is longer than the 8185 tokens the model "
                              "accepts for one question."}})

    client = DecisionClient("https://example.test/v1", "k", "eve", transport=httpx.MockTransport(handler),
                            sleep=lambda s: None)
    with pytest.raises(WindowExceeded):
        client.ask("x", [Question("q", "yes_no", "ok?")])
    assert len(calls) == 2
    assert calls[0].headers["Authorization"] == "Bearer k"
    assert calls[0].url.path == "/v1/decisions"


def test_the_decision_client_reports_other_refusals():
    client = DecisionClient("https://example.test/v1", "k", "eve", transport=httpx.MockTransport(
        lambda r: httpx.Response(403, json={"error": {"code": "model_not_entitled", "message": "no"}})))
    with pytest.raises(DecisionError, match="HTTP 403"):
        client.ask("x", [Question("q", "yes_no", "ok?")])


def test_personal_and_sensitive_values_never_appear_in_labels():
    email = Element(index=4, tag="input", input_type="email", name="Email", value="someone@example.com")
    phone = Element(index=5, tag="input", input_type="tel", name="Mobile number", value="9000000000")
    name = Element(index=6, tag="input", name="First name", value="Asha")
    city = Element(index=7, tag="input", name="To", value="Bangalore")
    for element in (email, phone, name):
        assert el.label(element).endswith("· filled"), el.label(element)
        assert element.value not in el.label(element)
    assert el.label(city).endswith("· Bangalore")
