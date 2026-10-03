//! Hosted decision scoring (`wally decisions`, alias `decide`): a decision
//! model answers typed questions with label probabilities instead of text.

use std::collections::BTreeSet;
use std::io::Read;
use std::time::Instant;

use crate::account::decisions_contract as contract;
use crate::account::{self, ConsoleClient, ConsoleSession};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output as out;
use crate::util::term;

const DEFAULT_MODEL: &str = crate::harness::DEFAULT_DECISIONS_MODEL;
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The pinned contract, read for its closed objects when a `--request` file is
/// checked for fields the server would refuse (or this CLI would drop).
const CONTRACT: &str = include_str!("../../contracts/wally-decisions-public-v1.openapi.json");
/// Widest label column in the human rendering; longer labels are cut.
const LABEL_MAX_CHARS: usize = 32;

fn read_stdin() -> Result<String, String> {
    let mut value = String::new();
    std::io::stdin()
        .read_to_string(&mut value)
        .map_err(|error| format!("could not read stdin: {error}"))?;
    Ok(value)
}

fn read_path(path: &str) -> Result<String, String> {
    if path == "-" {
        read_stdin()
    } else {
        std::fs::read_to_string(path).map_err(|error| format!("could not read {path}: {error}"))
    }
}

fn nonblank(value: &str, label: &str, maximum: usize) -> Result<(), String> {
    if value.is_empty() || !value.chars().any(|c| !c.is_whitespace()) {
        return Err(format!("{label} must not be blank"));
    }
    if value.chars().count() > maximum {
        return Err(format!("{label} may be at most {maximum} characters"));
    }
    Ok(())
}

fn validate(request: &contract::DecisionsRequest) -> Result<(), String> {
    nonblank(&request.input, "input", 1_000_000)?;
    nonblank(&request.model, "model", 128)?;
    let mut chars = request.model.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        || !request
            .model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
    {
        return Err("model must be a valid Wally model id".to_string());
    }
    if !(1..=128).contains(&request.questions.len()) {
        return Err("provide 1 to 128 questions".to_string());
    }
    if request
        .temperature
        .is_some_and(|value| value <= 0.0 || value > 100.0)
    {
        return Err("temperature must be greater than 0 and at most 100".to_string());
    }
    let mut ids = BTreeSet::new();
    for question in &request.questions {
        let (id, text, count) = match question {
            contract::DecisionQuestion::YesNoQuestion(value) => {
                if let Some(yes) = &value.yes {
                    nonblank(yes, "yes description", 65_536)?;
                }
                if let Some(no) = &value.no {
                    nonblank(no, "no description", 65_536)?;
                }
                (&value.id, &value.question, 2)
            }
            contract::DecisionQuestion::ChoiceQuestion(value) => {
                if !(2..=26).contains(&value.options.len()) {
                    return Err(format!("choice {} needs 2 to 26 options", value.id));
                }
                let mut names = BTreeSet::new();
                for option in &value.options {
                    nonblank(&option.name, "choice option", 256)?;
                    if !names.insert(option.name.trim().to_ascii_lowercase()) {
                        return Err(format!("choice {} has duplicate options", value.id));
                    }
                    if let Some(description) = &option.description {
                        nonblank(description, "choice description", 65_536)?;
                    }
                }
                (&value.id, &value.question, value.options.len())
            }
            contract::DecisionQuestion::ScoreQuestion(value) => {
                if !(2..=10).contains(&value.levels.len()) {
                    return Err(format!("score {} needs 2 to 10 levels", value.id));
                }
                for level in &value.levels {
                    nonblank(level, "score level", 65_536)?;
                }
                (&value.id, &value.question, value.levels.len())
            }
        };
        nonblank(id, "question id", 128)?;
        nonblank(text, "question", 65_536)?;
        if !ids.insert(id.clone()) {
            return Err(format!("duplicate question id: {id}"));
        }
        if count < 2 {
            return Err(format!("question {id} needs at least two labels"));
        }
    }
    let bytes = crate::io::json::dump(&request.to_json());
    if bytes.len() > MAX_BODY_BYTES {
        return Err("request body exceeds 1 MiB".to_string());
    }
    Ok(())
}

fn split_question_values(raw: &str, kind: &str) -> Result<(String, Vec<String>), String> {
    let (question, values) = raw
        .split_once('=')
        .ok_or_else(|| format!("{kind} must use 'QUESTION=value1,value2'"))?;
    nonblank(question, kind, 65_536)?;
    let values: Vec<String> = values
        .split(',')
        .map(|value| value.trim().to_string())
        .collect();
    if values.iter().any(|value| value.is_empty()) {
        return Err(format!("{kind} values must not be blank"));
    }
    Ok((question.trim().to_string(), values))
}

fn build_request(p: &crate::cli::Parsed) -> Result<contract::DecisionsRequest, String> {
    if let Some(path) = p.get_str("--request") {
        let conflicting = [
            "--input",
            "--input-file",
            "--ask",
            "--choice",
            "--score",
            "--temperature",
            "--model",
        ]
        .into_iter()
        .filter(|name| p.is_set(name))
        .collect::<Vec<_>>();
        if !conflicting.is_empty() {
            return Err(format!(
                "--request cannot be combined with {}",
                conflicting.join(", ")
            ));
        }
        let raw = read_path(&path)?;
        if raw.len() > MAX_BODY_BYTES {
            return Err("request body exceeds 1 MiB".to_string());
        }
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| format!("invalid request JSON: {error}"))?;
        // The generated reader is tolerant (right for responses), so a
        // misspelled field would be dropped and the request sent without it.
        // The contract closes every request object; hold the file to that.
        let unknown = unknown_request_fields(&value);
        if !unknown.is_empty() {
            return Err(format!(
                "request has fields the decisions contract does not define: {}",
                unknown.join(", ")
            ));
        }
        let request = contract::DecisionsRequest::from_json(&value)
            .map_err(|error| format!("request does not match the decisions contract: {error}"))?;
        validate(&request)?;
        return Ok(request);
    }

    if p.is_set("--input") && p.is_set("--input-file") {
        return Err("--input and --input-file are mutually exclusive".to_string());
    }
    let input = if let Some(value) = p.get_str("--input") {
        value
    } else if let Some(path) = p.get_str("--input-file") {
        read_path(&path)?
    } else if !term::stdin_is_tty() {
        read_stdin()?
    } else {
        return Err("provide --input TEXT, --input-file PATH, or pipe stdin".to_string());
    };

    let mut questions = Vec::new();
    for question in p.get_strs("--ask") {
        let id = format!("q{}", questions.len() + 1);
        questions.push(contract::DecisionQuestion::YesNoQuestion(
            contract::YesNoQuestion {
                id,
                question,
                r#type: "yes_no".to_string(),
                ..Default::default()
            },
        ));
    }
    for raw in p.get_strs("--choice") {
        let (question, names) = split_question_values(&raw, "--choice")?;
        let id = format!("q{}", questions.len() + 1);
        questions.push(contract::DecisionQuestion::ChoiceQuestion(
            contract::ChoiceQuestion {
                id,
                question,
                r#type: "choice".to_string(),
                options: names
                    .into_iter()
                    .map(|name| contract::DecisionOption {
                        name,
                        ..Default::default()
                    })
                    .collect(),
            },
        ));
    }
    for raw in p.get_strs("--score") {
        let (question, levels) = split_question_values(&raw, "--score")?;
        let id = format!("q{}", questions.len() + 1);
        questions.push(contract::DecisionQuestion::ScoreQuestion(
            contract::ScoreQuestion {
                id,
                question,
                levels,
                r#type: "score".to_string(),
            },
        ));
    }
    if questions.is_empty() {
        return Err(
            "no question given; example: wally decisions --input 'The deploy failed' --ask 'Did it fail?'"
                .to_string(),
        );
    }
    let request = contract::DecisionsRequest {
        model: p
            .get_str("--model")
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        input,
        questions,
        temperature: p.get_f64("--temperature"),
        prompt_format_version: None,
    };
    validate(&request)?;
    Ok(request)
}

/// Walks `value` against the contract schema `schema`, collecting the paths of
/// fields a closed object (`additionalProperties: false`) does not define.
/// Follows `$ref`s, picks a discriminated union's branch by its tag, and
/// descends into properties and array items.
fn collect_unknown_fields(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
    unknown: &mut Vec<String>,
) {
    let mut schema = schema;
    while let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
        match reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
        {
            Some(target) => schema = target,
            None => return,
        }
    }
    if let (Some(mapping), Some(tag)) = (
        schema.pointer("/discriminator/mapping"),
        schema
            .pointer("/discriminator/propertyName")
            .and_then(serde_json::Value::as_str),
    ) {
        if let Some(branch) = value
            .get(tag)
            .and_then(serde_json::Value::as_str)
            .and_then(|kind| mapping.get(kind))
            .and_then(serde_json::Value::as_str)
            .and_then(|reference| reference.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            collect_unknown_fields(value, branch, root, path, unknown);
        }
        return;
    }
    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                collect_unknown_fields(
                    item,
                    item_schema,
                    root,
                    &format!("{path}[{index}]"),
                    unknown,
                );
            }
        }
        return;
    }
    let Some(object) = value.as_object() else {
        return;
    };
    let properties = schema
        .get("properties")
        .and_then(serde_json::Value::as_object);
    let closed = schema.get("additionalProperties") == Some(&serde_json::Value::Bool(false));
    for (key, field) in object {
        let field_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        match properties.and_then(|properties| properties.get(key)) {
            Some(field_schema) => {
                collect_unknown_fields(field, field_schema, root, &field_path, unknown)
            }
            None if closed => unknown.push(field_path),
            None => {}
        }
    }
}

/// Fields of a `--request` document that the contract's DecisionsRequest
/// does not define, as dotted paths (`questions[0].opitons`).
fn unknown_request_fields(value: &serde_json::Value) -> Vec<String> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(CONTRACT) else {
        return Vec::new();
    };
    let Some(schema) = root.pointer("/components/schemas/DecisionsRequest") else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    collect_unknown_fields(value, schema, &root, "", &mut unknown);
    unknown
}

/// One question as the human rendering shows it: its text, and its labels in
/// the order the question listed them, each with the probability key the
/// server answers under. A score question's levels come back keyed "0" to
/// "9", so the level text the person wrote is what gets printed.
struct QuestionView {
    text: String,
    labels: Vec<(String, String)>,
}

fn question_views(request: &contract::DecisionsRequest) -> Vec<(String, QuestionView)> {
    request
        .questions
        .iter()
        .map(|question| match question {
            contract::DecisionQuestion::YesNoQuestion(value) => (
                value.id.clone(),
                QuestionView {
                    text: value.question.clone(),
                    labels: ["yes", "no"]
                        .iter()
                        .map(|label| (label.to_string(), label.to_string()))
                        .collect(),
                },
            ),
            contract::DecisionQuestion::ChoiceQuestion(value) => (
                value.id.clone(),
                QuestionView {
                    text: value.question.clone(),
                    labels: value
                        .options
                        .iter()
                        .map(|option| (option.name.clone(), option.name.clone()))
                        .collect(),
                },
            ),
            contract::DecisionQuestion::ScoreQuestion(value) => (
                value.id.clone(),
                QuestionView {
                    text: value.question.clone(),
                    labels: value
                        .levels
                        .iter()
                        .enumerate()
                        .map(|(index, level)| (index.to_string(), level.clone()))
                        .collect(),
                },
            ),
        })
        .collect()
}

/// A label for one terminal column: control characters dropped, cut to
/// LABEL_MAX_CHARS with an ellipsis.
fn display_label(label: &str) -> String {
    let clean: String = label.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() <= LABEL_MAX_CHARS {
        clean
    } else {
        let cut: String = clean.chars().take(LABEL_MAX_CHARS - 1).collect();
        format!("{cut}…")
    }
}

/// The question's answer as rows of (display label, probability), in the
/// question's own label order. A label the server answered that the question
/// did not list (it should not happen) follows, under its key.
fn answer_rows(view: &QuestionView, answer: &contract::DecisionAnswer) -> Vec<(String, f64)> {
    let mut rows: Vec<(String, f64)> = view
        .labels
        .iter()
        .filter_map(|(key, label)| {
            answer
                .probabilities
                .0
                .get(key)
                .map(|probability| (display_label(label), *probability))
        })
        .collect();
    for (key, probability) in &answer.probabilities.0 {
        if !view.labels.iter().any(|(listed, _)| listed == key) {
            rows.push((display_label(key), *probability));
        }
    }
    rows
}

/// The most probable row. On a tie the earlier label wins, so the headline
/// follows the question's own order rather than whichever came last.
fn top_row(rows: &[(String, f64)]) -> Option<&(String, f64)> {
    rows.iter()
        .fold(None, |best: Option<&(String, f64)>, row| match best {
            Some(current) if current.1 >= row.1 => Some(current),
            _ => Some(row),
        })
}

fn render_human(request: &contract::DecisionsRequest, response: &contract::DecisionsResponse) {
    for (id, view) in question_views(request) {
        let Some(answer) = response.answers.0.get(&id) else {
            continue;
        };
        out::result_line(&display_label_line(&view.text));
        let rows = answer_rows(&view, answer);
        if let Some((label, probability)) = top_row(&rows) {
            out::result_line(&format!("{label}  {:.1}%", probability * 100.0));
        }
        let width = rows
            .iter()
            .map(|(label, _)| label.chars().count())
            .max()
            .unwrap_or(0)
            .max(12);
        for (label, probability) in &rows {
            let bar = (probability * 20.0).round().clamp(0.0, 20.0) as usize;
            let pad = width - label.chars().count();
            out::result_line(&format!(
                "  {label}{} {:<20} {:>5.1}%",
                " ".repeat(pad),
                "█".repeat(bar),
                probability * 100.0
            ));
        }
        if answer.label_mass < 0.5 {
            out::result_line("  note: low fit — most probability was outside these labels");
        }
        out::result_line("");
    }
}

/// A question's text on one line: control characters (a newline in a long
/// question) dropped.
fn display_label_line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn run(p: &crate::cli::Parsed, json: bool) -> i32 {
    let request = match build_request(p) {
        Ok(request) => request,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };
    let credentials = match account::load() {
        Ok(credentials) if credentials.signed_in() => credentials,
        Ok(_) => {
            out::error_line("Sign in first: wally account login");
            return 1;
        }
        Err(error) => {
            out::error_line(&error);
            return 1;
        }
    };
    let mut session = match ConsoleSession::resume(
        ConsoleClient::default(),
        credentials,
        account::epoch_seconds(),
    ) {
        Ok(session) => session,
        Err(error) => {
            out::error_line(&error);
            return 1;
        }
    };
    let no_retry = p.flag("--no-retry");
    let started = Instant::now();
    let result = session.call(|client, console_url, access_token| {
        client.create_decisions(
            console_url,
            access_token,
            &request,
            no_retry,
            Some(&|status, wait| {
                out::status_line(&format!(
                    "{} returned HTTP {status}; retrying{}",
                    request.model,
                    if wait > 0 {
                        format!(" in {wait}s")
                    } else {
                        String::new()
                    }
                ));
            }),
        )
    });
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            out::error_line(&error);
            return 1;
        }
    };
    let latency_ms = started.elapsed().as_millis();
    if json {
        out::result_line(&result.raw_json);
    } else {
        render_human(&request, &result.response);
    }
    let usage = &result.response.usage;
    let cost = session
        .call(|client, console_url, access_token| client.fetch_catalog(console_url, access_token))
        .ok()
        .and_then(|prices| {
            prices
                .into_iter()
                .find(|price| price.id == result.response.model)
        })
        .map(|price| {
            let micros = usage.prompt_tokens.saturating_mul(price.input_per_mtok) / 1_000_000;
            format!("${:.6}", micros as f64 / 1_000_000.0)
        })
        .unwrap_or_else(|| "cost unavailable".to_string());
    out::status_line(&format!(
        "{} · {} tokens · {} · {}ms",
        result.response.model, usage.total_tokens, cost, latency_ms
    ));
    0
}

pub fn register_decisions(app: &mut App) {
    let cmd = app.add_subcommand("decisions", "Score questions with a hosted decision model");
    cmd.alias("decide");
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("Decision model (default {DEFAULT_MODEL})"),
    )
    .default_val(DEFAULT_MODEL);
    cmd.add_option(
        "--input,-i",
        ValueType::Text,
        "Text every question is about",
    );
    cmd.add_option(
        "--input-file",
        ValueType::Text,
        "Read input from PATH (- for stdin)",
    );
    cmd.add_option("--ask", ValueType::Text, "Yes/no question (repeatable)")
        .multi();
    cmd.add_option(
        "--choice",
        ValueType::Text,
        "Choice question as 'QUESTION=a,b,c' (repeatable)",
    )
    .multi();
    cmd.add_option(
        "--score",
        ValueType::Text,
        "Score question as 'QUESTION=none,minor,major,outage' (repeatable)",
    )
    .multi();
    cmd.add_option(
        "--request",
        ValueType::Text,
        "Complete DecisionsRequest JSON file (- for stdin)",
    );
    cmd.add_option(
        "--temperature",
        ValueType::Float,
        "Probability temperature (>0, at most 100)",
    );
    cmd.add_flag("--json", "Print the raw response JSON");
    cmd.add_flag("--no-retry", "Do not retry HTTP 429 or 503");
    cmd.footer(&examples_footer(&[
        Example::new(
            "wally decisions --input 'Checkout is blank' --ask 'Is this a bug?'",
            "",
        ),
        Example::new(
            "wally decide --input-file ticket.txt --choice 'Owner=frontend,payments,account'",
            "",
        ),
    ]));
    cmd.callback(|p, global| run(p, p.flag("--json") || global.json));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_question_and_temperature_constraints() {
        let request = contract::DecisionsRequest {
            model: DEFAULT_MODEL.to_string(),
            input: "ticket".to_string(),
            questions: vec![contract::DecisionQuestion::YesNoQuestion(
                contract::YesNoQuestion {
                    id: "q1".to_string(),
                    question: "Is this broken?".to_string(),
                    r#type: "yes_no".to_string(),
                    ..Default::default()
                },
            )],
            temperature: Some(1.0),
            ..Default::default()
        };
        assert!(validate(&request).is_ok());
        let mut invalid = request;
        invalid.temperature = Some(0.0);
        assert!(validate(&invalid).is_err());
    }

    #[test]
    fn defaults_to_the_served_decision_model() {
        assert_eq!(DEFAULT_MODEL, "pplx-decider-v1");
        assert!(crate::harness::is_decisions_model(DEFAULT_MODEL));
    }

    #[test]
    fn request_file_with_an_unknown_field_is_named_at_every_depth() {
        let value = serde_json::json!({
            "model": "pplx-decider-v1",
            "input": "ticket",
            "temprature": 0.2,
            "questions": [
                {"id": "q1", "type": "yes_no", "question": "Bug?", "yse": "it is"},
                {"id": "q2", "type": "choice", "question": "Owner?",
                 "options": [{"name": "a", "descripton": "x"}, {"name": "b"}]},
                {"id": "q3", "type": "score", "question": "Severity?", "levels": ["low", "high"]}
            ]
        });
        assert_eq!(
            unknown_request_fields(&value),
            vec![
                "questions[0].yse".to_string(),
                "questions[1].options[0].descripton".to_string(),
                "temprature".to_string(),
            ]
        );
    }

    #[test]
    fn request_file_with_only_contract_fields_passes_including_nulls() {
        let value = serde_json::json!({
            "model": "pplx-decider-v1",
            "input": "ticket",
            "temperature": 1.0,
            "prompt_format_version": null,
            "questions": [
                {"id": "q1", "type": "yes_no", "question": "Bug?", "yes": "broken", "no": "fine"},
                {"id": "q2", "type": "choice", "question": "Owner?",
                 "options": [{"name": "a", "description": "x"}, {"name": "b"}]}
            ]
        });
        assert!(unknown_request_fields(&value).is_empty());
    }

    fn score_request() -> contract::DecisionsRequest {
        contract::DecisionsRequest {
            model: DEFAULT_MODEL.to_string(),
            input: "ticket".to_string(),
            questions: vec![contract::DecisionQuestion::ScoreQuestion(
                contract::ScoreQuestion {
                    id: "q1".to_string(),
                    question: "Severity?".to_string(),
                    levels: vec![
                        "none".into(),
                        "minor".into(),
                        "major".into(),
                        "outage".into(),
                    ],
                    r#type: "score".to_string(),
                },
            )],
            ..Default::default()
        }
    }

    #[test]
    fn score_answers_show_level_names_in_level_order() {
        let request = score_request();
        let mut answer = contract::DecisionAnswer {
            r#type: "score".to_string(),
            label_mass: 1.0,
            ..Default::default()
        };
        for (key, probability) in [("3", 0.1), ("0", 0.2), ("2", 0.6), ("1", 0.1)] {
            answer.probabilities.0.insert(key.to_string(), probability);
        }
        let views = question_views(&request);
        let rows = answer_rows(&views[0].1, &answer);
        let labels: Vec<&str> = rows.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["none", "minor", "major", "outage"]);
        assert_eq!(
            top_row(&rows).map(|(label, _)| label.as_str()),
            Some("major")
        );
    }

    #[test]
    fn a_tie_headlines_the_earlier_label() {
        let rows = vec![
            ("none".to_string(), 0.25),
            ("minor".to_string(), 0.25),
            ("major".to_string(), 0.25),
            ("outage".to_string(), 0.25),
        ];
        assert_eq!(
            top_row(&rows).map(|(label, _)| label.as_str()),
            Some("none")
        );
    }

    #[test]
    fn long_or_control_labels_are_cut_for_the_terminal() {
        assert_eq!(display_label("a\u{1b}[31mb"), "a[31mb");
        let long = "x".repeat(40);
        assert_eq!(display_label(&long).chars().count(), LABEL_MAX_CHARS);
        assert!(display_label(&long).ends_with('…'));
    }
}
