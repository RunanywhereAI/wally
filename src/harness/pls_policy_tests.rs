//! Unit tests for the router's decision service helpers. A child module of
//! `pls`, so it sees the helpers through `super`. No model, no network.

use super::*;

use crate::account::decisions_contract as contract;
use serde_json::{json, Value};

const BODY_LIMIT: usize = 32 * 1024;

fn response_with(answers: Vec<(&str, Vec<(&str, f64)>)>) -> contract::DecisionsResponse {
    let answers = answers
        .into_iter()
        .map(|(id, labels)| {
            let probabilities = labels
                .into_iter()
                .map(|(label, probability)| (label.to_string(), probability))
                .collect();
            (
                id.to_string(),
                contract::DecisionAnswer {
                    probabilities: contract::LabelProbabilities(probabilities),
                    r#type: "yes_no".to_string(),
                    ..Default::default()
                },
            )
        })
        .collect();
    contract::DecisionsResponse {
        answers: contract::DecisionAnswers(answers),
        model: "clef-flash-9b".to_string(),
        object: "decisions".to_string(),
        ..Default::default()
    }
}

fn route_response(p_yes: f64) -> contract::DecisionsResponse {
    response_with(vec![("route", vec![("yes", p_yes), ("no", 1.0 - p_yes)])])
}

fn body(value: Value) -> Vec<u8> {
    serde_json::to_vec(&value).unwrap()
}

#[test]
fn parse_policy_accepts_privacy() {
    assert_eq!(parse_policy("privacy").unwrap().as_str(), "privacy");
}

#[test]
fn parse_policy_accepts_cost() {
    assert_eq!(parse_policy("cost").unwrap().as_str(), "cost");
}

#[test]
fn parse_policy_rejects_an_unknown_name() {
    assert!(parse_policy("speed").is_err());
}

#[test]
fn parse_policy_rejects_the_retired_local_name() {
    assert!(parse_policy("local").is_err());
}

#[test]
fn parse_policy_rejects_an_empty_string() {
    assert!(parse_policy("").is_err());
}

#[test]
fn a_policy_prints_the_name_it_was_parsed_from() {
    assert_eq!(Policy::Privacy.as_str(), "privacy");
    assert_eq!(Policy::Cost.as_str(), "cost");
}

#[test]
fn the_privacy_question_has_the_route_id() {
    assert_eq!(decide_question(Policy::Privacy).id, "route");
}

#[test]
fn the_cost_question_has_the_route_id() {
    assert_eq!(decide_question(Policy::Cost).id, "route");
}

#[test]
fn the_two_policies_ask_different_questions() {
    let privacy = decide_question(Policy::Privacy);
    let cost = decide_question(Policy::Cost);
    assert!(!privacy.question.trim().is_empty());
    assert!(!cost.question.trim().is_empty());
    assert_ne!(privacy.question, cost.question);
}

#[test]
fn the_policy_question_is_a_yes_no_question() {
    assert_eq!(decide_question(Policy::Privacy).r#type, "yes_no");
    assert_eq!(decide_question(Policy::Cost).r#type, "yes_no");
}

#[test]
fn the_decide_request_names_the_decision_model() {
    let request = build_decide_request(Policy::Privacy, "hello");
    assert_eq!(request.model, "mlx-clef-flash-9b-4bit");
}

#[test]
fn the_decide_request_carries_the_prompt_as_input() {
    let request = build_decide_request(Policy::Cost, "refactor the parser");
    assert_eq!(request.input, "refactor the parser");
}

#[test]
fn the_decide_request_holds_exactly_the_policy_question() {
    let request = build_decide_request(Policy::Cost, "x");
    assert_eq!(
        request.questions,
        vec![contract::DecisionQuestion::YesNoQuestion(decide_question(
            Policy::Cost
        ))]
    );
}

#[test]
fn the_decide_request_asks_a_different_question_per_policy() {
    let privacy = build_decide_request(Policy::Privacy, "x");
    let cost = build_decide_request(Policy::Cost, "x");
    assert_ne!(privacy.questions, cost.questions);
}

#[test]
fn the_decide_request_survives_a_round_trip_through_json() {
    let request = build_decide_request(Policy::Privacy, "my key is in .env");
    let again = contract::DecisionsRequest::from_json(&request.to_json()).unwrap();
    assert_eq!(again, request);
}

#[test]
fn a_decide_body_parses_policy_and_text() {
    let parsed = parse_decide_body(&body(json!({"policy": "cost", "text": "build it"}))).unwrap();
    assert_eq!(parsed.policy.as_str(), "cost");
    assert_eq!(parsed.text, "build it");
}

#[test]
fn a_decide_body_that_is_not_json_is_rejected() {
    assert!(parse_decide_body(b"not json at all").is_err());
}

#[test]
fn a_decide_body_that_is_not_an_object_is_rejected() {
    assert!(parse_decide_body(b"[\"privacy\", \"hi\"]").is_err());
}

#[test]
fn an_empty_decide_body_is_rejected() {
    assert!(parse_decide_body(b"").is_err());
}

#[test]
fn a_decide_body_with_an_unknown_policy_is_rejected() {
    let bytes = body(json!({"policy": "speed", "text": "hi"}));
    assert!(parse_decide_body(&bytes).is_err());
}

#[test]
fn a_decide_body_without_a_policy_is_rejected() {
    assert!(parse_decide_body(&body(json!({"text": "hi"}))).is_err());
}

#[test]
fn a_decide_body_without_text_is_rejected() {
    assert!(parse_decide_body(&body(json!({"policy": "privacy"}))).is_err());
}

#[test]
fn a_decide_body_with_empty_text_is_rejected() {
    let bytes = body(json!({"policy": "privacy", "text": ""}));
    assert!(parse_decide_body(&bytes).is_err());
}

#[test]
fn a_decide_body_with_blank_text_is_rejected() {
    let bytes = body(json!({"policy": "privacy", "text": " \n\t  "}));
    assert!(parse_decide_body(&bytes).is_err());
}

#[test]
fn a_decide_body_with_non_string_text_is_rejected() {
    let bytes = body(json!({"policy": "privacy", "text": 42}));
    assert!(parse_decide_body(&bytes).is_err());
}

#[test]
fn text_exactly_at_the_cap_is_kept_whole() {
    let text = "a".repeat(BODY_LIMIT);
    let parsed = parse_decide_body(&body(json!({"policy": "cost", "text": text}))).unwrap();
    assert_eq!(parsed.text.len(), BODY_LIMIT);
}

#[test]
fn text_over_the_cap_is_cut_to_the_cap() {
    let text = "a".repeat(BODY_LIMIT + 500);
    let parsed = parse_decide_body(&body(json!({"policy": "cost", "text": text}))).unwrap();
    assert_eq!(parsed.text.len(), BODY_LIMIT);
}

#[test]
fn text_over_the_cap_keeps_its_beginning() {
    let text = format!("start{}", "b".repeat(BODY_LIMIT));
    let parsed = parse_decide_body(&body(json!({"policy": "cost", "text": text}))).unwrap();
    assert!(parsed.text.starts_with("start"));
}

#[test]
fn text_is_cut_on_a_char_boundary_not_inside_a_character() {
    // Each euro sign is 3 bytes and 32768 is not a multiple of 3, so a naive
    // byte cut would land inside a character.
    let text = "\u{20ac}".repeat(20_000);
    let parsed = parse_decide_body(&body(json!({"policy": "privacy", "text": text}))).unwrap();
    assert!(parsed.text.len() <= BODY_LIMIT);
    assert_eq!(parsed.text.len(), BODY_LIMIT - BODY_LIMIT % 3);
    assert!(parsed.text.chars().all(|c| c == '\u{20ac}'));
}

#[test]
fn a_decide_reply_below_one_half_answers_no() {
    let reply = decide_reply(Policy::Cost, &route_response(0.3), 5);
    assert_eq!(reply["answer"], "no");
    assert_eq!(reply["p_yes"].as_f64(), Some(0.3));
}

#[test]
fn a_decide_reply_above_one_half_answers_yes() {
    let reply = decide_reply(Policy::Cost, &route_response(0.7), 5);
    assert_eq!(reply["answer"], "yes");
    assert_eq!(reply["p_yes"].as_f64(), Some(0.7));
}

#[test]
fn a_decide_reply_at_exactly_one_half_answers_yes() {
    let reply = decide_reply(Policy::Cost, &route_response(0.5), 5);
    assert_eq!(reply["answer"], "yes");
}

#[test]
fn privacy_treats_a_weak_yes_as_private() {
    let reply = decide_reply(Policy::Privacy, &route_response(0.4), 5);
    assert_eq!(reply["answer"], "yes");
}

#[test]
fn cost_at_the_same_weak_yes_answers_no() {
    let reply = decide_reply(Policy::Cost, &route_response(0.4), 5);
    assert_eq!(reply["answer"], "no");
}

#[test]
fn privacy_answers_no_only_below_its_threshold() {
    let reply = decide_reply(Policy::Privacy, &route_response(0.29), 5);
    assert_eq!(reply["answer"], "no");
}

#[test]
fn a_decide_reply_carries_the_model_name() {
    let reply = decide_reply(Policy::Cost, &route_response(0.9), 5);
    assert_eq!(reply["model"], "clef-flash-9b");
}

#[test]
fn a_decide_reply_carries_the_elapsed_milliseconds() {
    let reply = decide_reply(Policy::Cost, &route_response(0.9), 123);
    assert_eq!(reply["ms"].as_u64(), Some(123));
}

#[test]
fn a_successful_decide_reply_has_no_error_key() {
    let reply = decide_reply(Policy::Cost, &route_response(0.9), 5);
    assert!(reply.get("error").is_none());
}

#[test]
fn a_decide_reply_reads_p_yes_from_the_route_answer_only() {
    let response = response_with(vec![
        ("other", vec![("yes", 0.1), ("no", 0.9)]),
        ("route", vec![("yes", 0.8), ("no", 0.2)]),
    ]);
    let reply = decide_reply(Policy::Cost, &response, 5);
    assert_eq!(reply["answer"], "yes");
    assert_eq!(reply["p_yes"].as_f64(), Some(0.8));
}

#[test]
fn a_decide_reply_without_a_route_answer_is_an_error() {
    let response = response_with(vec![("other", vec![("yes", 0.9), ("no", 0.1)])]);
    let reply = decide_reply(Policy::Cost, &response, 5);
    assert!(reply.get("error").is_some());
    assert!(reply.get("answer").is_none());
}

#[test]
fn a_decide_reply_with_no_answers_is_an_error() {
    let reply = decide_reply(Policy::Cost, &contract::DecisionsResponse::default(), 5);
    assert!(reply.get("error").is_some());
}

#[test]
fn a_decide_reply_whose_route_answer_has_no_yes_label_is_an_error() {
    let response = response_with(vec![("route", vec![("no", 1.0)])]);
    let reply = decide_reply(Policy::Cost, &response, 5);
    assert!(reply.get("error").is_some());
}

#[test]
fn configure_value_sets_the_model_path() {
    let value = configure_value(json!({}), "/models/clef", "deepseek-v4", Policy::Privacy);
    assert_eq!(value["model_path"], "/models/clef");
}

#[test]
fn configure_value_sets_the_cloud_model() {
    let value = configure_value(json!({}), "/models/clef", "deepseek-v4", Policy::Privacy);
    assert_eq!(value["cloud_model"], "deepseek-v4");
}

#[test]
fn configure_value_sets_the_privacy_policy() {
    let value = configure_value(json!({}), "/m", "c", Policy::Privacy);
    assert_eq!(value["policy"], "privacy");
}

#[test]
fn configure_value_sets_the_cost_policy() {
    let value = configure_value(json!({}), "/m", "c", Policy::Cost);
    assert_eq!(value["policy"], "cost");
}

#[test]
fn configure_value_keeps_keys_it_does_not_own() {
    let existing = json!({"max_turns": 12, "mode": "hybrid"});
    let value = configure_value(existing, "/m", "c", Policy::Cost);
    assert_eq!(value["max_turns"], 12);
    assert_eq!(value["mode"], "hybrid");
}

#[test]
fn configure_value_replaces_the_previous_policy_and_paths() {
    let existing = json!({"policy": "privacy", "model_path": "/old", "cloud_model": "old"});
    let value = configure_value(existing, "/new", "new", Policy::Cost);
    assert_eq!(value["policy"], "cost");
    assert_eq!(value["model_path"], "/new");
    assert_eq!(value["cloud_model"], "new");
}

#[test]
fn configure_value_builds_an_object_when_there_was_no_config() {
    let value = configure_value(Value::Null, "/m", "c", Policy::Privacy);
    assert!(value.is_object());
    assert_eq!(value["policy"], "privacy");
}
