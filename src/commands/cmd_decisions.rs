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
use std::io::Write as _;
use std::time::Instant;

use crate::account::decisions_contract as contract;
use crate::account::{self, ConsoleClient, ConsoleSession};
use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::model_setup::ensure_model_ready;
use crate::config::preferences::{self, DecisionsRoute};
use crate::io::heic;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;
use crate::util::term;

const DEFAULT_MODEL: &str = crate::harness::DEFAULT_DECISIONS_MODEL;
/// The gateway's body caps (InferenceInfra constants.toml [gateway]
/// decisions_max_request_bytes, decisions_max_image_request_bytes): 1 MiB
/// outside the images, 24 MiB in all with them. Images go at full size: the
/// gateway resizes each to what the model sees, and refuses one it will not
/// decode (its pixel caps) with a 400 that names the image.
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_IMAGE_BODY_BYTES: usize = 24 * 1024 * 1024;
/// The pinned contract, read for its closed objects when a `--request` file is
/// checked for fields the server would refuse (or this CLI would drop).
const CONTRACT: &str = include_str!("../../contracts/wally-decisions-public-v1.openapi.json");
/// Widest label column in the human rendering; longer labels are cut.
const LABEL_MAX_CHARS: usize = 32;
/// The contract's `images`: at most 8, each a base64 data URL, PNG, JPEG or
/// WebP only.
const MAX_IMAGES: usize = 8;
const IMAGE_URL_PREFIXES: [&str; 3] = [
    "data:image/png;base64,",
    "data:image/jpeg;base64,",
    "data:image/webp;base64,",
];

fn image_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> shift) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// An image file as the data URL the contract takes, at its full size, its
/// type read from its bytes rather than its name. A HEIC photo is sent as a
/// JPEG made from it. The gateway judges every image's size, except one whose
/// base64 alone is past the 24 MiB a request with images may be, which is
/// refused here before it is read.
fn image_data_url(path: &str) -> Result<String, String> {
    let refuse_large = |size: u64| {
        let chars = size.div_ceil(3) * 4;
        if chars > MAX_IMAGE_BODY_BYTES as u64 {
            return Err(format!(
                "{path} is too large to send: as base64 it is {:.1} MiB, over the 24 MiB a request with images may be",
                chars as f64 / 1_048_576.0
            ));
        }
        Ok(())
    };
    let size = std::fs::metadata(path)
        .map_err(|error| format!("could not read {path}: {error}"))?
        .len();
    refuse_large(size)?;
    let mut bytes =
        std::fs::read(path).map_err(|error| format!("could not read {path}: {error}"))?;
    if heic::is_heif(&bytes) {
        bytes = heic::to_jpeg(&bytes).map_err(|reason| {
            format!("{path} is a HEIC photo that could not be converted to JPEG: {reason}")
        })?;
        refuse_large(bytes.len() as u64)?;
    }
    let kind = image_kind(&bytes)
        .ok_or_else(|| format!("{path} is not a PNG, JPEG, WebP or HEIC image"))?;
    Ok(format!("data:image/{kind};base64,{}", base64(&bytes)))
}

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

pub(crate) fn validate(request: &contract::DecisionsRequest) -> Result<(), String> {
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
    if let Some(images) = &request.images {
        if !(1..=MAX_IMAGES).contains(&images.len()) {
            return Err(format!("provide 1 to {MAX_IMAGES} images"));
        }
        for (i, image) in images.iter().enumerate() {
            if !IMAGE_URL_PREFIXES
                .iter()
                .any(|prefix| image.starts_with(prefix))
            {
                return Err(format!(
                    "images[{i}] must be a base64 PNG, JPEG or WebP data URL"
                ));
            }
        }
    }
    // The gateway's own test: past 1 MiB, the body less its image strings
    // must still be within 1 MiB.
    let bytes = crate::io::json::dump(&request.to_json()).len();
    let images = request.images.as_deref().unwrap_or_default();
    let image_chars: usize = images.iter().map(String::len).sum();
    if bytes > MAX_BODY_BYTES && bytes - image_chars > MAX_BODY_BYTES {
        return Err(if images.is_empty() {
            "request body exceeds 1 MiB".to_string()
        } else {
            "request body exceeds 1 MiB outside its images".to_string()
        });
    }
    if bytes > MAX_IMAGE_BODY_BYTES {
        return Err("request body with its images exceeds 24 MiB".to_string());
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
            "--route",
            "--image",
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
    let paths = p.get_strs("--image");
    if paths.len() > MAX_IMAGES {
        return Err(format!("provide at most {MAX_IMAGES} --image files"));
    }
    let images = paths
        .iter()
        .map(|path| image_data_url(path))
        .collect::<Result<Vec<_>, _>>()?;
    let request = contract::DecisionsRequest {
        model: p
            .get_str("--model")
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
        input,
        questions,
        temperature: p.get_f64("--temperature"),
        prompt_format_version: None,
        images: (!images.is_empty()).then_some(images),
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
pub(crate) fn unknown_request_fields(value: &serde_json::Value) -> Vec<String> {
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

/// The local decision model a bare `wally decisions` runs: the GGUF row where
/// the linked kit has llama.cpp, the MLX row on Apple builds without it.
/// `None` where this build cannot score anything on-device.
fn default_local_model() -> Option<&'static str> {
    if cfg!(wally_has_llamacpp) {
        Some("clef-flash-9b")
    } else if cfg!(target_os = "macos") {
        Some("clef-flash-mlx")
    } else {
        None
    }
}

/// Parse a route-menu answer: `1` is cloud, `2` is local, anything else
/// (including an empty line) takes the suggestion.
fn parse_route_choice(line: &str, suggested: DecisionsRoute) -> DecisionsRoute {
    match line.trim_end_matches(['\n', '\r']).trim() {
        "1" => DecisionsRoute::Cloud,
        "2" => DecisionsRoute::Local,
        _ => suggested,
    }
}

/// Ask once where bare invocations score. Numbered with local suggested
/// (the prompt only runs where a local model exists), Enter takes it.
/// Everything on stderr so `--json` stays clean.
fn prompt_decisions_route(local_id: &str) -> DecisionsRoute {
    let signed_in = account::load().map(|c| c.signed_in()).unwrap_or(false);
    let login_hint = if signed_in {
        String::new()
    } else {
        " (needs `wally account login` first)".to_string()
    };
    eprint!(
        "How should decisions be scored? One-time question, your answer is saved as the default.\n  1) cloud: hosted {} model{}\n  2) local: on-device {}, data stays on this machine\nChoice [1/2, Enter=2]: ",
        DEFAULT_MODEL, login_hint, local_id
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0 {
        return DecisionsRoute::Local;
    }
    parse_route_choice(&answer, DecisionsRoute::Local)
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
pub(crate) fn resolve_local_transport(
    reference: &str,
    options: &GlobalOptions,
) -> Result<bool, String> {
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
/// A created, model-loaded decision component. Dropping unloads nothing
/// extra (unload is per-model on reload); destroy always runs exactly once.
pub(crate) struct LoadedDecisions {
    handle: sys::rac_handle_t,
}

impl Drop for LoadedDecisions {
    fn drop(&mut self) {
        // SAFETY: handle came from rac_decision_component_create and is
        // destroyed exactly once here; nothing else touches it afterwards.
        unsafe { sys::rac_decision_component_destroy(self.handle) };
    }
}

/// Create the component and load `model` into it. Error strings match what
/// run_local printed inline, so both callers report identically.
pub(crate) fn load_decisions_component(
    model: &crate::commands::model_setup::ResolvedModelPaths,
) -> Result<LoadedDecisions, String> {
    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: `handle` is a valid out-pointer for the duration of the call.
    let rc = unsafe { sys::rac_decision_component_create(&mut handle) };
    if rc != sys::SUCCESS || handle.is_null() {
        return Err("failed to create decision component".to_string());
    }
    let loaded = LoadedDecisions { handle };

    let model_path = CString::new(model.primary_path.as_str());
    let model_id = CString::new(model.model_id.as_str());
    let model_name = CString::new(model.display_name.as_str());
    let (model_path, model_id, model_name) = match (model_path, model_id, model_name) {
        (Ok(path), Ok(id), Ok(name)) => (path, id, name),
        _ => return Err("model path, id or name contains a NUL byte".to_string()),
    };
    // SAFETY: handle is live; the three C strings outlive this call.
    let rc = unsafe {
        sys::rac_decision_component_load_model(
            loaded.handle,
            model_path.as_ptr(),
            model_id.as_ptr(),
            model_name.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        return Err(format!(
            "failed to load decision model: {}",
            out::describe_result(rc)
        ));
    }
    Ok(loaded)
}

/// Score one request on an already-loaded component. Error strings match
/// what run_local printed inline (partial results fail naming the rc, a
/// missing answer id fails naming the id).
pub(crate) fn score_loaded(
    loaded: &LoadedDecisions,
    request: &contract::DecisionsRequest,
) -> Result<contract::DecisionsResponse, String> {
    let proto_request = local_request(request);
    let bytes = serialize(&proto_request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: handle is live and loaded; bytes/out_buffer are valid for the
    // duration of the call.
    let proto_rc = unsafe {
        sys::rac_decision_component_decide_proto(
            loaded.handle,
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::DecisionResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(result) if !result.answers.is_empty() => {
            return Err(format!(
                "decision failed: {} ({} of {} questions answered)",
                out::describe_result(proto_rc),
                result.answers.len(),
                request.questions.len()
            ));
        }
        Ok(_) => return Err("decision failed: ".to_string()),
        Err(error) => return Err(format!("decision failed: {error}")),
    };

    // Every requested question must carry an answer, whatever the rc said;
    // a missing id is the same partial failure the arm above rejects.
    if let Some((missing, _)) = question_views(request)
        .into_iter()
        .find(|(id, _)| !result.answers.iter().any(|answer| answer.id == *id))
    {
        return Err(format!(
            "decision failed: no answer for question '{missing}'"
        ));
    }

    Ok(local_response(&result, request))
}

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

    // Third-party code inside load/score prints to stdout (the tokenizer
    // fallback); that corrupts --json, so stdout stays muted across both.
    // Stderr stays live for SDK diagnostics.
    let hush = crate::util::hush::HushedStdout::mute();

    let loaded = match load_decisions_component(&model) {
        Ok(loaded) => loaded,
        Err(message) => {
            drop(hush);
            out::error_line(&message);
            return 1;
        }
    };

    let started = Instant::now();
    let response = match score_loaded(&loaded, request) {
        Ok(response) => response,
        Err(message) => {
            // Teardown prints too: destroy the component while muted.
            drop(loaded);
            drop(hush);
            out::error_line(&message);
            return 1;
        }
    };
    // Unload inside the muted window too: session teardown re-enters
    // third-party code that prints, after the last line is already out.
    drop(loaded);
    drop(hush);
    let latency_ms = started.elapsed().as_millis();
    if json {
        out::result_line(&crate::io::json::dump(&response.to_json()));
    } else {
        render_human(request, &response, false);
    }
    // The fallback print also fires during runtime teardown, after the last
    // line is out. Mute the tail: nothing else prints to stdout past here.
    crate::util::hush::mute_stdout_process();
    out::status_line(&format!(
        "{} · {} tokens · local · {}ms",
        response.model, response.usage.total_tokens, latency_ms
    ));
    0
}

fn run(p: &crate::cli::Parsed, options: &GlobalOptions, json: bool) -> i32 {
    let mut transport = match transport_from_flags(p) {
        Ok(transport) => transport,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };
    // `--route` saves the default; validated here, persisted after the
    // request checks out so a failing invocation never saves a preference.
    // It already selects the route, so the per-run flags and a request file
    // (which carries its own model) have nothing left to say.
    let mut forced_route = None;
    if let Some(route) = p.get_str("--route") {
        if p.flag("--local") || p.flag("--cloud") || p.is_set("--request") {
            out::error_line("--route cannot be combined with --local, --cloud, or --request");
            return 2;
        }
        match preferences::parse_decisions_route(Some(&route)) {
            Some(parsed) => forced_route = Some(parsed),
            None => {
                out::error_line(&format!(
                    "decisions route must be 'local' or 'cloud', not '{route}'"
                ));
                return 2;
            }
        }
    }
    let mut request = match build_request(p) {
        Ok(request) => request,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };
    if transport == Transport::Local && request.images.is_some() {
        out::error_line("images are scored on the cloud only: drop --local");
        return 2;
    }
    if let Some(route) = forced_route {
        if let Err(error) = preferences::set_decisions_route(route.name()) {
            out::error_line(&error);
            return 2;
        }
        out::status_line(&format!("saved decisions route: {}", route.name()));
    }
    // No flags and no model is the first-run shape: follow the saved route,
    // ask once when there is none, and stay on today's cloud default where
    // no answer can come (a pipe) or nothing local can run. Images are
    // cloud-only, so they never ask.
    if transport == Transport::Auto
        && !p.is_set("--model")
        && !p.is_set("--request")
        && request.images.is_none()
    {
        let route = match forced_route.or_else(preferences::saved_decisions_route) {
            Some(saved) => {
                out::status_line(&format!("using saved decisions route: {}", saved.name()));
                saved
            }
            None if !term::stdin_is_tty() => DecisionsRoute::Cloud,
            None => match default_local_model() {
                None => DecisionsRoute::Cloud,
                Some(local_id) => {
                    let choice = prompt_decisions_route(local_id);
                    match preferences::set_decisions_route(match choice {
                        DecisionsRoute::Local => "local",
                        DecisionsRoute::Cloud => "cloud",
                    }) {
                        Ok(_) => out::status_line("saved as your decisions default"),
                        Err(error) => out::status_line(&format!(
                            "warning: choice not saved ({error}); continuing anyway"
                        )),
                    }
                    choice
                }
            },
        };
        if route == DecisionsRoute::Local {
            match default_local_model() {
                Some(local_id) => {
                    request.model = local_id.to_string();
                    transport = Transport::Local;
                }
                None => {
                    out::error_line(
                        "saved decisions route is 'local' but this build cannot score on-device",
                    );
                    return 2;
                }
            }
        }
    }
    let local = match transport {
        Transport::Local => match resolve_local_transport(&request.model, options) {
            Ok(true) => true,
            Ok(false) => {
                out::error_line(&format!(
                    "local scoring was selected but '{}' does not resolve to a model on this machine",
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
        if request.images.is_some() {
            out::error_line("images are scored on the cloud only: drop --local, or add --cloud");
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
        "--image",
        ValueType::Text,
        "PNG, JPEG, WebP or HEIC file every question is about, sent at full size (repeatable, up to 8; cloud only; HEIC needs macOS)",
    )
    .multi();
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
    cmd.add_option(
        "--route",
        ValueType::Text,
        "Save 'local' or 'cloud' as the default route (used when no flags are given)",
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
        Example::new(
            "wally decisions --image receipt.png --input 'Expense claim' --ask 'Is the total over $100?'",
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
    fn base64_matches_the_standard_alphabet_and_padding() {
        for (raw, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foobar", "Zm9vYmFy"),
            (&[0xFB, 0xFF, 0xBF][..], "+/+/"),
        ] {
            assert_eq!(base64(raw), encoded);
        }
    }

    #[test]
    fn an_image_type_is_read_from_its_bytes() {
        assert_eq!(image_kind(b"\x89PNG\r\n\x1a\nrest"), Some("png"));
        assert_eq!(image_kind(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpeg"));
        assert_eq!(image_kind(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(image_kind(b"GIF89a"), None);
        assert_eq!(image_kind(b"RIFF\0\0\0\0WAVE"), None);
    }

    #[test]
    fn an_image_file_becomes_a_data_url_and_anything_else_is_refused() {
        let dir = std::env::temp_dir().join(format!("wally-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n").unwrap();
        assert_eq!(
            image_data_url(png.to_str().unwrap()).unwrap(),
            "data:image/png;base64,iVBORw0KGgo="
        );
        let gif = dir.join("a.gif");
        std::fs::write(&gif, b"GIF89a").unwrap();
        assert!(image_data_url(gif.to_str().unwrap())
            .unwrap_err()
            .contains("not a PNG, JPEG, WebP or HEIC"));
        // Full size: the gateway, not this CLI, judges an image's size.
        let big = dir.join("big.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(5_000_000, 7);
        std::fs::write(&big, &bytes).unwrap();
        assert_eq!(
            image_data_url(big.to_str().unwrap()).unwrap(),
            format!("data:image/png;base64,{}", base64(&bytes))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_image_whose_base64_alone_is_past_24_mib_is_refused_unread() {
        let dir = std::env::temp_dir().join(format!("wally-image-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 18 MiB is the most whose base64 (four characters for three bytes)
        // fits; past it the file is refused on its length, without reading
        // it (this one is sparse and holds no image at all).
        let limit = (MAX_IMAGE_BODY_BYTES / 4 * 3) as u64;
        let pano = dir.join("pano.jpg");
        std::fs::File::create(&pano)
            .unwrap()
            .set_len(limit + 1)
            .unwrap();
        assert_eq!(
            image_data_url(pano.to_str().unwrap()).unwrap_err(),
            format!(
                "{} is too large to send: as base64 it is 24.0 MiB, over the 24 MiB a request with images may be",
                pano.display()
            )
        );
        // At the limit it is read, and typed from its bytes.
        let mut at = vec![0xFF, 0xD8, 0xFF, 0xE0];
        at.resize(limit as usize, 0);
        std::fs::write(&pano, &at).unwrap();
        assert!(image_data_url(pano.to_str().unwrap())
            .unwrap()
            .starts_with("data:image/jpeg;base64,/9j/4A"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_heic_photo_is_sent_as_jpeg_whatever_its_name() {
        let dir = std::env::temp_dir().join(format!("wally-heic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let photo = dir.join("IMG_0001.jpg");
        std::fs::write(
            &photo,
            include_bytes!("../../tests/fixtures/quadrants-orientation-6-gps.heic"),
        )
        .unwrap();
        let sent = image_data_url(photo.to_str().unwrap());
        if cfg!(target_os = "macos") {
            assert!(sent.unwrap().starts_with("data:image/jpeg;base64,/9j/"));
        } else {
            let error = sent.unwrap_err();
            assert!(error.contains("IMG_0001.jpg is a HEIC photo"), "{error}");
            assert!(error.contains("macOS only"), "{error}");
        }

        let broken = dir.join("broken.heic");
        let mut bytes = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic".to_vec();
        bytes.resize(256, 0);
        std::fs::write(&broken, bytes).unwrap();
        let error = image_data_url(broken.to_str().unwrap()).unwrap_err();
        assert!(
            error.contains("broken.heic is a HEIC photo that could not be converted to JPEG"),
            "{error}"
        );
        // Cut short: refused before decoding, which would make it black.
        let cut = dir.join("cut.heic");
        let whole = include_bytes!("../../tests/fixtures/quadrants-grid.heic");
        std::fs::write(&cut, &whole[..whole.len() - 1]).unwrap();
        assert!(image_data_url(cut.to_str().unwrap())
            .unwrap_err()
            .ends_with("could not be converted to JPEG: the file is incomplete or damaged"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn images_are_held_to_the_contract() {
        let request = |images: Option<Vec<String>>| contract::DecisionsRequest {
            model: DEFAULT_MODEL.to_string(),
            input: "receipt".to_string(),
            questions: vec![contract::DecisionQuestion::YesNoQuestion(
                contract::YesNoQuestion {
                    id: "q1".to_string(),
                    question: "Over $100?".to_string(),
                    r#type: "yes_no".to_string(),
                    ..Default::default()
                },
            )],
            images,
            ..Default::default()
        };
        let url = "data:image/png;base64,iVBORw0KGgo=".to_string();
        assert!(validate(&request(None)).is_ok());
        assert!(validate(&request(Some(vec![url.clone(); 8]))).is_ok());
        assert!(validate(&request(Some(vec![]))).is_err());
        assert!(validate(&request(Some(vec![url.clone(); 9]))).is_err());
        assert!(validate(&request(Some(
            vec!["https://example.com/a.png".to_string()]
        )))
        .is_err());
        assert!(validate(&request(Some(vec![
            "data:image/gif;base64,R0lG".to_string()
        ])))
        .is_err());
    }

    #[test]
    fn the_body_is_held_to_the_gateways_caps_and_nothing_else() {
        // `pad` bytes more text: the input takes up to its own 1,000,000-char
        // limit, the question the rest.
        let request = |pad: usize, images: Vec<usize>| contract::DecisionsRequest {
            model: DEFAULT_MODEL.to_string(),
            input: format!("x{}", "a".repeat(pad.min(990_000))),
            questions: vec![contract::DecisionQuestion::YesNoQuestion(
                contract::YesNoQuestion {
                    id: "q1".to_string(),
                    question: format!("Over $100?{}", "a".repeat(pad.saturating_sub(990_000))),
                    r#type: "yes_no".to_string(),
                    ..Default::default()
                },
            )],
            images: (!images.is_empty()).then(|| {
                images
                    .into_iter()
                    .map(|chars| format!("data:image/png;base64,{}", "A".repeat(chars)))
                    .collect()
            }),
            ..Default::default()
        };
        let bytes =
            |request: &contract::DecisionsRequest| crate::io::json::dump(&request.to_json()).len();
        let outside = |request: &contract::DecisionsRequest| {
            bytes(request)
                - request
                    .images
                    .iter()
                    .flatten()
                    .map(String::len)
                    .sum::<usize>()
        };

        // No images: 1 MiB.
        let room = MAX_BODY_BYTES - bytes(&request(0, vec![]));
        assert!(validate(&request(room, vec![])).is_ok());
        assert_eq!(
            validate(&request(room + 1, vec![])).unwrap_err(),
            "request body exceeds 1 MiB"
        );
        // With images: 1 MiB outside them, however large they are.
        let room = MAX_BODY_BYTES - outside(&request(0, vec![2_000_000]));
        assert!(validate(&request(room, vec![2_000_000])).is_ok());
        assert_eq!(
            validate(&request(room + 1, vec![2_000_000])).unwrap_err(),
            "request body exceeds 1 MiB outside its images"
        );
        // And 24 MiB in all, but no cap on any one image.
        assert!(validate(&request(0, vec![12_000_000, 12_000_000])).is_ok());
        let fill = MAX_IMAGE_BODY_BYTES - bytes(&request(0, vec![0]));
        assert_eq!(bytes(&request(0, vec![fill])), MAX_IMAGE_BODY_BYTES);
        assert!(validate(&request(0, vec![fill])).is_ok());
        assert_eq!(
            validate(&request(0, vec![fill + 1])).unwrap_err(),
            "request body with its images exceeds 24 MiB"
        );
    }

    #[test]
    fn defaults_to_the_served_decision_model() {
        assert_eq!(DEFAULT_MODEL, "eve");
        assert!(crate::harness::is_decisions_model(DEFAULT_MODEL));
    }

    #[test]
    fn default_local_model_matches_what_this_build_can_run() {
        // Mirrors the catalog gating the transport test asserts on: GGUF
        // where llama.cpp is linked, MLX on Apple builds without it, and no
        // local model where neither exists.
        if cfg!(wally_has_llamacpp) {
            assert_eq!(default_local_model(), Some("clef-flash-9b"));
        } else if cfg!(target_os = "macos") {
            assert_eq!(default_local_model(), Some("clef-flash-mlx"));
        } else {
            assert_eq!(default_local_model(), None);
        }
    }

    #[test]
    fn route_menu_numbers_are_stable_and_blank_takes_the_suggestion() {
        assert_eq!(
            parse_route_choice("1", DecisionsRoute::Local),
            DecisionsRoute::Cloud
        );
        assert_eq!(
            parse_route_choice("2", DecisionsRoute::Cloud),
            DecisionsRoute::Local
        );
        assert_eq!(
            parse_route_choice("", DecisionsRoute::Local),
            DecisionsRoute::Local
        );
        assert_eq!(
            parse_route_choice("  \n", DecisionsRoute::Cloud),
            DecisionsRoute::Cloud
        );
        assert_eq!(
            parse_route_choice("3", DecisionsRoute::Local),
            DecisionsRoute::Local
        );
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

        // A catalog decision row is local on a build whose linked kit can run
        // it; an LLM row in the same catalog is never local (it would fail at
        // the decision component). The GGUF row is llama.cpp-gated, so the
        // commons-only Windows ARM64 kit filters it out.
        assert_eq!(
            resolve_local_transport("clef-flash-9b", &options),
            Ok(cfg!(wally_has_llamacpp))
        );
        // The MLX row is platform-gated (Apple only), so off-macOS the
        // catalog filter hides it and it does not resolve locally.
        assert_eq!(
            resolve_local_transport("clef-flash-mlx", &options),
            Ok(cfg!(target_os = "macos"))
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
