"""The action guard sits at the registry's one choke point, so nothing that
re-registers an action can get around it."""

import asyncio
from types import SimpleNamespace

import wally_browser  # noqa: F401
from wally_browser.tools import build_tools


class Node:
    def __init__(self, name, tag="BUTTON", attributes=None):
        self.node_name = tag
        self.attributes = attributes or {}
        self.ax_node = SimpleNamespace(role="button" if tag == "BUTTON" else "textbox", name=name)
        self.parent_node = None

    def get_all_children_text(self, max_depth=-1):
        return ""


class Session:
    def __init__(self, node):
        self.node = node

    async def get_element_by_index(self, index):
        return self.node


def run(tools, name, params, node):
    return asyncio.run(tools.registry.execute_action(name, params, browser_session=Session(node)))


def test_the_guard_survives_browser_use_re_registering_click():
    tools = build_tools(ask=lambda q: None)
    tools.set_coordinate_clicking(True)  # what Agent.__init__ does for some model names
    result = run(tools, "click", {"index": 3}, Node("Pay now"))
    assert result.error and "stops before payment" in result.error


def test_a_click_without_an_index_is_refused():
    tools = build_tools(ask=lambda q: None)
    tools.set_coordinate_clicking(True)
    result = run(tools, "click", {"coordinate_x": 10, "coordinate_y": 20}, Node("Pay now"))
    assert result.error and "without an element index" in result.error


def test_typing_into_an_otp_field_is_refused():
    tools = build_tools(ask=lambda q: None)
    result = run(tools, "input", {"index": 4, "text": "123456"}, Node("Enter OTP", tag="INPUT"))
    assert result.error and "refused to type" in result.error


def test_a_commit_click_needs_a_yes_and_stops_without_a_person():
    tools = build_tools(ask=lambda q: None)
    result = run(tools, "click", {"index": 5}, Node("Book now"))
    assert result.is_done and result.success is False and "did not confirm" in result.error


def test_opening_a_payment_gateway_is_refused():
    tools = build_tools(ask=lambda q: None)
    result = run(tools, "navigate", {"url": "https://checkout.razorpay.com/v1/x"}, Node("x"))
    assert result.error and "payment gateway" in result.error
