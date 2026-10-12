"""The loop's own stops. These do not start Chrome."""

import time

from jev_ultrafast.agent import Agent, suppress_repeated
from jev_ultrafast.guards import refuses
from jev_ultrafast.model import EXPLICIT_NULL, chosen_target, parse_field_value


def test_a_repeat_is_forgotten_after_six_actions():
    history = [{"action": "Mumbai", "text": None, "page_changed": True}] * 2
    history += [{"action": f"step {i}", "text": None, "page_changed": True} for i in range(6)]
    actions = [{"label": "Mumbai"}, {"label": "Search"}]
    assert [a["label"] for a in suppress_repeated(actions, history)] == ["Mumbai", "Search"]


def test_a_recent_repeat_is_dropped():
    history = [{"action": "Mumbai", "text": None, "page_changed": True} for _ in range(2)]
    actions = [{"label": "Mumbai"}, {"label": "Search"}]
    assert [a["label"] for a in suppress_repeated(actions, history)] == ["Search"]


def test_the_checkout_pay_button_is_refused():
    page = {"text": "Pay ₹4,532", "url": "http://127.0.0.1/payment"}
    assert refuses({"kind": "click", "label": "Pay ₹4,532"}, page)
    assert refuses({"kind": "fill", "label": "Card number", "autocomplete": "cc-number"}, page)
    assert refuses({"kind": "click", "label": "Yes"}, {"text": "Pay from your saved card?"})
    assert not refuses({"kind": "click", "label": "Search"}, page)


def test_explicit_null_is_not_an_empty_reply():
    assert parse_field_value('{"text": null}') is EXPLICIT_NULL
    assert parse_field_value("not json at all") == "not json at all"
    assert parse_field_value('{"text": ""}') is None


def test_every_chunk_rejecting_the_target_raises():
    result = {"answers": {"click_target_1": {
        "choice": "none",
        "confidence": 0.5,
        "probabilities": {"1": 0.1, "2": 0.1, "none": 0.8},
    }, "click_target_2": {
        "choice": "none",
        "confidence": 0.5,
        "probabilities": {"3": 0.1, "4": 0.1, "none": 0.8},
    }}}
    candidates = {str(i): {"id": f"e{i}"} for i in range(1, 5)}
    try:
        chosen_target(result, "CLICK", candidates, limit=3)
    except ValueError as error:
        assert "no target" in str(error)
    else:
        raise AssertionError("a rejected target was returned")


def test_the_agent_blocks_a_pay_click_before_the_browser_acts():
    agent = Agent.__new__(Agent)
    agent.pending_text = None
    acted = []

    class Browser:
        def fresh(self, page, action=None):
            return True

        def act(self, *args, **kwargs):
            acted.append(args)

    page = {
        "fingerprint": "fp",
        "url": "http://127.0.0.1/payment",
        "title": "Payment",
        "text": "Pay ₹4,532",
        "actions": [{"id": "e1", "kind": "click", "label": "Pay ₹4,532", "node": 1}],
    }
    agent.browser = Browser()
    agent.state = {
        "decision": {"choice": "e1", "probabilities": {"e1": 1}, "latency_ms": 1,
                     "confidence": 1, "operation": "CLICK", "target": "1", "usage": {}},
        "page": page,
        "history": [],
        "status": "predicted",
        "started_at": time.perf_counter(),
        "browser": agent.browser,
        "goal": "book the flight",
    }
    snapshot = agent.command("act", {"fingerprint": "fp"})
    assert snapshot["status"] == "blocked" and acted == []
