"""The compact element table eve reads, built from browser-use's selector map.

browser-use's own serialized DOM runs to 40,000 characters, far past the 8,185
tokens a decision question may hold. This keeps one short line per interactive
element (`[12] button Search · empty`), estimates tokens conservatively, and
splits any candidate list longer than the 26-option cap into regions.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Iterable

from .guards import Element, is_selectable, is_typeable, looks_like_card_number, never_type_reason

# The decision API's hard limits (eve, /v1/decisions; master's 00:46 corrections).
MAX_OPTIONS = 26
MAX_QUESTION_TOKENS = 8185
OPTION_NAME_MAX = 256
NAME_MAX = 80
FULL_TEXT_MAX = 2000  # what the guards read (head and tail kept); the display name is NAME_MAX
CONTEXT_MAX = 600

# Fields whose values are personal: filled from the profile or by the person, never by a model.
PERSONAL_FIELD = re.compile(
    r"\bname\b|first|last|surname|given|family|middle|\bdob\b|birth|passport|e-?mail|\bmail\b|phone|mobile|"
    r"\btel\b|contact|gender|nationality|address|street|\bflat\b|house|building|landmark|locality|\barea\b|"
    r"sector|village|apartment|\bsuite\b|pin ?code|zip|postal|frequent|aadhaar|\bpan\b|\btitle\b|salutation",
    re.I,
)


def is_personal(element: Element) -> bool:
    token = element.autocomplete.split(" ")[-1] if element.autocomplete else ""
    if element.input_type in ("email", "tel") or token in (
            "email", "tel", "tel-national", "name", "given-name", "family-name", "bday", "street-address",
            "address-line1", "address-line2", "address-line3", "address-level1", "address-level2", "postal-code"):
        return True
    return bool(PERSONAL_FIELD.search(element.label_text))


_TEXT_ROLES = {"textbox", "searchbox", "combobox", "spinbutton"}
_TYPEABLE_INPUTS = {"", "text", "search", "email", "tel", "number", "date", "url", "datetime-local", "month"}


def estimate_tokens(text: str) -> int:
    """A deliberately high estimate: ~3 characters per token (English page text
    runs nearer 4). Over-estimating only makes the table shorter than needed;
    under-estimating would get a 400 and a retry."""
    return len(text) // 3 + 1


def question_tokens(input_text: str, question: str, options: Iterable[str]) -> int:
    """The per-question window: input + question + options + 67 + 3 per option."""
    options = list(options)
    return (estimate_tokens(input_text) + estimate_tokens(question)
            + sum(estimate_tokens(o) for o in options) + 67 + 3 * len(options))


def _clip(text: str, limit: int) -> str:
    text = " ".join((text or "").split())
    return text if len(text) <= limit else text[: limit - 1] + "…"


_TEXT_NODE = 3


def _all_text(node, budget: int = 20000) -> str:
    """Every text node under `node`, light and shadow DOM alike (not frame documents).
    browser-use's get_all_children_text walks children only and skips shadow roots."""
    parts: list[str] = []
    stack = [node]
    seen = 0
    while stack and seen < budget:
        current = stack.pop()
        seen += 1
        if getattr(current, "node_type", None) == _TEXT_NODE or (getattr(current, "node_type", None) is not None
                                                                  and int(current.node_type) == _TEXT_NODE):
            value = getattr(current, "node_value", "") or ""
            if value.strip():
                parts.append(value.strip())
            continue
        children = list(getattr(current, "children_nodes", None) or [])
        shadows = list(getattr(current, "shadow_roots", None) or [])
        stack.extend(reversed(children + shadows))
    return " ".join(parts)


def _head_and_tail(text: str, limit: int) -> str:
    """Long text keeps its start and its end: a fare card's pay word is often last."""
    if len(text) <= limit:
        return text
    half = limit // 2
    return text[:half] + " … " + text[-half:]


_DIALOG_CLASSES = {"modal", "dialog", "popup", "overlay", "drawer", "bottom-sheet"}


def marks_dialog(node_name: str, attributes: dict) -> bool:
    """A real dialog ancestor. Class names are whole tokens: a class that merely
    contains 'dialog' or 'overlay' is not one, or the whole page becomes the
    context and a Search button reads as a pay button."""
    attributes = attributes or {}
    classes = set((attributes.get("class") or "").lower().split())
    name = (node_name or "").upper()
    return (name == "DIALOG" or attributes.get("role") in ("dialog", "alertdialog")
            or attributes.get("aria-modal") == "true" or bool(classes & _DIALOG_CLASSES))


def _dialog_context(node) -> str:
    """Text of the nearest dialog or modal ancestor, so a bare "Yes" is read in context."""
    current = getattr(node, "parent_node", None)
    depth = 0
    while current is not None and depth < 60:
        attributes = getattr(current, "attributes", None) or {}
        if marks_dialog(getattr(current, "node_name", "") or "", attributes):
            return " ".join(_all_text(current, 4000).split())[:CONTEXT_MAX]
        current = getattr(current, "parent_node", None)
        depth += 1
    return ""


def _frame_url(node) -> str:
    """src of the nearest enclosing iframe/frame, walking up parent_node."""
    current = getattr(node, "parent_node", None)
    depth = 0
    while current is not None and depth < 200:
        if (getattr(current, "node_name", "") or "").upper() in ("IFRAME", "FRAME"):
            # A frame injected without a src (chat and support widgets) is still someone else's frame.
            return (getattr(current, "attributes", None) or {}).get("src", "") or "about:blank"
        current = getattr(current, "parent_node", None)
        depth += 1
    return ""


def element_from_node(index: int, node) -> Element:
    """An Element from a browser-use EnhancedDOMTreeNode."""
    attributes = dict(getattr(node, "attributes", None) or {})
    tag = (getattr(node, "node_name", "") or "").lower()
    ax = getattr(node, "ax_node", None)
    role = (getattr(ax, "role", None) or attributes.get("role") or "").lower()
    name = getattr(ax, "name", None) or attributes.get("aria-label") or ""
    try:
        # Every descendant, shadow roots included: `<div role=button><span><span><b>Pay ₹500</b>`
        # has its only words levels down, and a custom element may keep them in its shadow root.
        children = _all_text(node)
    except Exception:
        children = ""
    full_text = _head_and_tail(" ".join(f"{name} {children}".split()), FULL_TEXT_MAX)
    return Element(
        index=index,
        tag=tag,
        role=role,
        name=_clip(name or children, NAME_MAX),
        full_text=full_text,
        context_text=_dialog_context(node),
        input_type=attributes.get("type", "").lower() if tag == "input" else "",
        autocomplete=attributes.get("autocomplete", ""),
        placeholder=_clip(attributes.get("placeholder", ""), NAME_MAX),
        value=_clip(attributes.get("value", ""), 40) if tag in ("input", "select", "textarea") else "",
        frame_url=_frame_url(node),
        attributes=attributes,
    )


def elements_from_selector_map(selector_map: dict) -> list[Element]:
    return [element_from_node(index, node) for index, node in sorted(selector_map.items())]


def kind_of(element: Element) -> str:
    """click | type | select: which target head an element belongs to. The same predicates
    the guards use, so eve is never offered a TYPE the guard would refuse."""
    if is_selectable(element):
        return "select"
    if is_typeable(element):
        return "type"
    return "click"


def label(element: Element) -> str:
    """One option line: `[12] button Search · current value`."""
    kind = element.role or element.input_type or element.tag
    name = element.name or element.placeholder or element.attributes.get("title", "") or "(no label)"
    text = f"[{element.index}] {kind} {name}"
    if kind_of(element) in ("type", "select"):
        # A personal or sensitive value is never shown, to eve or in the step log: only whether
        # the field is filled. The decision needs no more than that.
        if element.value and (is_personal(element) or never_type_reason(element)
                              or looks_like_card_number(element.value)):
            text += " · filled"
        else:
            text += f" · {element.value or 'empty'}"
    if element.frame_url:
        text += " · in frame"
    return _clip(text, OPTION_NAME_MAX)


def table(elements: list[Element], budget_tokens: int) -> str:
    """The element table as text, cut to `budget_tokens` (DOM order kept)."""
    lines: list[str] = []
    used = 0
    for element in elements:
        line = label(element)
        cost = estimate_tokens(line) + 1
        if used + cost > budget_tokens:
            lines.append(f"… {len(elements) - len(lines)} more elements not shown")
            break
        lines.append(line)
        used += cost
    return "\n".join(lines)


@dataclass(frozen=True)
class Region:
    """A run of consecutive candidates, offered as one option when there are
    more candidates than one question may list."""

    members: tuple[Element, ...]

    @property
    def option(self) -> str:
        first, last = self.members[0], self.members[-1]
        if len(self.members) == 1:
            return label(first)
        return _clip(f"elements [{first.index}]–[{last.index}]: {first.name or first.tag} … {last.name or last.tag}",
                     OPTION_NAME_MAX)


def regions(candidates: list[Element], max_options: int = MAX_OPTIONS) -> list[Region]:
    """Split candidates into at most `max_options` regions of consecutive
    elements, each holding at most `max_options` members, so a region round
    then a target round always fits. Raises when even that cannot fit
    (more than max_options² candidates); the caller trims first."""
    if len(candidates) > max_options * max_options:
        raise ValueError(f"{len(candidates)} candidates exceed {max_options}² for a two-round pick")
    count = -(-len(candidates) // max_options)  # regions needed so each holds <= max_options
    count = max(1, min(max_options, count))
    size = -(-len(candidates) // count)
    return [Region(tuple(candidates[i: i + size])) for i in range(0, len(candidates), size)]
