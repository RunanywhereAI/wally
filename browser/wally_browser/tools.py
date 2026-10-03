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

from .elements import element_from_node, is_personal, label
from .guards import ControlTier, Element, control_tier, is_payment_gateway, is_selectable, is_typeable, \
    never_type_reason, refused_value

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
    """Set by the agent every step and by the guard itself; read by the guard."""

    attach: bool = False  # the person's own Chrome, or a persistent profile: saved cards, logins, 1-click
    # Sticky: the first checkout-like page of the run sets it, and nothing clears it. From then on
    # every click and choice asks, whatever its label (checkout mode).
    checkout: str | None = None
    # Set once the run types a personal detail. Commits (bookings, purchases, schedules) come after
    # the details are entered, so from here every click and choice needs the person's yes, whatever
    # the labels say.
    personal_typed: bool = False
    # Set after the person has had the browser window (a bot-check hand-off): they may have signed in.
    person_used_browser: bool = False

    def confirm_reason(self) -> str | None:
        if self.attach:
            return "this browser keeps your logins and saved payment methods"
        if self.person_used_browser:
            return "you used this browser window, so it may be signed in"
        if self.personal_typed:
            return "your details are entered, so any click could commit"
        if self.checkout:
            return f"checkout mode: {self.checkout}"
        return None


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
        if answer is not None:
            context.person_used_browser = True  # they had the window and may have signed in
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
        params = dict(params or {})
        if isinstance(params.get("text"), str):
            # A newline is an Enter keystroke: it can submit the form's default button.
            params["text"] = clean_text(params["text"])
        refusal = await check_action(action_name, params, browser_session, ask, on_stop, context)
        if refusal is not None:
            return refusal
        return await original(action_name, params, browser_session=browser_session, **kwargs)

    registry.execute_action = execute_action


def clean_text(text: str) -> str:
    """Typed text without newlines or control characters (each would be a keystroke)."""
    return " ".join("".join(c if c.isprintable() else " " for c in text).split())


OPTION_TEXT_JS = """function (wanted) {
  const w = String(wanted).trim().toLowerCase();
  for (const o of this.options || []) {
    if (o.text.trim().toLowerCase() === w || String(o.value).trim().toLowerCase() === w) return o.text;
  }
  return null;
}"""


async def option_text(session, node, wanted: str) -> str | None:
    """The visible text of the native <select> option browser-use would pick for `wanted`."""
    try:
        cdp = await session.cdp_client_for_node(node)
        resolved = await cdp.cdp_client.send.DOM.resolveNode(
            params={"backendNodeId": node.backend_node_id}, session_id=cdp.session_id)
        result = await cdp.cdp_client.send.Runtime.callFunctionOn(
            params={"objectId": resolved["object"]["objectId"], "functionDeclaration": OPTION_TEXT_JS,
                    "arguments": [{"value": wanted}], "returnByValue": True}, session_id=cdp.session_id)
        value = (result.get("result") or {}).get("value")
        return value if isinstance(value, str) else None
    except Exception:
        return None


async def live_text(session, node) -> str | None:
    """What the element says in the page right now; None when it cannot be read. Resolved through
    the node's own session (the one browser-use clicks through), so an element in a cross-origin
    frame is read where it lives."""
    try:
        cdp = await session.cdp_client_for_node(node)
        resolved = await cdp.cdp_client.send.DOM.resolveNode(
            params={"backendNodeId": node.backend_node_id}, session_id=cdp.session_id)
        result = await cdp.cdp_client.send.Runtime.callFunctionOn(
            params={"objectId": resolved["object"]["objectId"], "functionDeclaration": LIVE_TEXT_JS,
                    "returnByValue": True}, session_id=cdp.session_id)
        value = (result.get("result") or {}).get("value")
        return value if isinstance(value, str) else None
    except Exception:
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

    if name == "input":
        if not is_typeable(element):
            return ActionResult(error=f"wally refused input into [{index}]: it is not a text field")
        reason = never_type_reason(element) or refused_value(str(params.get("text", "")), element)
        if reason:
            return ActionResult(error=f"wally refused to type into [{index}]: {reason}")
        if context.attach:
            refusal = await _ask_or_refuse(ask, f"Type into {label(element)!r} in your own browser? [y/N]",
                                           f"the person did not confirm typing into [{index}]")
            if refusal:
                return refusal
        if is_personal(element):
            context.personal_typed = True
        return None

    if name == "select_dropdown":
        if not is_selectable(element):
            return ActionResult(error=f"wally refused select in [{index}]: it is not a dropdown")
        reason = never_type_reason(element)
        if reason:
            return ActionResult(error=f"wally refused to choose in [{index}]: {reason}")
        # The option is clicked (ARIA) or fires change (native): check the option that will really be
        # chosen like a button. browser-use matches the given text against an option's text OR value.
        wanted = str(params.get("text", ""))
        shown = await option_text(session, node, wanted) if element.tag == "select" else None
        option = Element(index=index, tag="option", name=shown or wanted,
                         full_text=f"{wanted} {shown or ''}", context_text=element.name)
        tier = control_tier(option)
        if tier is ControlTier.PAYMENT:
            message = f"wally stops before payment: choosing {option.name!r} in [{index}] would pay"
            if on_stop:
                on_stop(message)
            return ActionResult(error=message, is_done=True, success=True, extracted_content=message)
        why = "it may book or confirm" if tier is ControlTier.COMMIT else context.confirm_reason()
        if why:
            return await _ask_or_refuse(ask, f"Choose {option.name!r} in {label(element)!r} ({why})? [y/N]",
                                        f"the person did not confirm choosing {option.name!r}")
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
    why = "it may book or confirm" if tier is ControlTier.COMMIT else context.confirm_reason()
    if why is None and live is None and element.frame_url:
        why = "its current text could not be read inside a frame"  # fail closed
    if why:
        return await _ask_or_refuse(ask, f"The next click is {label(element)!r} ({why}). Click it? [y/N]",
                                    f"the person did not confirm clicking [{index}] {element.name!r}; "
                                    "do not try it again")
    return None


async def _ask_or_refuse(ask, question: str, refusal: str) -> ActionResult | None:
    """None on the person's yes; otherwise the refusal. Nobody to ask ends the run."""
    answer = await asyncio.to_thread(ask, question)
    if (answer or "").strip().lower() in ("y", "yes"):
        return None
    ended = answer is None
    return ActionResult(error=refusal, is_done=ended, success=False if ended else None)
