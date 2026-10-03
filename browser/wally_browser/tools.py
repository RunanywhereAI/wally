"""browser-use's Tools, narrowed and guarded.

* Only the actions the eve policy emits are registered; `evaluate` (arbitrary
  JavaScript), `send_keys` (an Enter can press a pay button), file actions,
  `extract` (another model) and the rest are excluded.
* Every action passes one guard at `registry.execute_action`. For click, input
  and select it re-reads the element (the snapshot, then its live text in the
  page, CSS ::before/::after included) and applies guards.py: a payment control
  or field is refused outright, a book/confirm control needs the person's yes.
* On a page with checkout signals, and on the person's own Chrome (attach),
  every click needs the person's yes, so a label the guards misread cannot
  commit anything on its own. On the person's own Chrome every typed value
  needs it too.
* Input only goes into a typeable field, and browser-use's fallback of typing
  into whatever has focus is switched off.
* `ask_user` asks the person in the terminal.
"""

from __future__ import annotations

import asyncio
import dataclasses
import sys
from dataclasses import dataclass
from typing import Callable

from browser_use import ActionResult, Tools
from pydantic import BaseModel, Field

from .elements import element_from_node, label
from .guards import ControlTier, NAVIGATES_TO_PAYMENT, control_tier, is_payment_gateway, is_typeable, \
    never_type_reason, normalize

ALLOWED_ACTIONS = {"done", "navigate", "go_back", "wait", "click", "input", "switch", "scroll", "select_dropdown"}
GUARDED = {"click", "input", "select_dropdown"}

# Read in the page just before a click: what the element says now, CSS-drawn text included.
LIVE_TEXT_JS = """function () {
  const attr = (n, k) => (n.getAttribute && n.getAttribute(k)) || '';
  const pseudo = (w) => { try { const c = getComputedStyle(this, w).content;
    return c && c !== 'none' && c !== 'normal' ? c.replace(/^["']|["']$/g, '') : ''; } catch (e) { return ''; } };
  return [this.innerText || this.textContent || '', attr(this, 'aria-label'), attr(this, 'title'),
          attr(this, 'value'), attr(this, 'alt'), attr(this, 'href'), pseudo('::before'), pseudo('::after')]
    .join(' ').slice(0, 4000);
}"""


class AskUserParams(BaseModel):
    question: str = Field(description="What to ask the person, in one sentence")


@dataclass
class GuardContext:
    """Set by the agent every step; read by the action guard."""

    attach: bool = False  # the person's own Chrome: saved cards, logins, 1-click
    checkout: str | None = None  # why this page needs every click confirmed, or None


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


def disable_page_typing() -> None:
    """browser-use's input action, when typing into the element fails, clicks the
    element and types into whatever has focus (default_action_watchdog). That
    target was never checked, so the fallback is refused."""
    from browser_use.browser.watchdogs.default_action_watchdog import DefaultActionWatchdog

    async def refuse(self, text: str):
        raise RuntimeError("wally: typing into the focused element is not allowed; only a checked field")

    DefaultActionWatchdog._type_to_page = refuse


def build_tools(ask: Callable[[str], str | None] = terminal_ask, on_stop: Callable[[str], None] | None = None,
                context: GuardContext | None = None) -> Tools:
    every = set(Tools().registry.registry.actions)
    tools = Tools(exclude_actions=sorted(every - ALLOWED_ACTIONS))
    context = context or GuardContext()

    @tools.registry.action("Ask the person for a detail or a choice the goal does not give.",
                           param_model=AskUserParams)
    async def ask_user(params: AskUserParams):
        answer = await asyncio.to_thread(ask, params.question)
        if answer is None:
            return ActionResult(error="nobody to ask (not a terminal); the run stops here", is_done=True,
                                success=False, extracted_content=f"Needed from the person: {params.question}")
        return ActionResult(extracted_content=f"The person answered {params.question!r}: {answer}",
                            long_term_memory=f"Person said: {answer}")

    disable_page_typing()
    _guard_registry(tools, ask, on_stop, context)
    return tools


def _guard_registry(tools: Tools, ask, on_stop, context: GuardContext) -> None:
    """Gate every action at the one place all of them pass through.

    Wrapping the registered functions would not hold: browser-use re-registers
    `click` (Tools.set_coordinate_clicking, called from Agent.__init__ for some
    model names), which would drop such a wrapper. execute_action stays ours.
    """
    registry = tools.registry
    original = registry.execute_action

    async def execute_action(action_name: str, params: dict, browser_session=None, **kwargs):
        refusal = await check_action(action_name, params or {}, browser_session, ask, on_stop, context)
        if refusal is not None:
            return refusal
        return await original(action_name, params, browser_session=browser_session, **kwargs)

    registry.execute_action = execute_action


async def live_text(session, node) -> str | None:
    """What the element says in the page right now; None when it cannot be read."""
    try:
        cdp = await session.get_or_create_cdp_session(target_id=node.target_id, focus=False)
        resolved = await cdp.cdp_client.send.DOM.resolveNode(
            params={"backendNodeId": node.backend_node_id}, session_id=cdp.session_id)
        result = await cdp.cdp_client.send.Runtime.callFunctionOn(
            params={"objectId": resolved["object"]["objectId"], "functionDeclaration": LIVE_TEXT_JS,
                    "returnByValue": True}, session_id=cdp.session_id)
        value = (result.get("result") or {}).get("value")
        return value if isinstance(value, str) else None
    except Exception:
        return None


_DISMISS = ("skip", "no thanks", "not now", "close", "cancel", "back", "dismiss", "maybe later", "×", "x")


def _confirm_needed(element, context: GuardContext) -> str | None:
    """Why this click needs the person's yes beyond the label tiers, or None."""
    text = normalize(element.name or element.full_text)
    if text in _DISMISS or NAVIGATES_TO_PAYMENT.fullmatch(text or "-"):
        return None
    if context.attach:
        return "this is your own Chrome, where saved cards and logins can complete a purchase"
    if context.checkout:
        return context.checkout
    return None


async def check_action(name: str, params: dict, session, ask, on_stop,
                       context: GuardContext | None = None) -> ActionResult | None:
    """None when the action may run; otherwise the ActionResult that replaces it. Fails closed."""
    context = context or GuardContext()
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
        if not is_typeable(element):
            return ActionResult(error=f"wally refused {name} into [{index}]: it is not a text field or dropdown")
        reason = never_type_reason(element)
        if reason:
            return ActionResult(error=f"wally refused to type into [{index}]: {reason}")
        if context.attach:
            answer = await asyncio.to_thread(
                ask, f"Type into {label(element)!r} in your own Chrome? [y/N]")
            if (answer or "").strip().lower() not in ("y", "yes"):
                ended = answer is None
                return ActionResult(error=f"the person did not confirm typing into [{index}]", is_done=ended,
                                    success=False if ended else None)
        return None

    # click: read what the element says now, not only what it said at the start of the step.
    live = await live_text(session, node)
    if live:
        element = dataclasses.replace(element, full_text=f"{element.full_text} {live}")
    tier = control_tier(element)
    if tier is ControlTier.PAYMENT:
        message = f"wally stops before payment: [{index}] {label(element)} would pay or place the order"
        if on_stop:
            on_stop(message)
        return ActionResult(error=message, is_done=True, success=True, extracted_content=message)
    why = "it may book or confirm" if tier is ControlTier.COMMIT else _confirm_needed(element, context)
    if why:
        answer = await asyncio.to_thread(ask, f"The next click is {label(element)!r} ({why}). Click it? [y/N]")
        if (answer or "").strip().lower() not in ("y", "yes"):
            ended = answer is None  # nobody to ask: the run stops here
            return ActionResult(error=f"the person did not confirm clicking [{index}] {element.name!r}; "
                                      "do not try it again", is_done=ended,
                                success=False if ended else None)
    return None
