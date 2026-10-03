"""EveAgent: browser-use's Agent with eve choosing each step.

browser-use keeps everything it is good at (CDP, tabs, iframes, shadow DOM,
the step loop, retries). One method changes: get_model_output builds the next
action from the eve policy instead of asking a chat model for JSON. Before
eve is asked, the page rules run: the payment page ends the run, a bot check
is handed to the person.
"""

from __future__ import annotations

import asyncio
import json
import re
import time
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Callable

from browser_use import Agent

from .elements import elements_from_selector_map, is_personal, label
from .guards import Element, check_page
from .policy import GATE_BOT, GATE_PAYMENT, GATE_THRESHOLD, EvePolicy, Observation
from .profile import Profile, key_fits_field
from .text import TextError, TextModel

CONTACT_FIELD = re.compile(r"e-?mail|\bmail\b|phone|mobile|\btel\b|contact", re.I)


def short(element: Element) -> str:
    """An element named for the action history: no value marker, which would go stale
    the moment the action runs ("· empty" on a field that was just filled)."""
    return f"[{element.index}] {element.name or element.placeholder or element.tag}"


@dataclass
class StepRecord:
    step: int
    url: str
    operation: str
    target: str = ""
    value_source: str = ""  # profile:<key> | text-model | person | ""
    decision_ms: int = 0
    text_ms: int = 0
    step_ms: int = 0  # filled in when the next step starts (observe + decide + act)
    rounds: int = 0
    prompt_tokens: int = 0
    options_per_question: dict = field(default_factory=dict)
    over_26: dict = field(default_factory=dict)
    window_retries: int = 0
    elements: int = 0
    gates: dict = field(default_factory=dict)
    stop: str = ""


def outcome_notes(results, last_target: Element | None) -> tuple[list[str], set[str]]:
    """Notes for eve from the previous action's results, and the element names that must not be
    offered again (the person declined, or a guard refused)."""
    notes: list[str] = []
    refused: set[str] = set()
    for result in results:
        error = getattr(result, "error", None)
        content = getattr(result, "extracted_content", None)
        if error:
            notes.append(f"last action failed: {error[:200]}")
            if last_target is not None and ("did not confirm" in error or "refused" in error):
                refused.add(last_target.name)
        elif content:
            notes.append(content[:300])
    return notes, refused


def page_frames(dom_state, limit: int = 50000) -> list[tuple[str, bool]]:
    """(src, visible) of every iframe/frame in THIS page's DOM tree (not every
    tab's), walking children, shadow roots and frame documents."""
    root = getattr(getattr(dom_state, "_root", None), "original_node", None)
    found: list[tuple[str, bool]] = []
    stack = [root] if root is not None else []
    seen = 0
    while stack and seen < limit:
        node = stack.pop()
        seen += 1
        if (getattr(node, "node_name", "") or "").upper() in ("IFRAME", "FRAME"):
            src = (getattr(node, "attributes", None) or {}).get("src", "")
            box = getattr(node, "absolute_position", None)
            tiny = box is not None and (getattr(box, "width", 1) < 3 or getattr(box, "height", 1) < 3)
            visible = getattr(node, "is_visible", None) is not False and not tiny
            if src:
                found.append((src, visible))
        stack.extend(getattr(node, "children_nodes", None) or [])
        stack.extend(getattr(node, "shadow_roots", None) or [])
        document = getattr(node, "content_document", None)
        if document is not None:
            stack.append(document)
    return found


class EveAgent(Agent):
    def __init__(self, *args, policy: EvePolicy, text_model: TextModel | None, profile: Profile,
                 ask: Callable[[str], str | None], log_path: Path | None = None, **kwargs):
        super().__init__(*args, **kwargs)
        self._wally_policy = policy
        self._wally_text = text_model
        self._wally_profile = profile
        self._wally_ask = ask
        self._wally_log = log_path
        self._wally_state = None
        self._wally_plan = ""
        self._wally_history: list[str] = []
        self._wally_notes: list[str] = []
        self._wally_steps: list[StepRecord] = []
        self._wally_step_started = 0.0
        self._wally_last_results: list = []
        self._wally_last_target = None
        self._wally_refused: set[str] = set()  # element names the person declined or a guard refused
        self._wally_handed_off: set[tuple[str, str]] = set()  # (url, reason) already handed to the person
        self.stop_reason = ""
        self.plan_ms = 0

    # step() clears state.last_result right after _prepare_context; read it here first.
    async def _prepare_context(self, step_info=None):
        self._wally_last_results = list(self.state.last_result or [])
        return await super()._prepare_context(step_info)

    def _absorb_results(self) -> None:
        """Hand the previous action's outcome to eve: answers, refusals and errors."""
        notes, refused = outcome_notes(self._wally_last_results, self._wally_last_target)
        self._wally_notes.extend(notes)
        self._wally_refused.update(refused)
        self._wally_last_results = []

    # browser-use hands the page state to _get_next_action; keep it for get_model_output.
    async def _get_next_action(self, browser_state_summary):
        self._wally_state = browser_state_summary
        return await super()._get_next_action(browser_state_summary)

    def _close_previous_step(self, now: float) -> None:
        if self._wally_steps and self._wally_step_started:
            self._wally_steps[-1].step_ms = round((now - self._wally_step_started) * 1000)
            self._write_log(self._wally_steps[-1])

    def _write_log(self, record: StepRecord) -> None:
        if self._wally_log:
            with self._wally_log.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(asdict(record)) + "\n")

    def finish(self) -> None:
        """Close the last step's timing (call after the run)."""
        self._close_previous_step(time.perf_counter())

    def _output(self, action: dict, next_goal: str):
        name, params = next(iter(action.items()))
        return self.AgentOutput(evaluation_previous_goal="", memory=self._wally_plan[:500] or None,
                                next_goal=next_goal, action=[self.ActionModel(**{name: params})])

    def _done(self, text: str, success: bool):
        self.stop_reason = text
        return self._output({"done": {"text": text, "success": success}}, "stop")

    async def get_model_output(self, input_messages):  # noqa: ARG002 - eve reads the page, not the messages
        now = time.perf_counter()
        self._close_previous_step(now)
        self._wally_step_started = now
        state = self._wally_state
        goal = self.task

        if self._wally_text is not None and not self._wally_plan:
            started = time.perf_counter()
            try:
                self._wally_plan = await asyncio.to_thread(self._wally_text.plan, goal)
            except TextError as error:
                self._wally_notes.append(f"no plan: {error}")
            self.plan_ms = round((time.perf_counter() - started) * 1000)

        self._absorb_results()
        elements = elements_from_selector_map(state.dom_state.selector_map)
        try:
            frames = page_frames(state.dom_state)
        except Exception as error:
            frames = []
            self._wally_notes.append(f"could not read the page's frames: {error}")
        # Payment: any frame, visible or not, ends the run (fail closed). Bot check: visible only.
        payment_frames = [src for src, _ in frames]
        visible_frames = [src for src, visible in frames if visible]
        record = StepRecord(step=len(self._wally_steps) + 1, url=state.url, operation="", elements=len(elements))
        self._wally_steps.append(record)

        # Page rules first. They need no model and nothing can override them.
        page = check_page(state.url, state.title, elements, payment_frames)
        bot = check_page(state.url, state.title, [], visible_frames).bot_check if page.bot_check else None
        if page.payment_page:
            record.operation, record.stop = "STOP", f"payment page: {page.payment_page}"
            return self._done(f"Stopped at the payment page ({page.payment_page}). {state.title} — {state.url}", True)
        if bot and (state.url, bot) not in self._wally_handed_off:
            self._wally_handed_off.add((state.url, bot))
            record.operation, record.stop = "HAND_OFF", f"bot check: {bot}"
            return await self._hand_off_bot_check(bot)

        for message in getattr(state, "closed_popup_messages", None) or []:
            if message not in self._wally_notes:
                self._wally_notes.append(message)
        offered = [e for e in elements if e.name not in self._wally_refused]
        obs = Observation(goal=goal, url=state.url, title=state.title, elements=offered,
                          tabs=[(t.target_id[-4:], t.title) for t in state.tabs], plan=self._wally_plan,
                          history=self._wally_history, notes=self._wally_notes)
        decision = await asyncio.to_thread(self._wally_policy.decide, obs)
        self._wally_last_target = decision.target
        record.operation = decision.operation
        record.target = label(decision.target) if decision.target else ""
        record.decision_ms, record.rounds = decision.decision_ms, decision.rounds
        record.prompt_tokens, record.window_retries = decision.prompt_tokens, decision.window_retries
        record.options_per_question, record.over_26, record.gates = (
            decision.options_per_question, decision.over_26, decision.gates)

        # eve's gates may only ADD a stop or a hand-off.
        if decision.gates.get(GATE_PAYMENT, 0) >= GATE_THRESHOLD:
            answer = await self._ask(f"eve reads this page as asking for payment (p={decision.gates[GATE_PAYMENT]:.2f}). "
                                     "Stop here? [Y/n]")
            if answer is None or answer.strip().lower() in ("", "y", "yes"):
                record.stop = "payment page (eve gate)"
                return self._done(f"Stopped: this looks like the payment page. {state.title} — {state.url}", True)
        if decision.gates.get(GATE_BOT, 0) >= GATE_THRESHOLD and (state.url, "eve") not in self._wally_handed_off:
            self._wally_handed_off.add((state.url, "eve"))
            record.stop = "bot check (eve gate)"
            return await self._hand_off_bot_check("eve reads a bot check on the page")

        return await self._act(decision, obs, record)

    async def _hand_off_bot_check(self, why: str):
        try:
            cdp = await self.browser_session.get_or_create_cdp_session(focus=True)
            await cdp.cdp_client.send.Page.bringToFront(session_id=cdp.session_id)
        except Exception:
            pass
        answer = await self._ask(f"A bot check needs a person ({why}). Solve it in the browser window, "
                                 "then press Enter here.")
        if answer is None:
            return self._done(f"Stopped: a bot check needs a person ({why}).", False)
        self._wally_notes.append("the person solved a bot check")
        return self._output({"wait": {"seconds": 1}}, "re-read the page after the bot check")

    async def _act(self, decision, obs: Observation, record: StepRecord):
        op, target = decision.operation, decision.target
        if op == "CLICK" and target is not None:
            self._wally_history.append(f"chose to click {short(target)}")
            return self._output({"click": {"index": target.index}}, f"click {target.name}")
        if op in ("TYPE", "SELECT") and target is not None:
            value, source = await self._value_for(decision, obs, record)
            if value is None:
                return await self._ask_for(target, obs, record)
            record.value_source = source
            self._wally_history.append(f"{'typed' if op == 'TYPE' else 'selected'} into {short(target)}")
            if op == "TYPE":
                return self._output({"input": {"index": target.index, "text": value, "clear": True}},
                                    f"fill {target.name}")
            return self._output({"select_dropdown": {"index": target.index, "text": value}}, f"choose {target.name}")
        if op in ("SCROLL_DOWN", "SCROLL_UP"):
            self._wally_history.append(op.lower())
            return self._output({"scroll": {"down": op == "SCROLL_DOWN", "pages": 1}}, op.lower())
        if op == "GO_BACK":
            self._wally_history.append("went back")
            return self._output({"go_back": {}}, "go back")
        if op == "SWITCH_TAB" and decision.tab_id:
            for tab in self._wally_state.tabs:
                if tab.target_id.endswith(decision.tab_id):
                    self._wally_history.append(f"switched to tab {tab.title[:40]}")
                    return self._output({"switch": {"tab_id": decision.tab_id}}, "switch tab")
        if op == "ASK_USER":
            question = (f"The agent needs a detail or a choice to continue on '{obs.title[:80]}'. "
                        f"Goal: {obs.goal}. What should it do?")
            return self._output({"ask_user": {"question": question}}, "ask the person")
        if op == "DONE":
            return self._done(f"Goal reached: {obs.title} — {obs.url}", True)
        self._wally_history.append("waited")
        return self._output({"wait": {"seconds": 2}}, "wait")

    async def _value_for(self, decision, obs: Observation, record: StepRecord) -> tuple[str | None, str]:
        target = decision.target
        # Finding 10: eve proposes the key; the field's own label must agree before a profile
        # value is typed, so personal data never lands in a search or promo box.
        if decision.value_key and key_fits_field(decision.value_key, target.label_text, target.autocomplete):
            value = self._wally_profile.get(decision.value_key)
            if value is not None:
                return value, f"profile:{decision.value_key}"
        if is_personal(target):
            return None, ""  # personal details come from the profile or the person, never a model
        if self._wally_text is None:
            return None, ""
        started = time.perf_counter()
        try:
            value = await asyncio.to_thread(self._wally_text.field_text, obs.goal, self._wally_plan, obs.title,
                                            label(target))
        except TextError as error:
            self._wally_notes.append(f"text model: {error}")
            value = None
        record.text_ms = round((time.perf_counter() - started) * 1000)
        return value, "text-model" if value is not None else ""

    async def _ask(self, question: str) -> str | None:
        """Ask the person off the event loop, so the browser connection stays alive while they think."""
        return await asyncio.to_thread(self._wally_ask, question)

    async def _ask_for(self, target: Element, obs: Observation, record: StepRecord):
        contact = bool(CONTACT_FIELD.search(target.label_text)) or target.input_type in ("email", "tel")
        question = f"What should go in {label(target)!r}? (not in your traveller profile)"
        answer = await self._ask(question)
        if answer is None or not answer.strip():
            what = "contact details" if contact else "a detail the agent does not have"
            record.stop = f"reached {what}: {label(target)}"
            return self._done(f"Stopped: reached {what} ({target.name or target.placeholder}). "
                              f"{obs.title} — {obs.url}", False)
        record.value_source = "person"
        self._wally_history.append(f"typed the person's answer into {short(target)}")
        return self._output({"input": {"index": target.index, "text": answer.strip(), "clear": True}},
                            f"fill {target.name}")
