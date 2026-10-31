//! What to type into a field. A small chat model reads the goal and the field
//! and returns one JSON object; when it gives nothing usable, the decision
//! model picks one of the strings the goal already contains. Nothing is
//! invented: no value means nothing is typed.
//!
//! The chat call is OpenAI chat completions on the gateway. No contract in this
//! repo pins that route, so the request and the one response field read here
//! are the narrow typed structs below.

use serde::{Deserialize, Serialize};

use super::json::Json;
use super::page::{Action, ActionKind, Observation};
use super::words::{boundary, contains_phrase};
use super::Step;

/// The field, the page and the recent steps the text model is shown.
pub fn field_context(goal: &str, action: &Action, page: &Observation, history: &[Step]) -> Json {
    let mut field = Json::object()
        .with("label", &action.label)
        .with("role", action.role.clone())
        .with("value", action.value.clone());
    // A native date or time input only takes its own format.
    if let Some(kind) = &action.input_type {
        field.set("format", super::page::date_format(kind));
    }
    // The focused box can rename itself. The click that opened it still has the real name.
    if let Some(item) = history
        .iter()
        .rev()
        .find(|step| step.kind == ActionKind::Click)
    {
        if item.node == action.node {
            if let Some(opened) = item.action.strip_prefix("Open ") {
                field.set("name_after_focus", &action.label);
                field.set("label", opened);
            }
        }
    }
    let start = history.len().saturating_sub(6);
    Json::object()
        .with("goal", goal)
        .with("field", field)
        .with(
            "page",
            Json::object()
                .with("title", &page.title)
                .with("text", page.text.chars().take(6000).collect::<String>()),
        )
        .with(
            "recent_actions",
            Json::Array(
                history[start..]
                    .iter()
                    .map(|step| {
                        Json::object()
                            .with("kind", step.kind_name())
                            .with("action", &step.action)
                            .with("text", step.text.clone())
                    })
                    .collect(),
            ),
        )
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Text(String),
    /// The model said `{"text": null}`: the goal does not give this value.
    Refused,
    Unusable,
}

/// The value in the model's reply: a JSON `{"text": ...}`, possibly fenced, or
/// short plain text.
pub fn parse_field_value(content: &str) -> FieldValue {
    let mut text = content.trim();
    let unfenced;
    if text.starts_with("```") {
        let body = text.split_once('\n').map_or("", |(_, rest)| rest);
        let body = match body.rfind("```") {
            Some(fence) => &body[..fence],
            None => body,
        };
        unfenced = body.trim().to_string();
        text = &unfenced;
    }
    let Ok(output) = serde_json::from_str::<serde_json::Value>(text) else {
        let length = text.chars().count();
        return if length > 0 && length <= 200 {
            FieldValue::Text(text.to_string())
        } else {
            FieldValue::Unusable
        };
    };
    let Some(object) = output.as_object() else {
        return FieldValue::Unusable;
    };
    match object.get("text") {
        Some(serde_json::Value::Null) => FieldValue::Refused,
        Some(serde_json::Value::String(value))
            if !value.trim().is_empty() && value.chars().count() <= 2000 =>
        {
            FieldValue::Text(value.clone())
        }
        _ => FieldValue::Unusable,
    }
}

pub enum Resolved {
    /// The text model's own value.
    Typed(String),
    /// One of the goal's strings, picked by the decision model.
    FromGoal(String),
}

/// The value to type. A reply with no usable value, and an explicit refusal
/// too, falls back to the decision model choosing among the goal's own
/// strings: a refusal often means the field looked filled or misnamed (Google
/// renames "Where from?" to "Where else?" once it opens), and a string the
/// goal already contains invents nothing. `None` means nothing is typed.
pub fn resolve_field_value(
    parsed: FieldValue,
    pick_from_goal: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<Option<Resolved>, String> {
    match parsed {
        FieldValue::Text(value) => Ok(Some(Resolved::Typed(value))),
        FieldValue::Refused | FieldValue::Unusable => Ok(pick_from_goal()?.map(Resolved::FromGoal)),
    }
}

const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "for", "from", "in", "of", "on", "or", "the", "to", "with", "do", "not",
    "stop", "find",
];

fn is_stop(word: &str) -> bool {
    STOP_WORDS.contains(&word.to_lowercase().as_str())
}

/// Runs of capitalised words: `\b[A-Z][a-zA-Z]+(?:\s+[A-Z][a-zA-Z]+)*\b`.
fn capitalised_phrases(chars: &[char]) -> Vec<String> {
    let word_end = |start: usize| -> Option<usize> {
        if start >= chars.len() || !chars[start].is_ascii_uppercase() {
            return None;
        }
        let mut end = start + 1;
        while end < chars.len() && chars[end].is_ascii_alphabetic() {
            end += 1;
        }
        (end > start + 1).then_some(end)
    };
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let Some(first) = boundary(chars, at).then(|| word_end(at)).flatten() else {
            at += 1;
            continue;
        };
        if !boundary(chars, first) {
            at += 1;
            continue;
        }
        let mut end = first;
        loop {
            let mut next = end;
            while next < chars.len() && chars[next].is_whitespace() {
                next += 1;
            }
            match (next > end).then(|| word_end(next)).flatten() {
                Some(after) if boundary(chars, after) => end = after,
                _ => break,
            }
        }
        found.push(chars[at..end].iter().collect());
        at = end;
    }
    found
}

/// Words: `\b[a-zA-Z][a-zA-Z0-9-]{1,}\b`, backing off a trailing hyphen.
fn goal_words(chars: &[char]) -> Vec<String> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if !boundary(chars, at) || !chars[at].is_ascii_alphabetic() {
            at += 1;
            continue;
        }
        let mut greedy = at + 1;
        while greedy < chars.len()
            && (chars[greedy].is_ascii_alphanumeric() || chars[greedy] == '-')
        {
            greedy += 1;
        }
        match (at + 2..=greedy).rev().find(|&end| boundary(chars, end)) {
            Some(end) => {
                found.push(chars[at..end].iter().collect());
                at = end;
            }
            None => at += 1,
        }
    }
    found
}

/// Strings the goal already contains, plus today's date when the goal says today.
pub fn goal_strings(goal: &str, today: &str) -> Vec<String> {
    let chars: Vec<char> = goal.chars().collect();
    let mut found: Vec<String> = Vec::new();
    for phrase in capitalised_phrases(&chars) {
        if !is_stop(&phrase) && !found.contains(&phrase) {
            found.push(phrase);
        }
    }
    if contains_phrase(goal, "today") {
        found.push(today.to_string());
    }
    for word in goal_words(&chars) {
        if is_stop(&word) || found.contains(&word) {
            continue;
        }
        found.push(word);
    }
    found.truncate(25);
    found
}

#[derive(Debug, Serialize)]
pub struct ChatRequest<'a> {
    pub model: &'a str,
    pub max_tokens: u32,
    pub response_format: ResponseFormat,
    pub reasoning_effort: &'a str,
    pub messages: Vec<ChatMessage>,
}

#[derive(Debug, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ChatMessage {
    pub role: &'static str,
    pub content: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChatChoice {
    #[serde(default)]
    pub message: ChatReply,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChatReply {
    #[serde(default)]
    pub content: Option<String>,
}

pub fn chat_request<'a>(
    model: &'a str,
    context: &Json,
    today: &str,
    system: &str,
) -> ChatRequest<'a> {
    let user = context.clone().with("today", today);
    ChatRequest {
        model,
        max_tokens: 256,
        response_format: ResponseFormat {
            kind: "json_object",
        },
        reasoning_effort: "none",
        messages: vec![
            ChatMessage {
                role: "system",
                content: system.to_string(),
            },
            ChatMessage {
                role: "user",
                content: user.to_python(),
            },
        ],
    }
}

/// The decision request that picks one goal string for the field.
pub fn goal_string_question(model: &str, context: &Json, options: &[String]) -> Json {
    let mut criteria = Json::object();
    for option in options {
        criteria.set(option, "A string from the goal.");
    }
    let recent = match context.get("recent_actions") {
        Some(Json::Array(items)) => Json::Array(items[items.len().saturating_sub(3)..].to_vec()),
        _ => Json::Array(Vec::new()),
    };
    Json::object()
        .with("model", model)
        .with(
            "state",
            Json::object()
                .with(
                    "field",
                    context.get("field").cloned().unwrap_or(Json::object()),
                )
                .with("recent_actions", recent),
        )
        .with(
            "questions",
            Json::object().with(
                "text",
                Json::object()
                    .with("type", "choice")
                    .with("criteria", criteria)
                    .with(
                        "instructions",
                        Json::object()
                            .with("goal", context.get("goal").cloned().unwrap_or(Json::Null))
                            .with(
                                "rules",
                                "Choose the string to type into the field that was just opened. \
                             The visible label can differ from that click. \
                             The current value can be a leftover. \
                             If the field is a date, choose the date.",
                            ),
                    ),
            ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_null_is_a_refusal_not_an_empty_reply() {
        assert_eq!(parse_field_value(r#"{"text": null}"#), FieldValue::Refused);
    }

    #[test]
    fn short_plain_text_is_the_value() {
        assert_eq!(
            parse_field_value("not json at all"),
            FieldValue::Text("not json at all".into())
        );
    }

    #[test]
    fn an_empty_text_is_no_value() {
        assert_eq!(parse_field_value(r#"{"text": ""}"#), FieldValue::Unusable);
    }

    #[test]
    fn long_plain_text_is_no_value() {
        assert_eq!(parse_field_value(&"x".repeat(201)), FieldValue::Unusable);
    }

    #[test]
    fn a_fenced_reply_is_unwrapped() {
        let reply = "```json\n{\"text\": \"Bengaluru\"}\n```";
        assert_eq!(
            parse_field_value(reply),
            FieldValue::Text("Bengaluru".into())
        );
    }

    #[test]
    fn goal_strings_keep_phrases_and_drop_stop_words() {
        let strings = goal_strings(
            "Find flights from New Delhi to Bengaluru, one-way",
            "2026-10-31",
        );
        assert_eq!(
            strings[..2],
            ["New Delhi".to_string(), "Bengaluru".to_string()]
        );
        assert!(strings.contains(&"one-way".to_string()));
        assert!(!strings
            .iter()
            .any(|s| s == "Find" || s == "from" || s == "to"));
    }

    #[test]
    fn today_is_added_only_when_the_goal_says_today() {
        assert!(goal_strings("Book for today", "2026-10-31").contains(&"2026-10-31".to_string()));
        assert!(!goal_strings("Book for Friday", "2026-10-31").contains(&"2026-10-31".to_string()));
    }

    #[test]
    fn goal_strings_stop_at_twenty_five() {
        let goal: Vec<String> = (0..40).map(|i| format!("word{i}")).collect();
        assert_eq!(goal_strings(&goal.join(" "), "2026-10-31").len(), 25);
    }

    #[test]
    fn a_trailing_hyphen_is_not_part_of_a_word() {
        assert_eq!(
            goal_words(&"abc- def".chars().collect::<Vec<_>>()),
            ["abc", "def"]
        );
    }

    #[test]
    fn the_field_name_comes_from_the_click_that_opened_it() {
        let field = Action {
            kind: ActionKind::Fill,
            node: Some(4),
            label: "Where else?".into(),
            ..Action::default()
        };
        let opened = Step {
            action: "Open Where from?".into(),
            kind: ActionKind::Click,
            node: Some(4),
            ..Step::default()
        };
        let page = Observation {
            url: String::new(),
            title: "Flights".into(),
            text: String::new(),
            actions: Vec::new(),
            marker: serde_json::Value::Null,
            page_key: serde_json::Value::Null,
            guards: serde_json::Value::Null,
            fingerprint: String::new(),
            payment_fields: false,
        };
        let context = field_context("goal", &field, &page, &[opened]);
        let field = context.get("field").unwrap();
        assert_eq!(field.get("label"), Some(&Json::from("Where from?")));
        assert_eq!(
            field.get("name_after_focus"),
            Some(&Json::from("Where else?"))
        );
    }

    #[test]
    fn a_refusal_falls_back_to_a_goal_string() {
        let resolved = resolve_field_value(FieldValue::Refused, || Ok(Some("Bengaluru".into())));
        assert!(matches!(resolved, Ok(Some(Resolved::FromGoal(v))) if v == "Bengaluru"));
    }

    #[test]
    fn a_refusal_with_no_goal_string_types_nothing() {
        assert!(matches!(
            resolve_field_value(FieldValue::Refused, || Ok(None)),
            Ok(None)
        ));
    }

    #[test]
    fn the_text_models_value_skips_the_fallback() {
        let resolved = resolve_field_value(FieldValue::Text("Delhi".into()), || {
            panic!("the fallback must not be asked")
        });
        assert!(matches!(resolved, Ok(Some(Resolved::Typed(v))) if v == "Delhi"));
    }
}
