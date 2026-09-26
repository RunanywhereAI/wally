//! test_wally_unit.cpp cases owned by the maintenance/diagnostics port. Port each C++ case below as a
//! `#[test] fn <same name>()` in this file, then delete its line from this list.
//! Test bodies are in explore-main/tests/test_wally_unit.cpp (grep the name).

#[allow(unused_imports)]
use super::common;

use wally::commands::bench_metrics::{fill_llm, fill_vlm};
use wally::commands::{collect_backend_rows, collect_llm_backend_rows};
use wally::io::proto::v1::{LlmGenerationResult, TokenUsage, VlmResult};
use wally::sys;

/// collect_llm_backend_rows() must be exactly the engines in
/// collect_backend_rows() that serve RAC_PRIMITIVE_GENERATE_TEXT — no more,
/// no fewer.
#[test]
fn llm_backend_rows_are_a_generate_text_subset() {
    let all = collect_backend_rows();
    let llm = collect_llm_backend_rows();
    // SAFETY: RAC_PRIMITIVE_GENERATE_TEXT is a valid, statically-known
    // primitive; rac_primitive_name returns a static string for it.
    let generate_text = unsafe {
        std::ffi::CStr::from_ptr(sys::rac_primitive_name(sys::RAC_PRIMITIVE_GENERATE_TEXT))
    }
    .to_string_lossy()
    .into_owned();

    for (name, row) in &llm {
        assert!(
            all.contains_key(name),
            "{name} is in the LLM view but not the full one"
        );
        assert!(
            row.primitives.contains(&generate_text),
            "{name} is in the LLM view without serving generate_text"
        );
    }
    for (name, row) in &all {
        let serves_llm = row.primitives.contains(&generate_text);
        assert_eq!(
            serves_llm,
            llm.contains_key(name),
            "{name} {}",
            if serves_llm {
                "serves generate_text but was filtered out"
            } else {
                "does not serve generate_text but was kept"
            }
        );
    }
}

/// bench_metrics::fill_llm/fill_vlm consume only the fields commons actually
/// reports (TokenUsage + measured phase timings); they must never invent a
/// wall-clock or a tokens÷rate throughput number, and must reject a result
/// with zero output tokens rather than fabricate a row for it.
#[test]
fn bench_metrics_consume_only() {
    // LLM: consume TokenUsage + measured phase fields; no tok/s or decode_ms invent.
    {
        let r = LlmGenerationResult {
            generation_time_ms: 1500.0,
            prompt_eval_time_ms: 200,
            decode_time_ms: 800,
            usage: Some(TokenUsage {
                output_tokens: 40,
                decode_tokens_per_second: 50.0,
                prefill_ms: 180,
                ttft_ms: 210, // must not alias into prompt_eval_ms
                ..Default::default()
            }),
            ..Default::default()
        };
        let m =
            fill_llm(&r, /* measured_e2e_ms= */ 9999.0).expect("fill_llm rejected a valid result");
        assert_eq!(m.output_tokens, 40);
        assert_eq!(m.end_to_end_ms, 1500.0);
        assert_eq!(m.tokens_per_second, 50.0);
        assert_eq!(m.decode_ms, 800.0);
        assert_eq!(m.prompt_eval_ms, 200.0);
    }

    // Missing decode throughput / phase times stay zero — no wall or tokens÷rate invent.
    {
        let r = LlmGenerationResult {
            usage: Some(TokenUsage {
                output_tokens: 256,
                ttft_ms: 500, // still must not become prefill
                ..Default::default()
            }),
            ..Default::default()
        };
        let m = fill_llm(&r, /* measured_e2e_ms= */ 20000.0)
            .expect("fill_llm should accept tokens with missing rates");
        assert_eq!(m.tokens_per_second, 0.0);
        assert_eq!(m.decode_ms, 0.0);
        assert_eq!(m.prompt_eval_ms, 0.0);
        assert_eq!(m.end_to_end_ms, 20000.0);
        assert_eq!(m.output_tokens, 256);
    }

    // Zero output tokens fail rather than inventing metrics.
    {
        let r = LlmGenerationResult {
            usage: Some(TokenUsage {
                decode_tokens_per_second: 99.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            fill_llm(&r, 1000.0).is_none(),
            "fill_llm must reject zero output tokens"
        );
    }

    // VLM: never invent tok/s from e2e; decode_ms always absent (no carrier).
    {
        let r = VlmResult {
            total_time_ms: 3000,
            usage: Some(TokenUsage {
                output_tokens: 64,
                prefill_ms: 120,
                ttft_ms: 130,
                ..Default::default()
            }),
            ..Default::default()
        };
        let m =
            fill_vlm(&r, /* measured_e2e_ms= */ 9999.0).expect("fill_vlm rejected a valid result");
        assert_eq!(m.tokens_per_second, 0.0);
        assert_eq!(m.decode_ms, 0.0);
        assert_eq!(m.prompt_eval_ms, 120.0);
        assert_eq!(m.end_to_end_ms, 3000.0);
        assert_eq!(m.output_tokens, 64);
    }

    // Prefill falls back to TokenUsage.prefill_ms when prompt_eval_time_ms is absent.
    {
        let r = LlmGenerationResult {
            usage: Some(TokenUsage {
                output_tokens: 8,
                prefill_ms: 77,
                ..Default::default()
            }),
            ..Default::default()
        };
        let m = fill_llm(&r, 100.0).expect("fill_llm rejected a valid result");
        assert_eq!(m.prompt_eval_ms, 77.0, "prefill_ms=77 from TokenUsage");
    }
}

/// A negative --trials must be rejected by the CLI's own PositiveNumber-style
/// validator (usage error -> exit 2) before run_bench ever loads a model, not
/// silently clamped to 1 trial and run anyway.
///
/// Blocked: wally::app::run (tests::common::run_in_process) is still
/// `todo!()`, owned by the CLI port.
#[test]
fn bench_negative_trials_exit2() {
    let code = common::run_in_process(&["bench", "--trials", "-5"]);
    assert_eq!(
        code, 2,
        "`bench --trials -5` should be a usage error, not a clamped run"
    );
}

/// `bench -n 0` should likewise be a usage error.
///
/// Blocked: wally::app::run (tests::common::run_in_process) is still
/// `todo!()`, owned by the CLI port.
#[test]
fn bench_zero_trials_exit2() {
    let code = common::run_in_process(&["bench", "-n", "0"]);
    assert_eq!(code, 2, "`bench -n 0` should be a usage error");
}
