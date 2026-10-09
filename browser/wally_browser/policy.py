"""The eve policy: one decision request per step picks the operation and,
speculatively, a target for every operation that needs one.

Round 1 (always): operation head, click/type/select/tab target heads (each at
most 26 options; a list longer than that is offered as regions), and three
yes/no gates (payment details asked for? bot check? goal already on screen?).
Round 2 (only when needed): the target inside the chosen region, and which
profile value fills the chosen field.

Gates from eve can only ADD a stop or a hand-off. Whether a click or a field is
allowed is decided by guards.py, never by a probability here.
"""

from __future__ import annotations

import re
import time
from dataclasses import dataclass, field

from . import elements as el
from .decisions import DecisionClient, Question, Result, WindowExceeded
from .guards import Element, never_type_reason
from .profile import FIELDS, Profile

OPERATIONS: dict[str, str] = {
    "CLICK": "click a button, link, tab, date or option",
    "TYPE": "type into a text field",
    "SELECT": "choose a value in a dropdown",
    "SCROLL_DOWN": "scroll down to see more of the page",
    "SCROLL_UP": "scroll up",
    "GO_BACK": "go back to the previous page",
    "SWITCH_TAB": "switch to another open tab",
    "WAIT": "wait for the page to finish loading",
    "ASK_USER": "ask the person for a detail or choice the goal does not give",
    "DONE": "the goal's final state is on screen; stop here",
}
GATE_PAYMENT = "gate_payment"
GATE_BOT = "gate_bot"
GATE_DONE = "gate_done"
GATE_THRESHOLD = 0.5  # only ever used to ADD a stop or hand-off
OTHER_TEXT = "none of these: other text"
HISTORY_KEEP = 10
_TYPED_INTO = re.compile(r"typed into \[(\d+)\]")
_CLICKED = re.compile(r"chose to click \[(\d+)\]")


def fields_typed_but_still_empty(history: list[str], elements: list[Element]) -> list[Element]:
    """A field we already typed into that still has no value. Typing it again
    does not operate a combobox; the suggestion is a click."""
    typed: set[int] = set()
    for line in history:
        match = _TYPED_INTO.search(line)
        if match:
            typed.add(int(match.group(1)))
    if not typed:
        return []
    return [element for element in elements if element.index in typed and not (element.value or "").strip()]


def clicks_already_taken(history: list[str]) -> set[int]:
    """Indexes already clicked. A dialog opener stays on the page after it has
    done its job, and clicking it again never reaches the controls inside."""
    taken: set[int] = set()
    for line in history:
        match = _CLICKED.search(line)
        if match:
            taken.add(int(match.group(1)))
    return taken


@dataclass
class Observation:
    goal: str
    url: str
    title: str
    elements: list[Element]
    tabs: list[tuple[str, str]] = field(default_factory=list)  # (tab_id, title)
    plan: str = ""
    history: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)  # dialogs, user answers, last errors
    image: str | None = None  # data URL of the page, sent with every decision round


@dataclass
class Decision:
    operation: str
    target: Element | None = None
    tab_id: str | None = None
    value_key: str | None = None  # profile key, or None when other text is needed
    gates: dict[str, float] = field(default_factory=dict)
    confidence: float = 0.0
    target_confidence: float | None = None
    rounds: int = 1
    decision_ms: int = 0
    prompt_tokens: int = 0
    options_per_question: dict[str, int] = field(default_factory=dict)
    window_retries: int = 0
    over_26: dict[str, int] = field(default_factory=dict)  # head -> candidate count when regions were used
    probabilities: dict[str, dict[str, float]] = field(default_factory=dict)


def _stuck_type_note(obs: Observation) -> str:
    stuck = fields_typed_but_still_empty(obs.history, obs.elements)
    if not stuck:
        return ""
    names = ", ".join(el.label(element) for element in stuck[:3])
    return f"Typing did not stick in {names}. Do not type there again; click the matching suggestion."


def _open_dialog_note(obs: Observation) -> str:
    if not any(element.context_text for element in obs.elements):
        return ""
    return "A dialog is open. Click a control inside it, or its close button. Do not click the button that opened it."


def _state_text(obs: Observation, table_budget: int) -> str:
    tabs = "; ".join(f"{tid}: {title[:40]}" for tid, title in obs.tabs) if len(obs.tabs) > 1 else ""
    parts = [
        f"Goal: {obs.goal}",
        f"Plan: {obs.plan}" if obs.plan else "",
        f"Page: {obs.title[:120]} — {obs.url[:200]}",
        f"Open tabs: {tabs}" if tabs else "",
        "Recent actions:\n" + "\n".join(f"- {h}" for h in obs.history[-HISTORY_KEEP:]) if obs.history else "",
        "Notes:\n" + "\n".join(f"- {n}" for n in obs.notes[-5:]) if obs.notes else "",
        "Interactive elements (in page order):\n" + el.table(obs.elements, table_budget),
        _stuck_type_note(obs),
        _open_dialog_note(obs),
        "Rules: never pay, never type card, UPI or OTP details, and stop when the payment page is reached.",
    ]
    return "\n".join(p for p in parts if p)


class EvePolicy:
    def __init__(self, client: DecisionClient, profile: Profile | None = None, clock=time.perf_counter):
        self.client = client
        self.profile = profile or Profile(values={})
        self._clock = clock

    # -- request building ---------------------------------------------------

    def _heads(self, obs: Observation) -> tuple[list[Question], dict]:
        """Round-1 questions, and how each target head maps back to elements."""
        stuck = {element.index for element in fields_typed_but_still_empty(obs.history, obs.elements)}
        clicked = clicks_already_taken(obs.history)
        by_kind: dict[str, list[Element]] = {"click": [], "type": [], "select": []}
        for element in obs.elements:
            kind = el.kind_of(element)
            if kind == "type" and (element.index in stuck or never_type_reason(element)):
                continue  # never offered; a stuck field needs a click, and guards.py would refuse the rest
            if kind == "click" and element.index in clicked:
                continue  # the opener already ran; the dialog's own controls are other indexes
            by_kind[kind].append(element)

        operations = ["CLICK", "SCROLL_DOWN", "SCROLL_UP", "GO_BACK", "WAIT", "ASK_USER", "DONE"]
        if by_kind["type"]:
            operations.insert(1, "TYPE")
        if by_kind["select"]:
            operations.insert(2, "SELECT")
        if len(obs.tabs) > 1:
            operations.insert(-2, "SWITCH_TAB")
        if not by_kind["click"]:
            operations.remove("CLICK")

        questions = [Question("operation", "choice", "What is the single best next action toward the goal?",
                              [f"{op}: {OPERATIONS[op]}" for op in operations])]
        mapping: dict = {"operations": operations}
        for kind, head in (("click", "click_target"), ("type", "type_target"), ("select", "select_target")):
            candidates = by_kind[kind]
            if len(candidates) < 2:
                mapping[head] = ("single", candidates)
                continue
            if len(candidates) <= el.MAX_OPTIONS:
                questions.append(Question(head, "choice", f"If the action is {kind.upper()}, which element?",
                                          [el.label(c) for c in candidates]))
                mapping[head] = ("elements", candidates)
            else:
                trimmed = candidates[: el.MAX_OPTIONS * el.MAX_OPTIONS]
                regions = el.regions(trimmed)
                questions.append(Question(head, "choice",
                                          f"If the action is {kind.upper()}, which group holds the element?",
                                          [r.option for r in regions]))
                mapping[head] = ("regions", regions, len(candidates))
        if len(obs.tabs) > 1:
            tabs = obs.tabs[: el.MAX_OPTIONS]
            questions.append(Question("tab_target", "choice", "If switching tabs, which tab?",
                                      [f"tab {tid}: {title[:80]}" for tid, title in tabs]))
            mapping["tab_target"] = tabs
        questions += [
            Question(GATE_PAYMENT, "yes_no", "Is this page asking for payment details (card, UPI, net banking, "
                                             "wallet) or showing a pay button?"),
            Question(GATE_BOT, "yes_no", "Is a CAPTCHA or 'verify you are human' check blocking the page?"),
            Question(GATE_DONE, "yes_no", "Is the goal's final state already on screen, so no further action "
                                          "is needed?"),
        ]
        return questions, mapping

    def build(self, obs: Observation, shrink: int = 0) -> tuple[str, list[Question], dict]:
        """Round-1 input and questions, cut so every question fits the window.
        `shrink` halves the element-table budget that many times (window retries)."""
        questions, mapping = self._heads(obs)
        widest = max(questions, key=lambda q: el.question_tokens("", q.text, q.options))
        overhead = el.question_tokens("", widest.text, widest.options)
        budget = el.MAX_QUESTION_TOKENS - overhead - 600  # 600: goal, plan, history, notes, rules
        budget = max(200, budget >> shrink)
        state = _state_text(obs, budget)
        while el.question_tokens(state, widest.text, widest.options) > el.MAX_QUESTION_TOKENS and budget > 200:
            budget //= 2
            state = _state_text(obs, budget)
        return state, questions, mapping

    # -- asking -------------------------------------------------------------

    def _ask(self, state: str, questions: list[Question], decision: Decision,
             image: str | None = None) -> Result:
        result = self.client.ask(state, questions, images=[image] if image else None)
        decision.decision_ms += result.latency_ms
        decision.prompt_tokens += result.prompt_tokens
        for q in questions:
            decision.options_per_question[q.id] = len(q.options) if q.kind != "yes_no" else 2
            decision.probabilities[q.id] = result.answers[q.id].probabilities
        return result

    def decide(self, obs: Observation) -> Decision:
        decision = Decision(operation="WAIT")
        shrink = 0
        while True:
            state, questions, mapping = self.build(obs, shrink)
            try:
                result = self._ask(state, questions, decision, obs.image)
                break
            except WindowExceeded:
                decision.window_retries += 1
                shrink += 1
                if shrink > 4:
                    raise
        answers = result.answers
        decision.gates = {gate: answers[gate].p("yes") for gate in (GATE_PAYMENT, GATE_BOT, GATE_DONE)}

        label, confidence = answers["operation"].top()
        operation = label.split(":", 1)[0]
        decision.operation, decision.confidence = operation, confidence

        head = {"CLICK": "click_target", "TYPE": "type_target", "SELECT": "select_target"}.get(operation)
        if head:
            decision.target, decision.target_confidence = self._resolve_target(
                obs, state, head, mapping[head], answers, decision)
            if decision.target is None:
                decision.operation = "WAIT"
        elif operation == "SWITCH_TAB":
            tabs = mapping.get("tab_target") or []
            if "tab_target" in answers and tabs:
                chosen, _ = answers["tab_target"].top()
                decision.tab_id = chosen.split(":", 1)[0].removeprefix("tab ").strip()

        for kind, info in mapping.items():
            if isinstance(info, tuple) and info[0] == "regions":
                decision.over_26[kind] = info[2]

        if decision.operation in ("TYPE", "SELECT") and decision.target is not None and self.profile.keys():
            decision.value_key = self._value_key(obs, decision)
        return decision

    def _resolve_target(self, obs, state, head, info, answers, decision) -> tuple[Element | None, float | None]:
        kind = info[0]
        if kind == "single":
            return (info[1][0], None) if info[1] else (None, None)
        chosen, p = answers[head].top()
        if kind == "elements":
            for candidate in info[1]:
                if el.label(candidate) == chosen:
                    return candidate, p
            return None, None
        # regions: a second round inside the chosen region
        regions = info[1]
        region = next((r for r in regions if r.option == chosen), None)
        if region is None:
            return None, None
        if len(region.members) == 1:
            return region.members[0], p
        question = Question(head, "choice", f"Which element? ({head.split('_')[0].upper()})",
                            [el.label(m) for m in region.members])
        decision.rounds += 1
        result = self._ask(state, [question], decision, obs.image)
        chosen, p2 = result.answers[head].top()
        for member in region.members:
            if el.label(member) == chosen:
                return member, p2
        return None, None

    def _value_key(self, obs: Observation, decision: Decision) -> str | None:
        """Round 2: which profile value fills the chosen field, if any."""
        keys = self.profile.keys()[: el.MAX_OPTIONS - 1]
        options = [f"{key}: {FIELDS[key]}" for key in keys] + [OTHER_TEXT]
        field_text = el.label(decision.target)
        state = f"Goal: {obs.goal}\nPage: {obs.title[:120]}\nThe field to fill: {field_text}"
        question = Question("value", "choice", "Which traveller detail belongs in this field?", options)
        decision.rounds += 1
        result = self._ask(state, [question], decision, obs.image)
        chosen, _ = result.answers["value"].top()
        key = chosen.split(":", 1)[0]
        return key if key in keys else None
