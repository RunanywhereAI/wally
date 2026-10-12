"""TypeSafe makes choices; an optional small OpenAI-compatible model writes field values."""

import datetime
import json
import math
import os
import re
import time

import httpx

from .questions import NEXT_ACTION, TARGET, TEXT_VALUE

CLIENT = httpx.Client(http2=True, timeout=25)
# The hosted Eve route accepts at most 26 options in one choice question.
MAX_OPTIONS = 26
NONE = "none"


def post_json(url, key, body):
    for attempt in range(3):
        try:
            response = CLIENT.post(url, json=body, headers={"Authorization": f"Bearer {key}"})
        except httpx.HTTPError:
            raise RuntimeError("Model connection failed; no action executed.") from None
        if response.status_code in {429, 529, 503} and attempt < 2:
            time.sleep(0.5 * 2**attempt)
            continue
        if response.is_error:
            detail = response.text[:400].replace("\n", " ")
            raise RuntimeError(
                f"Model provider returned HTTP {response.status_code}; no action executed. {detail}"
            )
        return response.json()
    raise RuntimeError("Model unavailable")


def validate_choice(answer, ids):
    """Accept Eve's raw label scores. They are not renormalised to 1."""
    try:
        probabilities = answer["probabilities"]
        if "confidence" not in answer:
            top = max(probabilities.values()) if probabilities else 0
            width = max(len(probabilities), 2)
            answer["confidence"] = max(0.0, min(1.0, (width * top - 1) / (width - 1)))
        numbers = [*probabilities.values(), answer["confidence"]]
        total = sum(probabilities.values())
        valid = (
            answer["choice"] in ids
            and set(probabilities) == set(ids)
            and all(type(n) in (int, float) and math.isfinite(n) and 0 <= n <= 1 for n in numbers)
            and total > 0
            and probabilities[answer["choice"]] >= max(probabilities.values()) - 1e-6
        )
    except (KeyError, TypeError, ValueError):
        valid = False
    if not valid:
        choice = answer.get("choice") if isinstance(answer, dict) else None
        got = sorted(answer.get("probabilities", {})) if isinstance(answer, dict) else []
        raise ValueError(
            f"Invalid TypeSafe response; no action executed. choice={choice!r} "
            f"expected={sorted(ids)} got={got}"
        )
    return answer


def criterion(index, action):
    label = action["label"].split(" → ")[0]
    value = action.get("current_value", action.get("value", ""))
    line = f"[{index}] {action.get('role', '')} {label}".strip()
    if value:
        line += f" · {value}"
    return line[:240]


def chunked_target_questions(goal, operation, candidates, limit=MAX_OPTIONS):
    """One question, or several, so each choice stays within Eve's 26-option cap."""
    ids = list(candidates)
    name = operation.lower() + "_target"
    if len(ids) < 2:
        return {}

    def question(part, extra=None):
        criteria = {index: criterion(index, candidates[index]) for index in part}
        if extra:
            criteria[NONE] = "None of these is the right target."
        today = datetime.date.today().isoformat()
        return {
            "type": "choice",
            "criteria": criteria,
            "instructions": {
                "goal": goal,
                "operation": operation,
                "today": today,
                "rules": [
                    NEXT_ACTION,
                    TARGET,
                    f"Today is {today}. If the goal says today, choose the calendar day for that date when it is in this list.",
                ],
            },
        }

    size = limit if len(ids) <= limit else limit - 1
    parts = [ids[i : i + size] for i in range(0, len(ids), size)]
    if len(parts) == 1:
        return {name: question(parts[0])}
    return {f"{name}_{n}": question(part, extra=True) for n, part in enumerate(parts, 1)}


def chosen_target(result, operation, candidates, limit=MAX_OPTIONS):
    ids = list(candidates)
    name = operation.lower() + "_target"
    size = limit if len(ids) <= limit else limit - 1
    if len(ids) == 1:
        only = ids[0]
        return {"choice": only, "probabilities": {only: 1.0}, "confidence": 1.0}
    parts = [ids[i : i + size] for i in range(0, len(ids), size)]
    if len(parts) == 1:
        return validate_choice(result["answers"].get(name, {}), {i: candidates[i] for i in parts[0]})
    best = None
    for n, part in enumerate(parts, 1):
        allowed = {i: candidates[i] for i in part}
        allowed[NONE] = {}
        answer = validate_choice(result["answers"].get(f"{name}_{n}", {}), allowed)
        if answer["choice"] == NONE:
            continue
        if best is None or answer["probabilities"][answer["choice"]] > best["probabilities"][best["choice"]]:
            best = answer
    if best is None:
        raise ValueError(f"Eve found no target for {operation}; no action executed.")
    best["probabilities"] = {i: p for i, p in best["probabilities"].items() if i != NONE}
    return best


def action_space(actions):
    """One index per observed element; each operation has its own valid target choices."""
    elements, indices, targets, controls = [], {}, {}, {}
    operations = {"click": "CLICK", "fill": "TYPE_TEXT", "select": "SELECT"}
    for action in actions:
        kind = action["kind"]
        if kind not in operations:
            controls[action["id"].upper()] = action
            continue
        node = action["node"]
        if node not in indices:
            index = str(len(elements) + 1)
            indices[node] = index
            element = {k: action[k] for k in ("role", "value", "checked", "selected", "expanded") if k in action}
            element.update(index=index, label=action["label"].split(" → ")[0], operations=[])
            if kind == "select":
                element["value"] = action.get("current_value", "")
                element["options"] = []
            elements.append(element)
        index = indices[node]
        operation = operations[kind]
        group = targets.setdefault(operation, {})
        element = elements[int(index) - 1]
        if operation not in element["operations"]:
            element["operations"].append(operation)
        target = index
        if kind == "select":
            target = f"{index}:{len(element['options']) + 1}"
            element["options"].append({"index": target, "label": action["label"], "value": action["value"]})
        group[target] = action
    return elements, targets, controls


def choose(state, goal, history):
    elements, targets, controls = action_space(state["actions"])
    labels = {
        "CLICK": "Click an element, button, menu option, autocomplete suggestion, or calendar day.",
        "TYPE_TEXT": "Enter or replace text in an editable field. A small LLM will supply the value from the goal.",
        "SELECT": "Select an observed dropdown value.",
    }
    operations = {key: labels[key] for key in targets}
    operations.update({key: value["label"] for key, value in controls.items()})
    operations.update(DONE="Every requirement is visibly satisfied.", BLOCKED="No supported operation can progress.")
    questions = {
        "operation": {"type": "choice", "criteria": operations, "instructions": {"goal": goal, "rules": NEXT_ACTION}}
    }
    for operation, candidates in targets.items():
        questions.update(chunked_target_questions(goal, operation, candidates))
    body = {
        "model": os.environ.get("WALLY_BROWSER_DECISION_MODEL", "eve"),
        "state": {
            "page": {k: state[k] for k in ("url", "title", "text")},
            "elements": [
                f"[{e['index']}] {e.get('role', '')} {e.get('label', '')}".strip()[:240] for e in elements
            ],
            "recent_actions": [
                {k: h.get(k) for k in ("action", "kind", "text", "page_changed")} for h in history[-10:]
            ],
        },
        "questions": questions,
    }
    started = time.perf_counter()
    cloud = os.environ.get("WALLY_BROWSER_API_BASE", "https://inference.runanywhere.ai/v1").rstrip("/")
    key = os.environ.get("WALLY_BROWSER_API_KEY") or os.environ.get("TYPESAFE_API_KEY") or "local"
    result = post_json(cloud + "/systemone", key, body)
    operation_answer = validate_choice(result["answers"].get("operation", {}), operations)
    operation = operation_answer["choice"]
    target = None
    target_answer = None
    probabilities = {}
    if operation in targets:
        # Unused target heads cannot cause an action. Validate the head selected by the operation.
        target_answer = chosen_target(result, operation, targets[operation])
        target = target_answer["choice"]
        choice = targets[operation][target]["id"]
        probabilities = {
            a["id"]: target_answer["probabilities"].get(index, 0.0) for index, a in targets[operation].items()
        }
    else:
        choice = controls[operation]["id"] if operation in controls else operation
        probabilities[choice] = operation_answer["probabilities"][operation]
    return {
        "choice": choice,
        "operation": operation,
        "target": target,
        "confidence": operation_answer["confidence"],
        "probabilities": probabilities,
        "operation_probabilities": operation_answer["probabilities"],
        "target_probabilities": target_answer["probabilities"] if target_answer else {},
        "target_confidence": target_answer["confidence"] if target_answer else None,
        "raw_answers": result["answers"],
        "model": result["model"],
        "usage": result.get("usage", {}),
        "latency_ms": round((time.perf_counter() - started) * 1000),
        "request": body,
    }


def field_context(goal, action, page, history):
    """The focused box can rename itself. The click that opened it still has the real name."""
    field = {k: action.get(k) for k in ("label", "role", "value")}
    for item in reversed(history):
        if item.get("kind") != "click":
            continue
        opened = str(item.get("action") or "")
        if item.get("node") == action.get("node") and opened.startswith("Open "):
            field["name_after_focus"] = field.get("label")
            field["label"] = opened.removeprefix("Open ")
        break
    return {
        "goal": goal,
        "field": field,
        "page": {"title": page["title"], "text": page["text"][:6000]},
        "recent_actions": [{k: h.get(k) for k in ("kind", "action", "text")} for h in history[-6:]],
    }


EXPLICIT_NULL = object()


def parse_field_value(content):
    if not isinstance(content, str):
        return None
    text = content.strip()
    if text.startswith("```"):
        text = text.split("\n", 1)[-1]
        fence = text.rfind("```")
        if fence >= 0:
            text = text[:fence]
        text = text.strip()
    try:
        output = json.loads(text)
    except json.JSONDecodeError:
        return text if text and len(text) <= 200 else None
    if not isinstance(output, dict):
        return None
    if "text" in output and output["text"] is None:
        return EXPLICIT_NULL
    value = output.get("text")
    if not isinstance(value, str) or not value.strip() or len(value) > 2000:
        return None
    return value


def goal_strings(goal, today):
    """Strings the goal already contains, plus today's date when the goal says today."""
    stop = {
        "a", "an", "and", "for", "from", "in", "of", "on", "or", "the", "to", "with",
        "do", "not", "stop", "find",
    }
    found = []
    for phrase in re.findall(r"\b[A-Z][a-zA-Z]+(?:\s+[A-Z][a-zA-Z]+)*\b", goal or ""):
        if phrase.lower() not in stop and phrase not in found:
            found.append(phrase)
    if re.search(r"\btoday\b", goal or "", re.IGNORECASE):
        found.append(today)
    for word in re.findall(r"\b[a-zA-Z][a-zA-Z0-9-]{1,}\b", goal or ""):
        if word.lower() in stop or word in found:
            continue
        found.append(word)
    return found[:25]


def text_from_goal(context, key, base, today):
    """Eve chooses which goal string to type. Used when the text model returns null."""
    options = goal_strings(context.get("goal") or "", today)
    if len(options) < 2:
        return None
    field = context.get("field") or {}
    result = post_json(
        base + "/systemone",
        key,
        {
            "model": os.environ.get("WALLY_BROWSER_DECISION_MODEL", "eve"),
            "state": {
                "field": field,
                "recent_actions": (context.get("recent_actions") or [])[-3:],
            },
            "questions": {
                "text": {
                    "type": "choice",
                    "criteria": {option: "A string from the goal." for option in options},
                    "instructions": {
                        "goal": context.get("goal"),
                        "rules": (
                            "Choose the string to type into the field that was just opened. "
                            "The visible label can differ from that click. "
                            "The current value can be a leftover. "
                            "If the field is a date, choose the date."
                        ),
                    },
                }
            },
        },
    )
    answer = validate_choice(result["answers"].get("text", {}), {option: option for option in options})
    return answer["choice"]


def field_text(context):
    key = os.environ.get("WALLY_BROWSER_API_KEY") or os.environ.get("TEXT_MODEL_API_KEY")
    if not key:
        raise ValueError("Typing needs a Wally session; no text is guessed.")
    base = (os.environ.get("WALLY_BROWSER_API_BASE") or os.environ.get("TEXT_MODEL_BASE_URL") or "https://inference.runanywhere.ai/v1").rstrip("/")
    model = "deepseek-v4.1-flash"
    reasoning = {"reasoning_effort": "none"}
    started = time.perf_counter()
    today = datetime.date.today().isoformat()
    snippets = []
    value = None
    usage = {}
    result = post_json(
        base + "/chat/completions",
        key,
        {
            "model": model,
            "max_tokens": 256,
            "response_format": {"type": "json_object"},
            **reasoning,
            "messages": [
                {"role": "system", "content": TEXT_VALUE},
                {"role": "user", "content": json.dumps({**context, "today": today})},
            ],
        },
    )
    usage = result.get("usage", {})
    content = result.get("choices", [{}])[0].get("message", {}).get("content")
    snippets.append(str(content)[:240])
    value = parse_field_value(content)
    if value is EXPLICIT_NULL:
        raise ValueError("Text helper refused the field; nothing typed.")
    if not value:
        value = text_from_goal(context, key, base, today)
        if value:
            model = os.environ.get("WALLY_BROWSER_DECISION_MODEL", "eve")
    if not value:
        field = context.get("field")
        raise ValueError(
            f"Text helper returned no valid field value; nothing typed. field={field!r} reply={snippets[-1]!r}"
        )
    return value, {
        "model": model,
        "latency_ms": round((time.perf_counter() - started) * 1000),
        "usage": usage,
    }
