//! The decision request: one `/systemone` call asks which operation to do next
//! and, for every operation the page offers, which target it would use. Only
//! the target head of the chosen operation can cause an action.
//!
//! Two places where the live services differ from the pinned contract
//! (`createSystemOne` in `contracts/wally-decisions-public-v1.openapi.json`):
//! eve leaves `confidence` out, so it is derived here when absent; and the
//! contract caps a choice at 26 options while jev-latest takes 64 (measured:
//! 64 answered, 80 refused), so the cap is per model.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::account::systemone_contract::{SystemOneAnswer, SystemOneResponse};

use super::json::Json;
use super::page::{Action, ActionKind, Observation};
use super::prompts::{NEXT_ACTION, TARGET};
use super::Step;

pub const NONE: &str = "none";
const EVE_OPTION_CAP: usize = 26;
const OPTION_CAPS: &[(&str, usize)] = &[("jev-latest", 64)];

pub fn option_cap(model: &str) -> usize {
    OPTION_CAPS
        .iter()
        .find(|(id, _)| *id == model)
        .map_or(EVE_OPTION_CAP, |(_, cap)| *cap)
}

/// Targets grouped by operation, in the order the page offered them.
#[derive(Debug, Clone, Default)]
pub struct ActionSpace {
    pub elements: Vec<Json>,
    pub targets: Vec<(String, Vec<(String, Action)>)>,
    pub controls: Vec<(String, Action)>,
}

impl ActionSpace {
    pub fn targets_for(&self, operation: &str) -> Option<&[(String, Action)]> {
        self.targets
            .iter()
            .find(|(name, _)| name == operation)
            .map(|(_, targets)| targets.as_slice())
    }
}

fn operation_name(kind: ActionKind) -> Option<&'static str> {
    match kind {
        ActionKind::Click => Some("CLICK"),
        ActionKind::Fill => Some("TYPE_TEXT"),
        ActionKind::Select => Some("SELECT"),
        ActionKind::Scroll | ActionKind::Wait => None,
    }
}

fn short_label(label: &str) -> &str {
    label.split(" → ").next().unwrap_or(label)
}

/// One index per observed element; each operation has its own valid targets.
pub fn action_space(actions: &[Action]) -> ActionSpace {
    let mut space = ActionSpace::default();
    let mut indices: Vec<(u64, usize)> = Vec::new();
    for action in actions {
        let Some(operation) = operation_name(action.kind) else {
            space
                .controls
                .push((action.id.to_uppercase(), action.clone()));
            continue;
        };
        let node = action.node.unwrap_or_default();
        let position = match indices.iter().find(|(n, _)| *n == node) {
            Some((_, position)) => *position,
            None => {
                let index = (space.elements.len() + 1).to_string();
                let mut element = Json::object();
                for (key, value) in [
                    ("role", &action.role),
                    ("value", &action.value),
                    ("checked", &action.checked),
                    ("selected", &action.selected),
                    ("expanded", &action.expanded),
                ] {
                    if let Some(value) = value {
                        element.set(key, value);
                    }
                }
                element.set("index", index);
                element.set("label", short_label(&action.label));
                element.set("operations", Json::Array(Vec::new()));
                if action.kind == ActionKind::Select {
                    element.set("value", action.current_value.clone().unwrap_or_default());
                    element.set("options", Json::Array(Vec::new()));
                }
                space.elements.push(element);
                indices.push((node, space.elements.len() - 1));
                space.elements.len() - 1
            }
        };
        let index = (position + 1).to_string();
        let element = &mut space.elements[position];
        if let Some(Json::Array(operations)) = element_member(element, "operations") {
            if !operations.iter().any(|op| op == &Json::from(operation)) {
                operations.push(operation.into());
            }
        }
        let mut target = index.clone();
        if action.kind == ActionKind::Select {
            if let Some(Json::Array(options)) = element_member(element, "options") {
                target = format!("{index}:{}", options.len() + 1);
                options.push(
                    Json::object()
                        .with("index", target.clone())
                        .with("label", &action.label)
                        .with("value", action.value.clone().unwrap_or_default()),
                );
            }
        }
        match space.targets.iter_mut().find(|(name, _)| name == operation) {
            Some((_, group)) => group.push((target, action.clone())),
            None => space
                .targets
                .push((operation.to_string(), vec![(target, action.clone())])),
        }
    }
    space
}

fn element_member<'a>(element: &'a mut Json, key: &str) -> Option<&'a mut Json> {
    match element {
        Json::Object(members) => members
            .iter_mut()
            .find(|(name, _)| name == key)
            .map(|m| &mut m.1),
        _ => None,
    }
}

/// The element as Jev Ultrafast describes it: label, current value, and state.
pub fn criterion(index: &str, action: &Action) -> Json {
    let element: String = format!("[{index}] {}", action.label)
        .chars()
        .take(240)
        .collect();
    let current = action
        .current_value
        .clone()
        .or_else(|| action.value.clone())
        .unwrap_or_default();
    let mut criterion = Json::object()
        .with("element", element)
        .with("current_value", current);
    for (key, value) in [
        ("role", &action.role),
        ("checked", &action.checked),
        ("selected", &action.selected),
        ("expanded", &action.expanded),
    ] {
        if let Some(value) = value {
            criterion.set(key, value);
        }
    }
    criterion
}

fn chunks(count: usize, limit: usize) -> (usize, usize) {
    let size = if count <= limit { limit } else { limit - 1 };
    (size, count.div_ceil(size))
}

/// One question, or several, so each choice stays within the model's cap.
/// Split questions each add a `none` option.
pub fn target_questions(
    goal: &str,
    operation: &str,
    candidates: &[(String, Action)],
    limit: usize,
    today: &str,
) -> Vec<(String, Json)> {
    let name = format!("{}_target", operation.to_lowercase());
    if candidates.len() < 2 {
        return Vec::new();
    }
    let question = |part: &[(String, Action)], extra: bool| {
        let mut criteria = Json::object();
        for (index, action) in part {
            criteria.set(index, criterion(index, action));
        }
        if extra {
            criteria.set(NONE, "None of these is the right target.");
        }
        Json::object().with("type", "choice").with("criteria", criteria).with(
            "instructions",
            Json::object()
                .with("goal", goal)
                .with("operation", operation)
                .with("today", today)
                .with(
                    "rules",
                    vec![
                        NEXT_ACTION.into(),
                        TARGET.into(),
                        format!(
                            "Today is {today}. If the goal says today, choose the calendar day for that date when it is in this list."
                        )
                        .into(),
                    ],
                ),
        )
    };
    let (size, count) = chunks(candidates.len(), limit);
    if count == 1 {
        return vec![(name, question(candidates, false))];
    }
    candidates
        .chunks(size)
        .enumerate()
        .map(|(n, part)| (format!("{name}_{}", n + 1), question(part, true)))
        .collect()
}

pub const OPERATION_LABELS: &[(&str, &str)] = &[
    ("CLICK", "Click an element, button, menu option, autocomplete suggestion, or calendar day."),
    (
        "TYPE_TEXT",
        "Enter or replace text in an editable field. A small LLM will supply the value from the goal.",
    ),
    ("SELECT", "Select an observed dropdown value."),
];

/// The operation choice: every operation the page offers, its controls, then
/// DONE and BLOCKED.
pub fn operation_criteria(space: &ActionSpace) -> Json {
    let mut criteria = Json::object();
    for (operation, _) in &space.targets {
        let label = OPERATION_LABELS
            .iter()
            .find(|(name, _)| name == operation)
            .map_or("", |(_, label)| label);
        criteria.set(operation, label);
    }
    for (name, action) in &space.controls {
        criteria.set(name, &action.label);
    }
    criteria.set("DONE", "Every requirement is visibly satisfied.");
    criteria.set("BLOCKED", "No supported operation can progress.");
    criteria
}

fn recent_actions(history: &[Step]) -> Json {
    let start = history.len().saturating_sub(10);
    Json::Array(
        history[start..]
            .iter()
            .map(|step| {
                Json::object()
                    .with("action", &step.action)
                    .with("kind", step.kind_name())
                    .with("text", step.text.clone())
                    .with("page_changed", step.page_changed)
            })
            .collect(),
    )
}

pub fn decision_request(
    model: &str,
    page: &Observation,
    goal: &str,
    history: &[Step],
    today: &str,
) -> (Json, ActionSpace, Json) {
    let space = action_space(&page.actions);
    let operations = operation_criteria(&space);
    let mut questions = Json::object().with(
        "operation",
        Json::object()
            .with("type", "choice")
            .with("criteria", operations.clone())
            .with(
                "instructions",
                Json::object().with("goal", goal).with("rules", NEXT_ACTION),
            ),
    );
    for (operation, candidates) in &space.targets {
        for (name, question) in
            target_questions(goal, operation, candidates, option_cap(model), today)
        {
            questions.set(&name, question);
        }
    }
    let state = Json::object()
        .with(
            "page",
            Json::object()
                .with("url", &page.url)
                .with("title", &page.title)
                .with("text", &page.text),
        )
        .with("elements", Json::Array(space.elements.clone()))
        .with("recent_actions", recent_actions(history));
    let body = Json::object()
        .with("model", model)
        .with("state", state)
        .with("questions", questions);
    (body, space, operations)
}

/// A checked choice: the label, its score per label, and confidence.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

/// The answer to `name`, checked against the labels that were offered.
/// Eve's scores are not renormalised to sum to 1.
pub fn validate_choice(raw: &Value, name: &str, ids: &[String]) -> Result<Choice, String> {
    let fail = |choice: Option<&str>, got: Vec<String>| {
        let mut expected = ids.to_vec();
        expected.sort();
        format!(
            "Invalid decision response; no action executed. question={name} choice={choice:?} \
             expected={expected:?} got={got:?}"
        )
    };
    let answer_raw = raw.get("answers").and_then(|a| a.get(name));
    let parsed = SystemOneResponse::from_json(raw).ok();
    let answer = parsed.as_ref().and_then(|r| r.answers.0.get(name));
    let Some(SystemOneAnswer::SystemOneChoiceAnswer(answer)) = answer else {
        return Err(fail(None, Vec::new()));
    };
    let probabilities = answer.probabilities.0.clone();
    let mut got: Vec<String> = probabilities.keys().cloned().collect();
    got.sort();
    let has_confidence = answer_raw
        .and_then(|a| a.get("confidence"))
        .is_some_and(|c| !c.is_null());
    let confidence = if has_confidence {
        answer.confidence
    } else {
        let top = probabilities.values().copied().fold(0.0_f64, f64::max);
        let width = probabilities.len().max(2) as f64;
        ((width * top - 1.0) / (width - 1.0)).clamp(0.0, 1.0)
    };
    let top = probabilities
        .values()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    let total: f64 = probabilities.values().sum();
    let mut offered = ids.to_vec();
    offered.sort();
    let valid = ids.contains(&answer.choice)
        && got == offered
        && probabilities
            .values()
            .chain(std::iter::once(&confidence))
            .all(|n| n.is_finite() && (0.0..=1.0).contains(n))
        && total > 0.0
        && probabilities
            .get(&answer.choice)
            .is_some_and(|p| *p >= top - 1e-6);
    if !valid {
        return Err(fail(Some(&answer.choice), got));
    }
    Ok(Choice {
        choice: answer.choice.clone(),
        confidence,
        probabilities,
    })
}

/// The target for `operation`: the one question's answer, or across split
/// questions the best-scored real target. Every chunk answering `none` is a
/// refusal, not a guess.
pub fn chosen_target(
    raw: &Value,
    operation: &str,
    candidates: &[(String, Action)],
    limit: usize,
) -> Result<Choice, String> {
    let name = format!("{}_target", operation.to_lowercase());
    if candidates.len() == 1 {
        let only = candidates[0].0.clone();
        return Ok(Choice {
            choice: only.clone(),
            confidence: 1.0,
            probabilities: BTreeMap::from([(only, 1.0)]),
        });
    }
    let ids: Vec<String> = candidates.iter().map(|(id, _)| id.clone()).collect();
    let (size, count) = chunks(ids.len(), limit);
    if count == 1 {
        return validate_choice(raw, &name, &ids);
    }
    let mut best: Option<Choice> = None;
    for (n, part) in ids.chunks(size).enumerate() {
        let mut allowed = part.to_vec();
        allowed.push(NONE.to_string());
        let answer = validate_choice(raw, &format!("{name}_{}", n + 1), &allowed)?;
        if answer.choice == NONE {
            continue;
        }
        let score = answer.probabilities[&answer.choice];
        if best
            .as_ref()
            .is_none_or(|b| score > b.probabilities[&b.choice])
        {
            best = Some(answer);
        }
    }
    let mut best = best.ok_or_else(|| {
        format!("The decision model found no target for {operation}; no action executed.")
    })?;
    best.probabilities.remove(NONE);
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn click(node: u64, label: &str) -> Action {
        Action {
            id: format!("e{node}"),
            kind: ActionKind::Click,
            node: Some(node),
            role: Some("button".into()),
            label: label.into(),
            value: Some(String::new()),
            ..Action::default()
        }
    }

    fn candidates(n: usize) -> Vec<(String, Action)> {
        (1..=n)
            .map(|i| (i.to_string(), click(i as u64, &format!("Item {i}"))))
            .collect()
    }

    fn choice(choice: &str, probabilities: &[(&str, f64)]) -> Value {
        let probabilities: serde_json::Map<String, Value> = probabilities
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
        json!({"type": "choice", "choice": choice, "confidence": 0.5, "probabilities": probabilities})
    }

    #[test]
    fn one_index_per_node_with_targets_grouped_by_operation() {
        let mut fill = click(2, "Where to?");
        fill.kind = ActionKind::Fill;
        fill.id = "e3".into();
        let mut open = click(2, "Open Where to?");
        open.id = "e4".into();
        let space = action_space(&[click(1, "Search"), fill, open]);
        assert_eq!(space.elements.len(), 2);
        let ids = |op: &str| -> Vec<String> {
            space
                .targets_for(op)
                .unwrap()
                .iter()
                .map(|(i, _)| i.clone())
                .collect()
        };
        assert_eq!(ids("CLICK"), ["1", "2"]);
        assert_eq!(ids("TYPE_TEXT"), ["2"]);
        assert_eq!(
            space.elements[1].get("operations"),
            Some(&Json::Array(vec!["TYPE_TEXT".into(), "CLICK".into()]))
        );
    }

    #[test]
    fn dropdown_options_are_numbered_under_their_element() {
        let option = |value: &str| Action {
            id: format!("e{value}"),
            kind: ActionKind::Select,
            node: Some(7),
            label: format!("Class → {value}"),
            value: Some(value.into()),
            current_value: Some("Economy".into()),
            ..Action::default()
        };
        let space = action_space(&[option("Business"), option("First")]);
        let ids: Vec<&str> = space
            .targets_for("SELECT")
            .unwrap()
            .iter()
            .map(|(i, _)| i.as_str())
            .collect();
        assert_eq!(ids, ["1:1", "1:2"]);
        assert_eq!(space.elements[0].get("value"), Some(&Json::from("Economy")));
    }

    #[test]
    fn scroll_and_wait_are_controls_named_by_their_id() {
        let wait = Action {
            id: "wait".into(),
            kind: ActionKind::Wait,
            label: "Wait".into(),
            ..Action::default()
        };
        let space = action_space(&[click(1, "Search"), wait]);
        assert_eq!(space.controls[0].0, "WAIT");
        let criteria = operation_criteria(&space);
        let Json::Object(members) = criteria else {
            panic!()
        };
        let names: Vec<&str> = members.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["CLICK", "WAIT", "DONE", "BLOCKED"]);
    }

    #[test]
    fn targets_within_the_cap_go_in_one_question() {
        let questions = target_questions("g", "CLICK", &candidates(26), 26, "2026-10-31");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].0, "click_target");
        assert!(questions[0].1.get("criteria").unwrap().get(NONE).is_none());
    }

    #[test]
    fn targets_over_the_cap_split_with_a_none_option_in_each() {
        let questions = target_questions("g", "CLICK", &candidates(30), 26, "2026-10-31");
        let names: Vec<&str> = questions.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["click_target_1", "click_target_2"]);
        for (_, question) in &questions {
            assert!(question.get("criteria").unwrap().get(NONE).is_some());
        }
    }

    #[test]
    fn a_single_target_needs_no_question() {
        assert!(target_questions("g", "CLICK", &candidates(1), 26, "2026-10-31").is_empty());
    }

    #[test]
    fn the_cap_is_per_model() {
        assert_eq!(option_cap("eve"), 26);
        assert_eq!(option_cap("jev-latest"), 64);
        assert_eq!(option_cap("something-new"), 26);
    }

    #[test]
    fn the_best_scored_chunk_wins() {
        let raw = json!({"answers": {
            "click_target_1": choice("2", &[("1", 0.1), ("2", 0.6), ("none", 0.3)]),
            "click_target_2": choice("3", &[("3", 0.8), ("4", 0.1), ("none", 0.1)]),
        }});
        let chosen = chosen_target(&raw, "CLICK", &candidates(4), 3).unwrap();
        assert_eq!(chosen.choice, "3");
        assert!(!chosen.probabilities.contains_key(NONE));
    }

    #[test]
    fn every_chunk_answering_none_is_refused() {
        let raw = json!({"answers": {
            "click_target_1": choice("none", &[("1", 0.1), ("2", 0.1), ("none", 0.8)]),
            "click_target_2": choice("none", &[("3", 0.1), ("4", 0.1), ("none", 0.8)]),
        }});
        let error = chosen_target(&raw, "CLICK", &candidates(4), 3).unwrap_err();
        assert!(error.contains("no target"), "{error}");
    }

    #[test]
    fn a_choice_outside_the_offered_labels_is_refused() {
        let raw = json!({"answers": {"q": choice("9", &[("1", 0.4), ("9", 0.6)])}});
        assert!(validate_choice(&raw, "q", &["1".into(), "2".into()]).is_err());
    }

    #[test]
    fn a_missing_label_is_refused() {
        let raw = json!({"answers": {"q": choice("1", &[("1", 0.9)])}});
        assert!(validate_choice(&raw, "q", &["1".into(), "2".into()]).is_err());
    }

    #[test]
    fn a_score_above_one_is_refused() {
        let raw = json!({"answers": {"q": choice("1", &[("1", 1.5), ("2", 0.1)])}});
        assert!(validate_choice(&raw, "q", &["1".into(), "2".into()]).is_err());
    }

    #[test]
    fn a_choice_that_is_not_the_top_score_is_refused() {
        let raw = json!({"answers": {"q": choice("1", &[("1", 0.2), ("2", 0.7)])}});
        assert!(validate_choice(&raw, "q", &["1".into(), "2".into()]).is_err());
    }

    #[test]
    fn a_missing_confidence_is_derived_from_the_top_score() {
        let raw = json!({"answers": {"q": {"type": "choice", "choice": "1",
            "probabilities": {"1": 0.8, "2": 0.1}}}});
        let answer = validate_choice(&raw, "q", &["1".into(), "2".into()]).unwrap();
        assert!(
            (answer.confidence - 0.6).abs() < 1e-9,
            "{}",
            answer.confidence
        );
    }

    #[test]
    fn the_request_matches_what_python_sent_for_the_recorded_page() {
        let recorded: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/browser/flights-systemone.json"
        ))
        .unwrap();
        let page = Observation::from_snapshot(
            &serde_json::from_str(include_str!(
                "../../tests/fixtures/browser/flights-snapshot.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let (body, _, _) = decision_request(
            "jev-latest",
            &page,
            recorded["goal"].as_str().unwrap(),
            &[],
            recorded["today"].as_str().unwrap(),
        );
        assert_eq!(body.to_value(), recorded["request"]);
        let order = |value: &Json| -> Vec<String> {
            match value {
                Json::Object(members) => members.iter().map(|(k, _)| k.clone()).collect(),
                _ => Vec::new(),
            }
        };
        // The recorded file lists them in this order; a Value cannot keep it.
        assert_eq!(
            order(body.get("questions").unwrap()),
            ["operation", "click_target", "type_text_target"]
        );
    }

    #[test]
    fn the_request_body_reads_back_through_the_generated_binding() {
        use crate::account::systemone_contract::SystemOneRequest;
        let page = Observation::from_snapshot(
            &serde_json::from_str(include_str!(
                "../../tests/fixtures/browser/flights-snapshot.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let (body, _, _) = decision_request("eve", &page, "find flights", &[], "2026-10-31");
        let value: Value = serde_json::from_slice(&body.to_bytes()).unwrap();
        let typed = SystemOneRequest::from_json(&value).unwrap();
        assert_eq!(typed.to_json(), value);
    }

    #[test]
    fn the_recorded_response_validates() {
        let recorded: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/browser/flights-systemone.json"
        ))
        .unwrap();
        let page = Observation::from_snapshot(
            &serde_json::from_str(include_str!(
                "../../tests/fixtures/browser/flights-snapshot.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let space = action_space(&page.actions);
        let chosen = chosen_target(
            &recorded["response"],
            "CLICK",
            space.targets_for("CLICK").unwrap(),
            64,
        )
        .unwrap();
        assert_eq!(
            chosen.choice,
            recorded["decision"]["target"].as_str().unwrap()
        );
    }
}
