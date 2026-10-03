"""browser-use's Tools, narrowed and guarded.

* Only the actions the eve policy emits are registered; `evaluate` (arbitrary
  JavaScript), `send_keys` (an Enter can press a pay button), file actions,
  `extract` (another model) and the rest are excluded.
* `click`, `input` and `select_dropdown` re-read the real element before they
  run and apply guards.py: a payment control or field is refused outright, a
  book/confirm control needs the person's yes. This holds whatever produced
  the action.
* `ask_user` asks the person in the terminal.
"""

from __future__ import annotations

import asyncio
import sys
from typing import Callable

from browser_use import ActionResult, Tools
from pydantic import BaseModel, Field

from .elements import element_from_node, label
from .guards import ControlTier, control_tier, is_payment_gateway, never_type_reason

ALLOWED_ACTIONS = {"done", "navigate", "go_back", "wait", "click", "input", "switch", "scroll", "select_dropdown"}
GUARDED = {"click", "input", "select_dropdown"}


class AskUserParams(BaseModel):
    question: str = Field(description="What to ask the person, in one sentence")


class Stop(Exception):
    """Raised by a guard when the run must end here (the payment page)."""


def terminal_ask(question: str) -> str | None:
    """Ask on the terminal; None when there is no one to ask."""
    if not sys.stdin.isatty():
        return None
    sys.stderr.write(f"\n{question}\n> ")
    sys.stderr.flush()
    try:
        return sys.stdin.readline().strip()
    except (EOFError, KeyboardInterrupt):
        return None


def build_tools(ask: Callable[[str], str | None] = terminal_ask, on_stop: Callable[[str], None] | None = None) -> Tools:
    every = set(Tools().registry.registry.actions)
    tools = Tools(exclude_actions=sorted(every - ALLOWED_ACTIONS))

    @tools.registry.action("Ask the person for a detail or a choice the goal does not give.",
                           param_model=AskUserParams)
    async def ask_user(params: AskUserParams):
        answer = await asyncio.to_thread(ask, params.question)
        if answer is None:
            return ActionResult(error="nobody to ask (not a terminal); the run stops here", is_done=True,
                                success=False, extracted_content=f"Needed from the person: {params.question}")
        return ActionResult(extracted_content=f"The person answered {params.question!r}: {answer}",
                            long_term_memory=f"Person said: {answer}")

    _guard_registry(tools, ask, on_stop)
    return tools


def _guard_registry(tools: Tools, ask, on_stop) -> None:
    """Gate every action at the one place all of them pass through.

    Wrapping the registered functions would not hold: browser-use re-registers
    `click` (Tools.set_coordinate_clicking, called from Agent.__init__ for some
    model names), which would drop such a wrapper. execute_action stays ours.
    """
    registry = tools.registry
    original = registry.execute_action

    async def execute_action(action_name: str, params: dict, browser_session=None, **kwargs):
        refusal = await check_action(action_name, params or {}, browser_session, ask, on_stop)
        if refusal is not None:
            return refusal
        return await original(action_name, params, browser_session=browser_session, **kwargs)

    registry.execute_action = execute_action


async def check_action(name: str, params: dict, session, ask, on_stop) -> ActionResult | None:
    """None when the action may run; otherwise the ActionResult that replaces it. Fails closed."""
    if name == "navigate":
        url = str(params.get("url", ""))
        if is_payment_gateway(url):
            return ActionResult(error=f"wally refused to open a payment gateway ({url[:80]})")
        return None
    if name not in GUARDED:
        return None
    index = params.get("index")
    if index is None:
        return ActionResult(error=f"wally refused {name} without an element index (coordinate actions are not "
                                  "checked, so they are not allowed)")
    if session is None:
        return ActionResult(error=f"wally refused {name}: no browser session to check the element against")
    node = await session.get_element_by_index(index)
    if node is None:
        return ActionResult(error=f"wally refused {name}: element [{index}] is not on the page any more")
    element = element_from_node(index, node)
    if name in ("input", "select_dropdown"):
        reason = never_type_reason(element)
        if reason:
            return ActionResult(error=f"wally refused to type into [{index}]: {reason}")
    tier = control_tier(element)
    if tier is ControlTier.PAYMENT:
        message = f"wally stops before payment: [{index}] {label(element)} would pay or place the order"
        if on_stop:
            on_stop(message)
        return ActionResult(error=message, is_done=True, success=True, extracted_content=message)
    if tier is ControlTier.COMMIT and name == "click":
        answer = await asyncio.to_thread(
            ask, f"The next click is {label(element)!r}, which may book or confirm. Click it? [y/N]")
        if (answer or "").strip().lower() not in ("y", "yes"):
            ended = answer is None  # nobody to ask: the run stops here
            return ActionResult(error=f"the person did not confirm clicking [{index}] {element.name!r}; "
                                      "do not try it again", is_done=ended,
                                success=False if ended else None)
    return None
