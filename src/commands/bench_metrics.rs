//! Per-run metric extraction for `wally bench` (port of
//! src/commands/bench_metrics.h).

use crate::io::proto::v1;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LlmVlmMetrics {
    pub end_to_end_ms: f64,
    pub tokens_per_second: f64,
    pub prompt_eval_ms: f64,
    pub decode_ms: f64,
    pub output_tokens: i32,
}

pub fn measured_prefill_ms(prompt_eval_time_ms: i64, usage_prefill_ms: i64) -> f64 {
    if prompt_eval_time_ms > 0 {
        return prompt_eval_time_ms as f64;
    }
    if usage_prefill_ms > 0 {
        return usage_prefill_ms as f64;
    }
    0.0
}

pub fn fill_llm(r: &v1::LlmGenerationResult, measured_e2e_ms: f64) -> Option<LlmVlmMetrics> {
    let usage = r.usage.clone().unwrap_or_default();
    let out_tokens = usage.output_tokens;
    if out_tokens <= 0 {
        return None;
    }
    Some(LlmVlmMetrics {
        output_tokens: out_tokens,
        end_to_end_ms: if r.generation_time_ms > 0.0 {
            r.generation_time_ms
        } else {
            measured_e2e_ms
        },
        tokens_per_second: usage.decode_tokens_per_second,
        decode_ms: if r.decode_time_ms > 0 {
            r.decode_time_ms as f64
        } else {
            0.0
        },
        prompt_eval_ms: measured_prefill_ms(r.prompt_eval_time_ms, usage.prefill_ms),
    })
}

pub fn fill_vlm(r: &v1::VlmResult, measured_e2e_ms: f64) -> Option<LlmVlmMetrics> {
    let usage = r.usage.clone().unwrap_or_default();
    let out_tokens = usage.output_tokens;
    if out_tokens <= 0 {
        return None;
    }
    Some(LlmVlmMetrics {
        output_tokens: out_tokens,
        end_to_end_ms: if r.total_time_ms > 0 {
            r.total_time_ms as f64
        } else {
            measured_e2e_ms
        },
        tokens_per_second: usage.decode_tokens_per_second,
        prompt_eval_ms: measured_prefill_ms(0, usage.prefill_ms),
        decode_ms: 0.0,
    })
}
