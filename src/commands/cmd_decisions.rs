//! Hosted Qwev decision scoring (`wally decisions`, alias `decide`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::time::Instant;

use crate::account::decisions_contract as contract;
use crate::account::{self, ConsoleClient, ConsoleSession};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output as out;
use crate::util::term;

const DEFAULT_MODEL: &str = "qwev";
const MAX_BODY_BYTES: usize = 1024 * 1024;

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

fn question_labels(request: &contract::DecisionsRequest) -> BTreeMap<String, String> {
    request
        .questions
        .iter()
        .map(|question| match question {
            contract::DecisionQuestion::YesNoQuestion(value) => {
                (value.id.clone(), value.question.clone())
            }
            contract::DecisionQuestion::ChoiceQuestion(value) => {
                (value.id.clone(), value.question.clone())
            }
            contract::DecisionQuestion::ScoreQuestion(value) => {
                (value.id.clone(), value.question.clone())
            }
        })
        .collect()
}

fn render_human(request: &contract::DecisionsRequest, response: &contract::DecisionsResponse) {
    let questions = question_labels(request);
    for (id, answer) in &response.answers.0 {
        out::result_line(questions.get(id).map(String::as_str).unwrap_or(id));
        let top = answer
            .probabilities
            .0
            .iter()
            .max_by(|left, right| left.1.total_cmp(right.1));
        if let Some((label, probability)) = top {
            out::result_line(&format!("{label}  {:.1}%", probability * 100.0));
        }
        for (label, probability) in &answer.probabilities.0 {
            let width = (probability * 20.0).round().clamp(0.0, 20.0) as usize;
            out::result_line(&format!(
                "  {label:<12} {:<20} {:>5.1}%",
                "█".repeat(width),
                probability * 100.0
            ));
        }
        if answer.label_mass < 0.5 {
            out::result_line("  note: low fit — most probability was outside these labels");
        }
        out::result_line("");
    }
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
            out::error_line("Sign in first: wally login");
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
                    "Qwev returned HTTP {status}; retrying{}",
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
    let cmd = app.add_subcommand("decisions", "Score questions with hosted Qwev");
    cmd.alias("decide");
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        "Decision model (default qwev)",
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
            model: "qwev".to_string(),
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
}
