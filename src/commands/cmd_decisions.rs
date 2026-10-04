//! Decision scoring (`wally decisions`, alias `decide`): a decision model
//! answers typed questions with label probabilities instead of text.
//!
//! Two transports, one request/response contract. `--cloud` (or no local
//! artifact for `--model`) scores on the hosted console; `--local` (or a
//! `--model` that resolves to an on-disk artifact) scores in-process through
//! the kit's `rac_decision_*` component. The request shape, the rendering and
//! `--json` output are shared, so a script can point the same invocation at
//! either transport.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::io::Read;
use std::time::Instant;

use crate::account::decisions_contract as contract;
use crate::account::{self, ConsoleClient, ConsoleSession};
use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;
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

/// The hosted console accepts a model id, not a path; the local transport
/// accepts a catalog id, a registered id, or a path on this machine. So the
/// console's id-shape rule is applied by the cloud branch, not here — a
/// local path would otherwise be refused before the transport is chosen.
fn validate_model_shape(model: &str) -> Result<(), String> {
    nonblank(model, "model", 128)?;
    let mut chars = model.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
    {
        return Err("model must be a valid Wally model id".to_string());
    }
    Ok(())
}

fn validate(request: &contract::DecisionsRequest) -> Result<(), String> {
    nonblank(&request.input, "input", 1_000_000)?;
    nonblank(&request.model, "model", 128)?;
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

/// `label_mass_present` is the transport capability: the cloud server reports
/// the share of the whole next-token distribution that landed on the
/// question's labels, the local head does not. The low-fit note is only
/// printed when a mass actually exists to judge.
fn render_human(
    request: &contract::DecisionsRequest,
    response: &contract::DecisionsResponse,
    label_mass_present: bool,
) {
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
        if label_mass_present && answer.label_mass < 0.5 {
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

/// Which transport `wally decisions` should use. `Auto` is resolved by
/// whether `--model` names a local artifact: a decision model on this machine
/// is what the command runs; otherwise the hosted console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Auto,
    Local,
    Cloud,
}

fn transport_from_flags(p: &crate::cli::Parsed) -> Result<Transport, String> {
    let local = p.flag("--local");
    let cloud = p.flag("--cloud");
    match (local, cloud) {
        (true, true) => Err("--local and --cloud are mutually exclusive".to_string()),
        (true, false) => Ok(Transport::Local),
        (false, true) => Ok(Transport::Cloud),
        (false, false) => Ok(Transport::Auto),
    }
}

/// Whether `reference` names something the local decision component can load.
///
/// The catalog and the filesystem answer most references with no SDK
/// involved. A non-catalog, non-path id needs the registry, and a fresh
/// process has an empty in-memory registry until bootstrap restores the
/// manifest entries — so the probe bootstraps in offline mode (no
/// telemetry/auth, no network) before reading the stored category. A
/// registered id is local only when that category is Decision; an HF-pulled
/// LLM registered earlier must keep going to the hosted transport, and a
/// hosted decision id (`eve`) is never registered locally at all.
fn resolve_local_transport(reference: &str, options: &GlobalOptions) -> Result<bool, String> {
    if let Some(entry) = crate::catalog::find(reference) {
        return Ok(entry.category == v1::ModelCategory::Decision);
    }
    // A path on disk is a local artifact regardless of the registry.
    if crate::catalog::model_ref::is_local_path(reference) {
        return Ok(true);
    }
    // Known hosted decision ids are cloud by definition and are never
    // registered locally; skip the SDK init the registry read needs so the
    // default `eve` invocation keeps its fast path.
    if crate::harness::is_decisions_model(reference) {
        return Ok(false);
    }
    let offline = GlobalOptions {
        offline: true,
        ..options.clone()
    };
    bootstrap::bootstrap(&offline)
        .map_err(|rc| format!("cannot inspect local models: {}", out::describe_result(rc)))?;
    // Manifest-restored pulls come back here. A failed refresh leaves
    // whatever is already registered, so the category read is best-effort.
    let _ = crate::commands::model_setup::refresh_registry();
    Ok(registered_category(reference) == Some(v1::ModelCategory::Decision))
}

/// The stored category of a registered model id, if the registry knows it.
fn registered_category(reference: &str) -> Option<v1::ModelCategory> {
    let id = CString::new(reference).ok()?;
    let mut out = ProtoBuffer::new();
    // SAFETY: `id` is a valid NUL-terminated string for the call; `out` is a
    // freshly-initialised buffer.
    let rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            id.as_ptr(),
            out.as_mut_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        return None;
    }
    parse_proto_buffer::<v1::ModelInfo>(out)
        .ok()
        .and_then(|info| v1::ModelCategory::try_from(info.category).ok())
}

/// The response shape `create_decisions` returns, rebuilt locally so the
/// shared renderer/`--json` path is identical across transports. `label_mass`
/// is left at its default 0 and the renderer is told it is absent: the local
/// head has no whole-distribution mass to report, and faking one would be a
/// lie the `--json` output carried.
fn local_response(
    result: &v1::DecisionResult,
    request: &contract::DecisionsRequest,
) -> contract::DecisionsResponse {
    let mut answers = BTreeMap::new();
    for answer in &result.answers {
        let r#type = match v1::DecisionQuestionType::try_from(answer.r#type) {
            Ok(v1::DecisionQuestionType::Choice) => "choice",
            Ok(v1::DecisionQuestionType::Noul) => "yes_no",
            Ok(v1::DecisionQuestionType::Score) => "score",
            _ => "",
        }
        .to_string();
        // The local head keys probabilities by the option key it was given.
        // For a yes/no question the contract renders "yes"/"no", so the
        // canonical true/false keys are re-keyed the same way the SDK does.
        let mut probabilities = BTreeMap::new();
        for (key, probability) in &answer.probabilities {
            let key = match (r#type.as_str(), key.as_str()) {
                ("yes_no", "true") => "yes".to_string(),
                ("yes_no", "false") => "no".to_string(),
                (_, other) => other.to_string(),
            };
            probabilities.insert(key, f64::from(*probability));
        }
        let choice = match (&answer.answer, r#type.as_str()) {
            (Some(v1::decision_answer::Answer::Choice(key)), "choice") => Some(key.clone()),
            _ => None,
        };
        let score = match &answer.answer {
            Some(v1::decision_answer::Answer::Score(value)) => Some(f64::from(*value)),
            _ => None,
        };
        answers.insert(
            answer.id.clone(),
            contract::DecisionAnswer {
                choice,
                label_mass: 0.0,
                probabilities: contract::LabelProbabilities(probabilities),
                score,
                r#type,
            },
        );
    }
    let input_tokens = result
        .usage
        .as_ref()
        .map(|usage| i64::from(usage.input_tokens))
        .unwrap_or(0);
    contract::DecisionsResponse {
        answers: contract::DecisionAnswers(answers),
        model: if result.model_id.is_empty() {
            request.model.clone()
        } else {
            result.model_id.clone()
        },
        object: "decision".to_string(),
        prompt_format_version: i64::from(result.prompt_format_version),
        usage: contract::DecisionUsage {
            completion_tokens: 0,
            prompt_tokens: input_tokens,
            reasoning_tokens: None,
            total_tokens: input_tokens,
        },
    }
}

/// Build the proto request the local component takes from the contract-shaped
/// request the CLI built. The cloud `yes_no` question is the local NOUL; choice
/// options map name→key (the name IS the canonical key the renderer expects);
/// score levels map to ordered keys "0".."n-1" with the level text as the
/// description, so the answer's `legend` can label the expected level.
fn local_request(request: &contract::DecisionsRequest) -> v1::DecisionRequest {
    let questions = request
        .questions
        .iter()
        .map(|question| match question {
            contract::DecisionQuestion::YesNoQuestion(value) => v1::DecisionQuestion {
                id: value.id.clone(),
                r#type: v1::DecisionQuestionType::Noul as i32,
                instructions: value.question.clone(),
                options: vec![
                    v1::DecisionOption {
                        key: "true".to_string(),
                        description: value.yes.clone().unwrap_or_default(),
                    },
                    v1::DecisionOption {
                        key: "false".to_string(),
                        description: value.no.clone().unwrap_or_default(),
                    },
                ],
            },
            contract::DecisionQuestion::ChoiceQuestion(value) => v1::DecisionQuestion {
                id: value.id.clone(),
                r#type: v1::DecisionQuestionType::Choice as i32,
                instructions: value.question.clone(),
                options: value
                    .options
                    .iter()
                    .map(|option| v1::DecisionOption {
                        key: option.name.clone(),
                        description: option.description.clone().unwrap_or_default(),
                    })
                    .collect(),
            },
            contract::DecisionQuestion::ScoreQuestion(value) => v1::DecisionQuestion {
                id: value.id.clone(),
                r#type: v1::DecisionQuestionType::Score as i32,
                instructions: value.question.clone(),
                options: value
                    .levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| v1::DecisionOption {
                        key: index.to_string(),
                        description: level.clone(),
                    })
                    .collect(),
            },
        })
        .collect();

    let options =
        (request.temperature.is_some() || request.prompt_format_version.is_some()).then(|| {
            v1::DecisionOptions {
                temperature: request.temperature.unwrap_or(0.0) as f32,
                prompt_format_version: request.prompt_format_version.unwrap_or(0) as u32,
            }
        });

    v1::DecisionRequest {
        state: request.input.clone(),
        questions,
        options,
        model_id: (!request.model.is_empty()).then(|| request.model.clone()),
    }
}

/// Score through the local component: resolve the model, load it through the
/// lifecycle (auto-pull when missing), drive the decision component, and hand
/// the mapped response back to the shared renderer.
fn run_local(options: &GlobalOptions, request: &contract::DecisionsRequest, json: bool) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }
    if let Err(error) = crate::commands::model_setup::refresh_registry() {
        out::status_line(&format!("warning: registry refresh failed: {error}"));
    }

    let model = match ensure_model_ready(options, &request.model) {
        Ok(model) => model,
        Err(exit_code) => return exit_code,
    };

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: `handle` is a valid out-pointer for the duration of the call.
    let rc = unsafe { sys::rac_decision_component_create(&mut handle) };
    if rc != sys::SUCCESS || handle.is_null() {
        out::error_line("failed to create decision component");
        return 1;
    }

    let model_path = CString::new(model.primary_path.as_str());
    let model_id = CString::new(model.model_id.as_str());
    let model_name = CString::new(model.display_name.as_str());
    let (model_path, model_id, model_name) = match (model_path, model_id, model_name) {
        (Ok(path), Ok(id), Ok(name)) => (path, id, name),
        _ => {
            out::error_line("model path, id or name contains a NUL byte");
            // SAFETY: handle was just created above.
            unsafe { sys::rac_decision_component_destroy(handle) };
            return 1;
        }
    };
    // SAFETY: handle is the live component created above; the three C strings
    // outlive this call.
    let rc = unsafe {
        sys::rac_decision_component_load_model(
            handle,
            model_path.as_ptr(),
            model_id.as_ptr(),
            model_name.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "failed to load decision model: {}",
            out::describe_result(rc)
        ));
        // SAFETY: handle is the live component created above.
        unsafe { sys::rac_decision_component_destroy(handle) };
        return 1;
    }

    let proto_request = local_request(request);
    let bytes = serialize(&proto_request);
    let mut out_buffer = ProtoBuffer::new();
    let started = Instant::now();
    // SAFETY: handle is the live, loaded component; bytes/out_buffer are valid
    // for the duration of the call.
    let proto_rc = unsafe {
        sys::rac_decision_component_decide_proto(
            handle,
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::DecisionResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(result) if !result.answers.is_empty() => {
            // A non-success rc with answers in hand is a partial result: some
            // question was refused (an over-window prompt, a bad span). The
            // remaining answers are real but the request as a whole is not,
            // and reporting exit 0 would tell a script the missing ids are
            // authoritative "no answer". Fail instead, naming the rc.
            out::error_line(&format!(
                "decision failed: {} ({} of {} questions answered)",
                out::describe_result(proto_rc),
                result.answers.len(),
                request.questions.len()
            ));
            // SAFETY: handle is the live component created above.
            unsafe { sys::rac_decision_component_destroy(handle) };
            return 1;
        }
        Ok(_) => {
            out::error_line("decision failed: ");
            // SAFETY: handle is the live component created above.
            unsafe { sys::rac_decision_component_destroy(handle) };
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("decision failed: {error}"));
            // SAFETY: handle is the live component created above.
            unsafe { sys::rac_decision_component_destroy(handle) };
            return 1;
        }
    };
    // SAFETY: handle is the live component created above; not used again.
    unsafe { sys::rac_decision_component_destroy(handle) };

    // Every requested question must carry an answer, whatever the rc said;
    // a missing id is the same partial failure the arm above rejects.
    if let Some((missing, _)) = question_views(request)
        .into_iter()
        .find(|(id, _)| !result.answers.iter().any(|answer| answer.id == *id))
    {
        out::error_line(&format!(
            "decision failed: no answer for question '{missing}'"
        ));
        return 1;
    }

    let latency_ms = started.elapsed().as_millis();
    let response = local_response(&result, request);
    if json {
        out::result_line(&crate::io::json::dump(&response.to_json()));
    } else {
        render_human(request, &response, false);
    }
    out::status_line(&format!(
        "{} · {} tokens · local · {}ms",
        response.model, response.usage.total_tokens, latency_ms
    ));
    0
}

fn run(p: &crate::cli::Parsed, options: &GlobalOptions, json: bool) -> i32 {
    let transport = match transport_from_flags(p) {
        Ok(transport) => transport,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };
    let request = match build_request(p) {
        Ok(request) => request,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };
    let local = match transport {
        Transport::Local => match resolve_local_transport(&request.model, options) {
            Ok(true) => true,
            Ok(false) => {
                out::error_line(&format!(
                    "--local was given but '{}' does not resolve to a model on this machine",
                    request.model
                ));
                return 2;
            }
            Err(error) => {
                out::error_line(&error);
                return 1;
            }
        },
        Transport::Cloud => false,
        Transport::Auto => match resolve_local_transport(&request.model, options) {
            Ok(local) => local,
            Err(error) => {
                out::error_line(&error);
                return 1;
            }
        },
    };
    if local {
        if p.is_set("--no-retry") {
            out::error_line("--no-retry is a cloud-only flag");
            return 2;
        }
        // The local path promises no network; bootstrap must not run the
        // SDK's telemetry/auth phase, which posts anonymously to the staging
        // backend in development.
        let offline = GlobalOptions {
            offline: true,
            ..options.clone()
        };
        return run_local(&offline, &request, json);
    }
    // The hosted console takes a model id, not a path; only the local
    // transport accepts a path, so the id-shape rule gates cloud here.
    if let Err(error) = validate_model_shape(&request.model) {
        out::error_line(&error);
        return 2;
    }
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
        render_human(&request, &result.response, true);
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
    let cmd = app.add_subcommand("decisions", "Score questions with Eve, the decision model");
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
    cmd.add_flag(
        "--local",
        "Score on this machine with a local decision model (never the network)",
    );
    cmd.add_flag(
        "--cloud",
        "Score on your account's hosted endpoint (never a local model)",
    );
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
        Example::new(
            "wally decisions --local -m clef-flash-9b --input 'Checkout is blank' --ask 'Is this a bug?'",
            "",
        ),
    ]));
    cmd.callback(|p, global| run(p, global, p.flag("--json") || global.json));
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
        assert_eq!(DEFAULT_MODEL, "eve");
        assert!(crate::harness::is_decisions_model(DEFAULT_MODEL));
    }

    #[test]
    fn request_file_with_an_unknown_field_is_named_at_every_depth() {
        let value = serde_json::json!({
            "model": "eve",
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
            "model": "eve",
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

    fn ticket_request() -> contract::DecisionsRequest {
        contract::DecisionsRequest {
            model: "clef-flash-9b".to_string(),
            input: "ticket".to_string(),
            questions: vec![
                contract::DecisionQuestion::YesNoQuestion(contract::YesNoQuestion {
                    id: "refund".to_string(),
                    question: "Refund?".to_string(),
                    r#type: "yes_no".to_string(),
                    yes: Some("asks for a refund".to_string()),
                    no: Some("no refund".to_string()),
                }),
                contract::DecisionQuestion::ScoreQuestion(contract::ScoreQuestion {
                    id: "urgency".to_string(),
                    question: "Urgency?".to_string(),
                    r#type: "score".to_string(),
                    levels: vec!["low".into(), "medium".into(), "high".into()],
                }),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn local_request_maps_cloud_shapes_onto_the_proto_contract() {
        let proto = local_request(&ticket_request());
        assert_eq!(proto.state, "ticket");
        assert_eq!(proto.questions.len(), 2);
        let refund = &proto.questions[0];
        assert_eq!(refund.r#type, v1::DecisionQuestionType::Noul as i32);
        // The engine finds the affirmative option by the canonical key.
        assert_eq!(refund.options[0].key, "true");
        assert_eq!(refund.options[1].key, "false");
        let urgency = &proto.questions[1];
        assert_eq!(urgency.r#type, v1::DecisionQuestionType::Score as i32);
        // Score levels become ordered keys with the level text as legend.
        let keys: Vec<&str> = urgency.options.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["0", "1", "2"]);
        assert_eq!(urgency.options[2].description, "high");
    }

    #[test]
    fn local_response_maps_the_proto_result_back_to_the_cloud_shape() {
        let request = ticket_request();
        let mut result = v1::DecisionResult {
            model_id: "clef-flash-9b".to_string(),
            prompt_format_version: 3,
            ..Default::default()
        };
        let mut refund = v1::DecisionAnswer {
            id: "refund".to_string(),
            r#type: v1::DecisionQuestionType::Noul as i32,
            confidence: 0.93,
            ..Default::default()
        };
        refund.probabilities.insert("true".to_string(), 0.93_f32);
        refund.probabilities.insert("false".to_string(), 0.07_f32);
        refund.answer = Some(v1::decision_answer::Answer::Noul(0.93));
        let mut urgency = v1::DecisionAnswer {
            id: "urgency".to_string(),
            r#type: v1::DecisionQuestionType::Score as i32,
            ..Default::default()
        };
        urgency.probabilities.insert("2".to_string(), 0.7_f32);
        urgency.answer = Some(v1::decision_answer::Answer::Score(1.7));
        result.answers = vec![refund, urgency];
        result.usage = Some(v1::TokenUsage {
            input_tokens: 97,
            ..Default::default()
        });

        let response = local_response(&result, &request);
        assert_eq!(response.model, "clef-flash-9b");
        assert_eq!(response.prompt_format_version, 3);
        assert_eq!(response.usage.total_tokens, 97);
        let refund_answer = &response.answers.0["refund"];
        // yes_no keys are re-keyed to the contract's yes/no spelling.
        assert_eq!(refund_answer.r#type, "yes_no");
        assert!((refund_answer.probabilities.0["yes"] - 0.93).abs() < 1e-6);
        assert!((refund_answer.probabilities.0["no"] - 0.07).abs() < 1e-6);
        let urgency_answer = &response.answers.0["urgency"];
        assert_eq!(urgency_answer.r#type, "score");
        assert!((urgency_answer.score.unwrap() - 1.7).abs() < 1e-6);
        // The local head reports no whole-distribution mass; the renderer is
        // told it is absent rather than printing a fake low-fit note.
        assert_eq!(refund_answer.label_mass, 0.0);
    }

    #[test]
    fn local_response_choice_keeps_the_option_key() {
        let request = contract::DecisionsRequest {
            model: "clef-flash-9b".to_string(),
            input: "ticket".to_string(),
            questions: vec![contract::DecisionQuestion::ChoiceQuestion(
                contract::ChoiceQuestion {
                    id: "team".to_string(),
                    question: "Owner?".to_string(),
                    r#type: "choice".to_string(),
                    options: vec![
                        contract::DecisionOption {
                            name: "billing".into(),
                            description: None,
                        },
                        contract::DecisionOption {
                            name: "technical".into(),
                            description: None,
                        },
                    ],
                },
            )],
            ..Default::default()
        };
        let mut result = v1::DecisionResult::default();
        let mut answer = v1::DecisionAnswer {
            id: "team".to_string(),
            r#type: v1::DecisionQuestionType::Choice as i32,
            ..Default::default()
        };
        answer.probabilities.insert("billing".to_string(), 0.97_f32);
        answer
            .probabilities
            .insert("technical".to_string(), 0.03_f32);
        answer.answer = Some(v1::decision_answer::Answer::Choice("billing".to_string()));
        result.answers = vec![answer];

        let response = local_response(&result, &request);
        let mapped = &response.answers.0["team"];
        assert_eq!(mapped.choice.as_deref(), Some("billing"));
        assert!((mapped.probabilities.0["billing"] - 0.97).abs() < 1e-6);
        assert!(mapped.score.is_none());
    }

    #[test]
    fn catalog_decisions_are_local_and_hosted_ids_are_cloud() {
        // The registry probe (the unknown-word case) bootstraps the SDK, so
        // point it at a throwaway home rather than the real one. Every other
        // case below answers before the registry is touched.
        let dir = std::env::temp_dir().join(format!(
            "wally-decisions-transport-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let options = GlobalOptions {
            home_override: dir.to_string_lossy().into_owned(),
            ..GlobalOptions::default()
        };

        // A catalog decision row is local; an LLM row in the same catalog is
        // not (it would fail at the decision component).
        assert_eq!(resolve_local_transport("clef-flash-9b", &options), Ok(true));
        assert_eq!(
            resolve_local_transport("clef-flash-mlx", &options),
            Ok(true)
        );
        assert_eq!(resolve_local_transport("qwen3-0.6b", &options), Ok(false));
        // A hosted decision id is cloud by definition and must not be treated
        // as a local artifact (nor pay for an SDK init to find that out).
        assert_eq!(resolve_local_transport("eve", &options), Ok(false));
        // An unknown bare word is not local; the registry probe runs, finds
        // nothing, and says so.
        assert_eq!(
            resolve_local_transport("definitely-not-a-model", &options),
            Ok(false)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transport_flags_are_mutually_exclusive() {
        use crate::cli::Parsed;
        let flag = |name: &str| {
            let mut parsed = Parsed::default();
            parsed
                .name_to_spec
                .insert(name.to_string(), name.to_string());
            parsed
                .flag_values
                .insert(name.to_string(), vec!["true".to_string()]);
            parsed
        };

        let mut both = flag("--local");
        both.name_to_spec
            .insert("--cloud".to_string(), "--cloud".to_string());
        both.flag_values
            .insert("--cloud".to_string(), vec!["true".to_string()]);
        assert!(transport_from_flags(&both).is_err());

        assert_eq!(
            transport_from_flags(&flag("--local")).unwrap(),
            Transport::Local
        );
        assert_eq!(
            transport_from_flags(&flag("--cloud")).unwrap(),
            Transport::Cloud
        );
        assert_eq!(
            transport_from_flags(&Parsed::default()).unwrap(),
            Transport::Auto
        );
    }
}
