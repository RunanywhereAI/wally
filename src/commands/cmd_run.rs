//! `wally llm generate|stream`, `wally vlm generate`, and the terminal alias
//! `wally run`. Port of src/commands/cmd_run.cpp.
//!
//! Canonical SDK flow, all heavy lifting in commons:
//!   rac_model_lifecycle_load_proto(validate_availability=true) -> auto-pulls
//!   missing models through the download orchestrator (progress rendered via
//!   DownloadProgressScope), resolves artifact paths (incl. VLM mmproj) and
//!   loads the engine once.
//!   llm generate: rac_llm_generate_proto returns one LLMGenerationResult.
//!   llm stream:   rac_llm_generate_stream_proto streams LLMStreamEvent
//!   protos; ANSWER tokens go to stdout, THOUGHT tokens to stderr (dimmed,
//!   hidden with -q or --hide-thinking).
//!   VLM: rac_vlm_generate_proto (unary) returns a VLMResult.
//!   Ctrl-C: rac_llm_cancel_proto from the token callback thread.
//!
//! REPL turns are independent generations (no cross-turn memory yet -- that
//! needs a commons chat-session API; tracked in the wally plan doc).
//!
//! Triage A1 (streaming callback lifetime): rac_llm_generate_stream_proto's
//! doc comment warns the SDK may invoke `callback` on a background thread
//! AFTER the function has returned, because the dispatcher copies the
//! callback slot under its internal mutex and releases the mutex before
//! invoking the user callback. The C++ original handed a pointer to a
//! function-local stack `GenState` through a process-global `g_gen`, cleared
//! to null right after the call returns with no wait for in-flight callbacks
//! -- a genuine use-after-free race the C++ never observed only by luck of
//! timing. This port instead hands the SDK a strong `Arc<GenState>` reference
//! via `Arc::into_raw` as `user_data`, and only reclaims it (`Arc::from_raw`)
//! after `rac_llm_proto_quiesce()` has returned, which spin-waits until every
//! in-flight callback invocation is done. The callback itself never touches
//! `user_data` except to borrow through it, and its whole body runs inside
//! `catch_unwind` so a panic can never cross the FFI boundary.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use prost::Message as _;

use crate::bootstrap::{self, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, Parsed, Validator, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::config::cli_paths;
use crate::io::output::{describe_result, error_line, result_line, status_line, JsonWriter};
use crate::io::proto::{self, parse_proto_buffer, v1, ProtoBuffer};
use crate::progress::progress_bar::DownloadProgressScope;
use crate::repl::repl::LineEditor;
use crate::sys;
use crate::util::{getenv, term};

use super::engine_options::{engine_choices, resolve_engine_hint};
use super::{LlmVerb, ModelArg};

// ---------------------------------------------------------------------------
// Generation parameters shared by one-shot, streaming and REPL turns. Field
// names follow LlmOptions in the public API spec.
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
struct RunParams {
    model: String,
    image: String,
    system_prompt: String,
    engine: String,
    /// optional LoRA adapter (.gguf) to attach before generating
    lora: String,
    /// how strongly the adapter applies
    lora_scale: f32,
    /// on | off
    reasoning: String,
    /// reasoning.include_in_output
    show_thinking: bool,
    /// 0 = engine default
    temperature: f32,
    top_p: f32,
    min_p: f32,
    repetition_penalty: f32,
    frequency_penalty: f32,
    presence_penalty: f32,
    top_k: i32,
    max_output_tokens: i32,
    /// -1 = unset; LLMGenerationOptions.seed default is 0
    seed: i64,
    stop_sequences: Vec<String>,
}

impl Default for RunParams {
    fn default() -> Self {
        RunParams {
            model: String::new(),
            image: String::new(),
            system_prompt: String::new(),
            engine: String::new(),
            lora: String::new(),
            lora_scale: 1.0,
            reasoning: "on".to_string(),
            show_thinking: true,
            temperature: 0.0,
            top_p: 0.0,
            min_p: 0.0,
            repetition_penalty: 0.0,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            top_k: 0,
            max_output_tokens: 1024,
            seed: -1,
            stop_sequences: Vec::new(),
        }
    }
}

impl RunParams {
    /// Reconstructs RunParams by reading back every option this command
    /// registered on `p`. Where C++ bound CLI11 options directly onto a
    /// `shared_ptr<RunParams>` at registration time, this parser instead
    /// reads them back by name from the completed parse -- same option
    /// specs, defaults and validators, different plumbing.
    fn from_parsed(p: &Parsed, model_arg: ModelArg) -> RunParams {
        let mut params = RunParams {
            model: match model_arg {
                ModelArg::Option => p.get_str("--model").unwrap_or_default(),
                ModelArg::Positional => p.get_str("model").unwrap_or_default(),
            },
            image: p.get_str("--image").unwrap_or_default(),
            system_prompt: p.get_str("--system-prompt").unwrap_or_default(),
            lora: p.get_str("--lora").unwrap_or_default(),
            lora_scale: p.get_f64("--lora-scale").unwrap_or(1.0) as f32,
            engine: p.get_str("--engine").unwrap_or_default(),
            temperature: p.get_f64("--temperature").unwrap_or(0.0) as f32,
            top_p: p.get_f64("--top-p").unwrap_or(0.0) as f32,
            top_k: p.get_i64("--top-k").unwrap_or(0) as i32,
            min_p: p.get_f64("--min-p").unwrap_or(0.0) as f32,
            repetition_penalty: p.get_f64("--repetition-penalty").unwrap_or(0.0) as f32,
            frequency_penalty: p.get_f64("--frequency-penalty").unwrap_or(0.0) as f32,
            presence_penalty: p.get_f64("--presence-penalty").unwrap_or(0.0) as f32,
            seed: p.get_i64("--seed").unwrap_or(-1),
            stop_sequences: p.get_strs("--stop"),
            max_output_tokens: p.get_i64("--max-output-tokens").unwrap_or(1024) as i32,
            reasoning: p.get_str("--reasoning").unwrap_or_else(|| "on".to_string()),
            show_thinking: p.flag("--show-thinking"),
        };
        // --no-think is CLI11's add_flag_callback side effect; applied after
        // --reasoning is read back so it always wins when both are given,
        // regardless of argv order (a minor, disclosed simplification of
        // CLI11's strict left-to-right callback timing).
        if p.flag("--no-think") {
            params.reasoning = "off".to_string();
        }
        params
    }
}

fn reasoning_off(params: &RunParams) -> bool {
    params.reasoning == "off"
}

/// C's `std::to_string(double)`: fixed-point, 6 digits after the point.
fn to_string_f64(value: f64) -> String {
    format!("{value:.6}")
}

/// Fill LLMGenerationOptions from the parsed flags. Zero means "leave it to
/// the engine default" for every sampling knob the proto declares as
/// non-optional.
fn apply_options(params: &RunParams, gen: &mut v1::LlmGenerationOptions) {
    gen.max_output_tokens = Some(params.max_output_tokens);
    if params.temperature > 0.0 {
        gen.temperature = Some(params.temperature);
    }
    if params.top_p > 0.0 {
        gen.top_p = Some(params.top_p);
    }
    if params.top_k > 0 {
        gen.top_k = Some(params.top_k);
    }
    if params.min_p > 0.0 {
        gen.min_p = Some(params.min_p);
    }
    if params.repetition_penalty > 0.0 {
        gen.repeat_penalty = Some(params.repetition_penalty);
    }
    if params.frequency_penalty != 0.0 {
        gen.frequency_penalty = Some(params.frequency_penalty);
    }
    if params.presence_penalty != 0.0 {
        gen.presence_penalty = Some(params.presence_penalty);
    }
    if params.seed >= 0 {
        gen.seed = Some(params.seed);
    }
    gen.stop_sequences = params.stop_sequences.clone();
    if !params.system_prompt.is_empty() {
        gen.system_prompt = Some(params.system_prompt.clone());
    }
    let mut reasoning = v1::ReasoningOptions::default();
    if reasoning_off(params) {
        reasoning.mode = v1::ReasoningMode::Off as i32;
    } else {
        reasoning.include_in_output = params.show_thinking;
    }
    gen.reasoning = Some(reasoning);
}

// ---------------------------------------------------------------------------
// Ctrl-C handling. Each stream_once call owns Ctrl-C for as long as it
// streams, like C++'s std::signal/restore-per-call pattern (the REPL calls
// stream_once repeatedly); the shared flag is reset at the top of every call.
// ---------------------------------------------------------------------------
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Streaming state shared with the LLM proto callback (Triage A1: reachable
// from a background thread after stream_once returns, so it lives in an
// Arc and is only freed after rac_llm_proto_quiesce()).
// ---------------------------------------------------------------------------
#[derive(Default)]
struct GenStateData {
    done: bool,
    cancelled: bool,
    answer: String,
    finish_reason: String,
    error: String,
    show_thoughts: bool,
    in_thought_block: bool,
    /// false in --json mode (accumulate only)
    stream_to_stdout: bool,
}

struct GenState {
    data: Mutex<GenStateData>,
    cv: Condvar,
}

fn lock_gen_state(state: &GenState) -> std::sync::MutexGuard<'_, GenStateData> {
    state
        .data
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn handle_llm_stream_event(state: &GenState, event: &v1::LlmStreamEvent) {
    // Ctrl-C: cancel from this (callback) thread -- signal handlers must not
    // do blocking/FFI work themselves.
    if INTERRUPTED.load(Ordering::SeqCst) {
        let mut data = lock_gen_state(state);
        if !data.cancelled {
            data.cancelled = true;
            drop(data);
            let mut cancel_event = ProtoBuffer::new();
            // SAFETY: cancel_event is a live, initialized rac_proto_buffer_t;
            // the call only writes into it for the duration of this call.
            unsafe {
                sys::rac_llm_cancel_proto(cancel_event.as_mut_ptr());
            }
        }
    }

    if !event.token.is_empty() {
        let mut data = lock_gen_state(state);
        if event.event_kind == v1::LlmStreamEventKind::Thinking as i32 {
            if data.show_thoughts {
                if !data.in_thought_block {
                    eprint!("{}", if term::color_enabled() { "\x1b[2m" } else { "" });
                    data.in_thought_block = true;
                }
                eprint!("{}", event.token);
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
        } else {
            if data.in_thought_block {
                eprintln!("{}", if term::color_enabled() { "\x1b[0m" } else { "" });
                data.in_thought_block = false;
            }
            // Swallow the leading-whitespace artifact left by think-tag
            // stripping (qwen3 emits "\n\n" before the first answer token).
            let mut token = event.token.clone();
            if data.answer.is_empty() {
                match token.find(|c: char| !matches!(c, ' ' | '\t' | '\r' | '\n')) {
                    Some(first) => token = token[first..].to_string(),
                    None => token.clear(),
                }
            }
            if !token.is_empty() {
                if data.stream_to_stdout {
                    print!("{token}");
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                data.answer.push_str(&token);
            }
        }
    }

    if event.event_kind == v1::LlmStreamEventKind::Completed as i32
        || event.event_kind == v1::LlmStreamEventKind::Error as i32
    {
        let mut data = lock_gen_state(state);
        if data.in_thought_block {
            eprintln!("{}", if term::color_enabled() { "\x1b[0m" } else { "" });
            data.in_thought_block = false;
        }
        data.finish_reason = v1::FinishReason::try_from(event.finish_reason)
            .map(|f| f.as_str_name().to_string())
            .unwrap_or_default();
        if let Some(err) = &event.error {
            if !err.message.is_empty() {
                data.error = err.message.clone();
            }
        }
        data.done = true;
        state.cv.notify_all();
    }
}

/// The trampoline the SDK invokes, possibly on a background thread and
/// possibly after `rac_llm_generate_stream_proto` has already returned to its
/// caller (Triage A1). Never lets a panic cross the FFI boundary.
extern "C" fn llm_stream_callback(
    event_bytes: *const u8,
    event_size: usize,
    user_data: *mut c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if user_data.is_null() || event_bytes.is_null() {
            return;
        }
        // SAFETY: user_data is the `*const GenState` produced by
        // `Arc::into_raw` in stream_once and kept alive (by the Arc strong
        // count stream_once still holds) until quiesce() has returned there,
        // which only happens after every in-flight invocation of this
        // trampoline -- including this one -- has returned. Borrowing
        // through it without consuming it is the documented safe use of a
        // pointer obtained from Arc::into_raw.
        let state: &GenState = unsafe { &*(user_data as *const GenState) };
        // SAFETY: event_bytes/event_size describe a buffer valid only for
        // the duration of this call, per rac_llm_generate_stream_proto's doc
        // comment; it is not retained past this function.
        let bytes = unsafe { std::slice::from_raw_parts(event_bytes, event_size) };
        let event = match v1::LlmStreamEvent::decode(bytes) {
            Ok(event) => event,
            Err(_) => return,
        };
        handle_llm_stream_event(state, &event);
    });
}

/// One blocking streaming generation; returns 0 ok, 1 error, 130 user-cancel.
fn stream_once(options: &GlobalOptions, model_id: &str, prompt: &str, params: &RunParams) -> i32 {
    let mut request = v1::LlmGenerateRequest {
        messages: vec![v1::ChatMessage {
            role: v1::MessageRole::User as i32,
            content: prompt.to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut gen_options = v1::LlmGenerationOptions::default();
    apply_options(params, &mut gen_options);
    request.options = Some(gen_options);
    let _ = model_id; // lifecycle-owned state knows the loaded model

    let show_thoughts =
        params.show_thinking && !reasoning_off(params) && !options.quiet && !options.json;
    let state = Arc::new(GenState {
        data: Mutex::new(GenStateData {
            show_thoughts,
            stream_to_stdout: !options.json,
            ..Default::default()
        }),
        cv: Condvar::new(),
    });

    let _interrupt =
        crate::util::interrupt::on_interrupt(|| INTERRUPTED.store(true, Ordering::SeqCst));
    INTERRUPTED.store(false, Ordering::SeqCst);

    let started = std::time::Instant::now();
    let bytes = proto::serialize(&request);
    // Hand the SDK a strong Arc reference as user_data; reclaimed only after
    // rac_llm_proto_quiesce() below (Triage A1 -- see module doc comment).
    let user_data = Arc::into_raw(Arc::clone(&state)) as *mut c_void;
    // SAFETY: bytes/len describe a buffer valid for the duration of this
    // call; user_data is a live Arc strong pointer, reclaimed exactly once
    // below only after quiesce() proves no callback invocation can still be
    // running against it.
    let rc = unsafe {
        sys::rac_llm_generate_stream_proto(
            bytes.as_ptr(),
            bytes.len(),
            Some(llm_stream_callback),
            user_data,
        )
    };

    let mut exit_code = 0;
    if rc != sys::SUCCESS {
        error_line(&format!("generation failed: {}", describe_result(rc)));
        exit_code = 1;
    } else {
        let mut data = lock_gen_state(&state);
        loop {
            let (guard, _timeout) = state
                .cv
                .wait_timeout_while(data, std::time::Duration::from_millis(200), |d| !d.done)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            data = guard;
            if data.done {
                break;
            }
            if INTERRUPTED.load(Ordering::SeqCst) {
                drop(data);
                let mut cancel_event = ProtoBuffer::new();
                // SAFETY: cancel_event is live/initialized for the call.
                unsafe {
                    sys::rac_llm_cancel_proto(cancel_event.as_mut_ptr());
                }
                data = lock_gen_state(&state);
                let (guard2, timeout2) = state
                    .cv
                    .wait_timeout_while(data, std::time::Duration::from_secs(2), |d| !d.done)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                data = guard2;
                if timeout2.timed_out() && !data.done {
                    data.done = true;
                    data.cancelled = true;
                }
                break;
            }
        }

        if !data.answer.is_empty() && !data.answer.ends_with('\n') && !options.json {
            println!();
        }
        let elapsed_ms = started.elapsed().as_millis() as i64;
        if !data.error.is_empty() {
            error_line(&format!("generation failed: {}", data.error));
            exit_code = 1;
        } else if data.cancelled {
            status_line("(cancelled)");
            exit_code = 130;
        } else if options.json {
            let mut json = JsonWriter::new();
            json.begin_object()
                .field_str("model", model_id)
                .field_str("response", &data.answer)
                .field_str("finish_reason", &data.finish_reason)
                .field_i64("total_ms", elapsed_ms)
                .end_object();
            result_line(json.str());
        } else if options.verbose {
            status_line(&format!("({elapsed_ms} ms)"));
        }
    }

    // Triage A1: no NEW dispatch can start once this call has returned and
    // (on the success path) the terminal event has been observed or
    // cancellation has been driven to its own terminal event / 2s timeout
    // above -- but the SDK's doc comment warns a dispatch already in flight
    // may still be executing on a background thread. Spin-wait until every
    // in-flight invocation has returned before reclaiming the Arc strong
    // reference lent to it as user_data. There is nothing to "unset" first:
    // unlike the VLM lifecycle callback slot, rac_llm_generate_stream_proto
    // takes its callback per call rather than through a persistent slot.
    // SAFETY: quiesce() takes no arguments and is documented safe to call
    // from any thread at any time.
    unsafe {
        sys::rac_llm_proto_quiesce();
    }
    // SAFETY: user_data was produced by Arc::into_raw above and is reclaimed
    // exactly once, only after quiesce() has proven no callback invocation
    // can still be dereferencing it.
    unsafe {
        drop(Arc::from_raw(user_data as *const GenState));
    }

    exit_code
}

/// Decides one-shot generation success/failure from the parsed result, same
/// presence-based rule as `lora_apply_outcome`: an error envelope with an
/// empty message still fails rather than rendering empty text as a result.
fn generation_outcome(result: &v1::LlmGenerationResult) -> Result<(), String> {
    match &result.error {
        None => Ok(()),
        Some(err) if err.message.is_empty() => Err("unknown error".to_string()),
        Some(err) => Err(err.message.clone()),
    }
}

/// One unary generation (`llm generate`): the whole result lands at once.
fn generate_once(options: &GlobalOptions, model_id: &str, prompt: &str, params: &RunParams) -> i32 {
    let mut request = v1::LlmGenerateRequest {
        messages: vec![v1::ChatMessage {
            role: v1::MessageRole::User as i32,
            content: prompt.to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut gen_options = v1::LlmGenerationOptions::default();
    apply_options(params, &mut gen_options);
    request.options = Some(gen_options);

    let bytes = proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // a live, initialized rac_proto_buffer_t.
    let rc = unsafe {
        sys::rac_llm_generate_proto(bytes.as_ptr(), bytes.len(), out_buffer.as_mut_ptr())
    };
    let result: v1::LlmGenerationResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        // parse_proto_buffer only fills an error detail on ITS OWN failure
        // path; when the buffer parsed cleanly but the call's own rc is a
        // failure, C++ prints the still-empty `error` string here, so match
        // that with no trailing detail rather than describe_result(rc).
        Ok(_) => {
            error_line("generation failed: ");
            return 1;
        }
        Err(message) => {
            error_line(&format!("generation failed: {message}"));
            return 1;
        }
    };

    if let Err(message) = generation_outcome(&result) {
        error_line(&format!("generation failed: {message}"));
        return 1;
    }

    let usage = result.usage.unwrap_or_default();
    if options.json {
        let model_used = if result.model_used.is_empty() {
            model_id
        } else {
            result.model_used.as_str()
        };
        let finish_reason = v1::FinishReason::try_from(result.finish_reason)
            .map(|f| f.as_str_name())
            .unwrap_or("");
        let mut json = JsonWriter::new();
        json.begin_object()
            .field_str("model", model_used)
            .field_str("response", &result.text)
            .field_str("thinking", result.thinking_content.as_deref().unwrap_or(""))
            .field_str("finish_reason", finish_reason)
            .field_i64("input_tokens", usage.input_tokens as i64)
            .field_i64("output_tokens", usage.output_tokens as i64)
            .field_f64("tokens_per_second", usage.decode_tokens_per_second)
            .field_i64("total_ms", result.generation_time_ms as i64)
            .end_object();
        result_line(json.str());
        return 0;
    }
    if params.show_thinking && !reasoning_off(params) && !options.quiet {
        if let Some(thinking) = &result.thinking_content {
            if !thinking.is_empty() {
                eprintln!(
                    "{}{}{}",
                    if term::color_enabled() { "\x1b[2m" } else { "" },
                    thinking,
                    if term::color_enabled() { "\x1b[0m" } else { "" }
                );
            }
        }
    }
    result_line(&result.text);
    if options.verbose {
        status_line(&format!(
            "({} ms, {} tok/s)",
            result.generation_time_ms as i64,
            to_string_f64(usage.decode_tokens_per_second)
        ));
    }
    0
}

/// Auto-pull (validate_availability) + resolve + engine load, one call.
fn load_model(
    options: &GlobalOptions,
    model_id: &str,
    framework: v1::InferenceFramework,
    is_vlm: bool,
) -> bool {
    let _progress_scope =
        DownloadProgressScope::new(model_id, !options.no_progress && !options.json);
    let mut request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        validate_availability: true,
        ..Default::default()
    };
    if framework != v1::InferenceFramework::Unspecified {
        request.framework = Some(framework as i32);
    }
    if is_vlm {
        request.category = Some(v1::ModelCategory::Multimodal as i32);
    }
    let bytes = proto::serialize(&request);

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle; bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result: v1::ModelLoadResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        // See the matching comment in generate_once: parse_proto_buffer only
        // populates an error detail on its own failure path, so a clean parse
        // with a failing rc prints no trailing detail in C++.
        Ok(_) => {
            error_line("model load failed: ");
            return false;
        }
        Err(message) => {
            error_line(&format!("model load failed: {message}"));
            return false;
        }
    };
    if let Some(err) = &result.error {
        let message = if err.message.is_empty() {
            "unknown error"
        } else {
            &err.message
        };
        error_line(&format!("model load failed: {message}"));
        return false;
    }
    if options.verbose {
        status_line(&format!("loaded {}", result.resolved_path));
    }
    true
}

/// Decides vlm-generation success/failure from the parsed result, same
/// presence-based rule as `lora_apply_outcome` and `generation_outcome`: an
/// error envelope with an empty message still fails rather than rendering
/// empty text as a result.
fn vlm_generation_outcome(result: &v1::VlmResult) -> Result<(), String> {
    match &result.error {
        None => Ok(()),
        Some(err) if err.message.is_empty() => Err("unknown error".to_string()),
        Some(err) => Err(err.message.clone()),
    }
}

fn run_vlm(
    options: &GlobalOptions,
    model_id: &str,
    image_path: &str,
    prompt: &str,
    params: &RunParams,
) -> i32 {
    let mut request = v1::VlmGenerationRequest {
        model_id: Some(model_id.to_string()),
        images: vec![v1::VlmImage {
            source: Some(v1::vlm_image::Source::FilePath(image_path.to_string())),
            ..Default::default()
        }],
        prompt: if prompt.is_empty() {
            "Describe this image.".to_string()
        } else {
            prompt.to_string()
        },
        ..Default::default()
    };
    // VLMGenerationRequest.options is the shared LLMGenerationOptions, so
    // frequency/presence penalty (and every other sampling knob) apply to
    // VLM generation exactly like the LLM path -- one mapper for both.
    let mut gen_options = v1::LlmGenerationOptions::default();
    apply_options(params, &mut gen_options);
    request.options = Some(gen_options);

    let bytes = proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc = unsafe {
        sys::rac_vlm_generate_proto(bytes.as_ptr(), bytes.len(), out_buffer.as_mut_ptr())
    };
    let result: v1::VlmResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        // See the matching comment in generate_once: parse_proto_buffer only
        // populates an error detail on its own failure path, so a clean parse
        // with a failing rc prints no trailing detail in C++.
        Ok(_) => {
            error_line("vlm generation failed: ");
            return 1;
        }
        Err(message) => {
            error_line(&format!("vlm generation failed: {message}"));
            return 1;
        }
    };
    if let Err(message) = vlm_generation_outcome(&result) {
        error_line(&format!("vlm generation failed: {message}"));
        return 1;
    }

    let usage = result.usage.unwrap_or_default();
    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object()
            .field_str("model", model_id)
            .field_str("response", &result.text)
            .field_i64("total_ms", result.total_time_ms)
            .field_f64("tokens_per_second", usage.decode_tokens_per_second)
            .end_object();
        result_line(json.str());
    } else {
        result_line(&result.text);
        if options.verbose {
            status_line(&format!(
                "({} ms, {} tok/s)",
                result.total_time_ms,
                to_string_f64(usage.decode_tokens_per_second)
            ));
        }
    }
    0
}

fn print_repl_help() {
    status_line("commands:");
    status_line("  /set system <text>          set the system prompt");
    status_line("  /set temperature <float>    set sampling temperature");
    status_line("  /set max-output-tokens <n>  set the generation budget");
    status_line("  /show                       show current settings");
    status_line("  /bye                        exit (also Ctrl-D)");
    status_line("note: turns are independent — no conversation memory yet");
}

fn run_repl(options: &GlobalOptions, model_id: &str, mut params: RunParams) -> i32 {
    status_line(&format!(
        "loaded {model_id} — type a prompt, /? for help, /bye to exit"
    ));
    let history_path = if getenv("RUNANYWHERE_NOHISTORY").is_some() {
        String::new()
    } else {
        format!("{}/history", cli_paths::state_dir())
    };
    let mut editor = LineEditor::new(&history_path);

    while let Some(line) = editor.read_line("» ") {
        if line.is_empty() {
            continue;
        }
        editor.add_history(&line);

        if line == "/bye" || line == "/exit" || line == "/quit" {
            break;
        }
        if line == "/?" || line == "/help" {
            print_repl_help();
            continue;
        }
        if line == "/show" {
            status_line(&format!("model              {model_id}"));
            let system_prompt = if params.system_prompt.is_empty() {
                "(none)".to_string()
            } else {
                params.system_prompt.clone()
            };
            status_line(&format!("system-prompt      {system_prompt}"));
            let temperature = if params.temperature > 0.0 {
                to_string_f64(params.temperature as f64)
            } else {
                "(engine default)".to_string()
            };
            status_line(&format!("temperature        {temperature}"));
            status_line(&format!("max-output-tokens  {}", params.max_output_tokens));
            status_line(&format!("reasoning          {}", params.reasoning));
            continue;
        }
        if let Some(rest) = line.strip_prefix("/set ") {
            if let Some(value) = rest.strip_prefix("system ") {
                params.system_prompt = value.to_string();
                status_line("system prompt set");
            } else if let Some(value) = rest.strip_prefix("temperature ") {
                params.temperature = strtof_prefix(value);
                status_line("temperature set");
            } else if let Some(value) = rest.strip_prefix("temp ") {
                params.temperature = strtof_prefix(value);
                status_line("temperature set");
            } else if let Some(value) = rest.strip_prefix("max-output-tokens ") {
                params.max_output_tokens = strtol_prefix(value);
                status_line("max-output-tokens set");
            } else if let Some(value) = rest.strip_prefix("max-tokens ") {
                params.max_output_tokens = strtol_prefix(value);
                status_line("max-output-tokens set");
            } else {
                status_line("unknown /set option (system | temperature | max-output-tokens)");
            }
            continue;
        }
        if line.starts_with('/') {
            status_line("unknown command — /? for help");
            continue;
        }

        let code = stream_once(options, model_id, &line, &params);
        if code == 1 {
            return 1; // hard error; cancel (130) just returns to the prompt
        }
    }
    0
}

/// Mirrors `std::strtof(s.c_str(), nullptr)`: parses a leading (after
/// optional whitespace) floating-point prefix and ignores everything after
/// it, returning 0.0 when no valid prefix is present. Rust's `f32::from_str`
/// requires the WHOLE trimmed string to be numeric, so `"0.7 please"` fails
/// there but must still parse to 0.7 here, matching `/set temperature`.
fn strtof_prefix(s: &str) -> f32 {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut saw_digit = false;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        saw_digit = true;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return 0.0;
    }
    let mut end = i;
    if end < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut j = end + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let mut saw_exp_digit = false;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
            saw_exp_digit = true;
        }
        if saw_exp_digit {
            end = j;
        }
    }
    trimmed[..end].parse().unwrap_or(0.0)
}

/// Mirrors `std::strtol(s.c_str(), nullptr, 10)` truncated to `int32_t`:
/// parses a leading (after optional whitespace) base-10 integer prefix and
/// ignores everything after it, returning 0 when no valid prefix is present.
/// Rust's `i32::from_str` requires the WHOLE trimmed string to be numeric, so
/// `"256 tokens"` fails there but must still parse to 256 here, matching
/// `/set max-output-tokens`.
fn strtol_prefix(s: &str) -> i32 {
    let trimmed = s.trim_start();
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let start_digits = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start_digits {
        return 0;
    }
    // strtol clamps to LONG_MIN/LONG_MAX on overflow before the C++ side's
    // static_cast<int32_t> truncates; REPL input overflowing i64 is a
    // pathological case neither side's user-facing behaviour depends on, so
    // this falls back to 0 rather than reproducing the clamp-then-truncate.
    trimmed[..i].parse::<i64>().map(|v| v as i32).unwrap_or(0)
}

/// Lossily decodes piped stdin bytes to a prompt string and trims trailing
/// `\n`/`\r`, exactly like the C++ raw-byte `read_piped_prompt` did except
/// for the UTF-8 conversion prost's `String`-typed proto fields require.
fn decode_piped_prompt(bytes: &[u8]) -> String {
    let mut piped = String::from_utf8_lossy(bytes).into_owned();
    while piped.ends_with('\n') || piped.ends_with('\r') {
        piped.pop();
    }
    piped
}

fn read_piped_prompt() -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    // Read raw bytes, not read_to_string: on ANY invalid UTF-8 byte,
    // read_to_string leaves its destination String untouched (never
    // partially filled), which silently drops the whole prompt. C++ forwards
    // the raw byte content unchanged; prost's proto3 string fields require a
    // Rust `String`, so from_utf8_lossy (replacing invalid bytes with
    // U+FFFD) is the closest achievable match -- the prompt survives instead
    // of vanishing.
    let _ = std::io::stdin().read_to_end(&mut bytes);
    decode_piped_prompt(&bytes)
}

/// Attach a LoRA adapter to the already-loaded LLM in this same process, so
/// the following generation actually uses it (adapter state is
/// session-scoped).
/// Decides lora-apply success/failure from the parsed result + call rc,
/// mirroring C++'s `!parsed || rc != RAC_SUCCESS || result.has_error()` gate:
/// an error envelope with an empty message still fails (unlike checking the
/// message alone, which would treat a present-but-empty-message error as
/// success).
fn lora_apply_outcome(result: &v1::LoraApplyResult, rc: sys::rac_result_t) -> Result<(), String> {
    let has_error = result.error.is_some();
    if rc == sys::SUCCESS && !has_error {
        return Ok(());
    }
    let message = result
        .error
        .as_ref()
        .and_then(|e| (!e.message.is_empty()).then(|| e.message.clone()))
        .unwrap_or_else(|| rc.to_string());
    Err(message)
}

fn apply_lora_adapter(adapter_path: &str, scale: f32) -> bool {
    // keep_existing left unset (false): SET semantics, which is what the
    // former explicit replace_existing(true) meant. LoraApplyRequest has no
    // replace_existing field to set.
    let request = v1::LoraApplyRequest {
        adapters: vec![v1::LoraAdapterConfig {
            adapter_path: Some(adapter_path.to_string()),
            scale: Some(scale),
            ..Default::default()
        }],
        ..Default::default()
    };
    let bytes = proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc =
        unsafe { sys::rac_lora_apply_proto(bytes.as_ptr(), bytes.len(), out_buffer.as_mut_ptr()) };
    match parse_proto_buffer::<v1::LoraApplyResult>(out_buffer) {
        Ok(result) => match lora_apply_outcome(&result, rc) {
            Ok(()) => true,
            Err(message) => {
                error_line(&format!("lora apply failed: {message}"));
                false
            }
        },
        Err(message) => {
            let message = if message.is_empty() {
                rc.to_string()
            } else {
                message
            };
            error_line(&format!("lora apply failed: {message}"));
            false
        }
    }
}

fn run_llm(options: &GlobalOptions, verb: LlmVerb, prompt: &str, params: &RunParams) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }
    if params.model.is_empty() {
        error_line("--model is required (a catalog id, alias, hf.co/... ref or URL)");
        return 2;
    }

    let mut engine_hint = match resolve_engine_hint(&params.engine) {
        Ok(hint) => hint,
        Err(message) => {
            error_line(&message);
            return 2;
        }
    };

    let is_vlm = !params.image.is_empty();
    if is_vlm {
        engine_hint.resolve_options.has_category = true;
        engine_hint.resolve_options.category = v1::ModelCategory::Multimodal;
    }

    let resolved = match model_ref::resolve(&params.model, Some(&engine_hint.resolve_options)) {
        Ok(resolved) => resolved,
        Err((_, message)) => {
            error_line(&message);
            return 1;
        }
    };

    // An explicit --engine is honoured whatever the ref resolved to. This used to
    // read `resolved.from_catalog ? UNSPECIFIED : engine_hint.framework`, which
    // silently DISCARDED the flag for anything that came out of the built-in
    // catalog -- `--engine <x>` on a catalog model did nothing at all, with no
    // warning. When the flag is absent engine_hint.framework is UNSPECIFIED, so
    // catalog entries still fall back to their own declared framework exactly as
    // before; the only behaviour that changes is that asking now works. Mirrors
    // cmd_embed.cpp.
    if !load_model(options, &resolved.model_id, engine_hint.framework, is_vlm) {
        return 1;
    }
    if !params.lora.is_empty() && !apply_lora_adapter(&params.lora, params.lora_scale) {
        return 1;
    }

    let mut effective_prompt = prompt.to_string();
    if effective_prompt.is_empty() && !term::stdin_is_tty() {
        // Piped stdin is the prompt: echo "..." | wally llm generate -m qwen3
        effective_prompt = read_piped_prompt();
    }

    if is_vlm {
        return run_vlm(
            options,
            &resolved.model_id,
            &params.image,
            &effective_prompt,
            params,
        );
    }
    if !effective_prompt.is_empty() {
        return if verb == LlmVerb::Generate {
            generate_once(options, &resolved.model_id, &effective_prompt, params)
        } else {
            stream_once(options, &resolved.model_id, &effective_prompt, params)
        };
    }
    // The REPL is interactive by nature; --json promises exactly one JSON
    // document on stdout, which an interactive prompt loop can never keep.
    // `wally run m "" --json` used to fall through into it anyway and exit 0
    // with nothing on stdout.
    if verb == LlmVerb::Chat && !options.json {
        return run_repl(options, &resolved.model_id, params.clone());
    }
    error_line("no prompt given");
    2
}

/// The sampling / reasoning / model flags shared by every llm and vlm
/// command. VLMGenerationRequest.options is the same LLMGenerationOptions the
/// LLM path uses (VLMGenerationOptions was deleted), so llm and vlm expose an
/// identical sampling surface.
fn add_generation_options(cmd: &mut App, model_arg: ModelArg) {
    if model_arg == ModelArg::Option {
        cmd.add_option(
            "--model,-m",
            ValueType::Text,
            "Model to use (downloaded if missing)",
        );
    } else {
        cmd.add_option(
            "model",
            ValueType::Text,
            "Model id, alias, hf.co/... ref or URL",
        )
        .required();
    }
    cmd.add_option("--system-prompt,--system", ValueType::Text, "System prompt");
    cmd.add_option("--lora", ValueType::Text, "LoRA adapter (.gguf) to attach");
    cmd.add_option(
        "--lora-scale",
        ValueType::Float,
        "LoRA strength (default 1.0)",
    )
    .default_val("1.0");
    cmd.add_option(
        "--engine",
        ValueType::Text,
        &format!("Engine to run on ({})", engine_choices()),
    );
    // The sampling knobs and the thinking switches get their own headings in
    // --help, so the page reads as three short lists instead of one of
    // twenty. Group names are printed in first-seen order, so "Options" (the
    // rows above) comes first, then these two.
    const SAMPLING: &str = "Sampling";
    const REASONING: &str = "Reasoning";
    cmd.add_option(
        "--temperature,--temp",
        ValueType::Float,
        "Sampling temperature (0 = engine default)",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--top-p",
        ValueType::Float,
        "Keep the smallest token set above this probability",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--top-k",
        ValueType::Int,
        "Sample from this many highest-probability tokens",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--min-p",
        ValueType::Float,
        "Drop tokens below this share of the top token",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--repetition-penalty",
        ValueType::Float,
        "Penalize tokens already in the context",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--seed",
        ValueType::Int64,
        "Fix the RNG for a repeatable answer",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--frequency-penalty",
        ValueType::Float,
        "Penalize tokens by how often they appeared",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--presence-penalty",
        ValueType::Float,
        "Penalize tokens that appeared at all",
    )
    .group(SAMPLING);
    cmd.add_option(
        "--stop",
        ValueType::Text,
        "Stop at this text (repeat for several)",
    )
    .multi()
    .group(SAMPLING);
    cmd.add_option(
        "--max-output-tokens,--max-tokens",
        ValueType::Int,
        "Cap on generated tokens (default 1024)",
    )
    // Range, not PositiveNumber, for the message alone (mirrors
    // cmd_bench.cpp's --trials): 0 or negative used to reach the engine
    // as-is and read as "no cap" -- full/whole-context output -- instead of
    // the usage error a nonsensical budget should be.
    .check(Validator::Range(1, i32::MAX as i64))
    .default_val("1024")
    .group(SAMPLING);
    cmd.add_option(
        "--reasoning",
        ValueType::Text,
        "Model thinking phase (default on)",
    )
    .check(Validator::IsMember(vec![
        "on".to_string(),
        "off".to_string(),
    ]))
    .default_val("on")
    .group(REASONING);
    cmd.add_flag(
        "--show-thinking,--hide-thinking{false}",
        "Print thinking tokens on stderr (default on)",
    )
    .default_val("true")
    .group(REASONING);
    cmd.add_flag("--no-think", "Same as --reasoning off")
        .group(REASONING);
}

pub fn configure_llm(cmd: &mut App, verb: LlmVerb, model_arg: ModelArg) {
    add_generation_options(cmd, model_arg);
    cmd.add_option(
        "prompt",
        ValueType::Text,
        if verb == LlmVerb::Chat {
            "Prompt to answer (omit for an interactive chat)"
        } else {
            "Prompt to complete (omit to read stdin)"
        },
    );
    if verb == LlmVerb::Chat {
        // The REPL and VLM paths share one implementation; `run --image`
        // stays the documented alias of `vlm generate`.
        cmd.add_option(
            "--image",
            ValueType::Text,
            "Ask about this image instead (vision models)",
        )
        .check(Validator::ExistingFile);
    }
    cmd.callback(move |p, options| {
        let params = RunParams::from_parsed(p, model_arg);
        let prompt = p.get_str("prompt").unwrap_or_default();
        run_llm(options, verb, &prompt, &params)
    });
}

pub fn configure_vlm_generate(cmd: &mut App) {
    add_generation_options(cmd, ModelArg::Option);
    cmd.add_option(
        "prompt",
        ValueType::Text,
        "Question about the image (default: describe it)",
    );
    cmd.add_option("--image,-i", ValueType::Text, "Image to look at")
        .required()
        .check(Validator::ExistingFile);
    cmd.callback(move |p, options| {
        let params = RunParams::from_parsed(p, ModelArg::Option);
        let prompt = p.get_str("prompt").unwrap_or_default();
        run_llm(options, LlmVerb::Generate, &prompt, &params)
    });
}

pub fn register_llm(app: &mut App) {
    let ns = app.add_subcommand("llm", "Generate text with a language model");
    ns.require_subcommand(1, 1);
    configure_llm(
        ns.add_subcommand("generate", "Complete a prompt, printed when done")
            .footer(&examples_footer(&[
                Example::new(
                    "wally llm generate -m qwen3-4b-instruct-2507 \"explain tunnelling\"",
                    "",
                ),
                Example::new(
                    "echo \"summarise this\" | wally llm generate -m qwen3-4b-instruct-2507",
                    "",
                ),
            ])),
        LlmVerb::Generate,
        ModelArg::Option,
    );
    configure_llm(
        ns.add_subcommand("stream", "Complete a prompt, printed as it arrives")
            .footer(&examples_footer(&[Example::new(
                "wally llm stream -m qwen3-4b-instruct-2507 \"tell me a short story\"",
                "",
            )])),
        LlmVerb::Stream,
        ModelArg::Option,
    );
}

pub fn register_vlm(app: &mut App) {
    let ns = app.add_subcommand("vlm", "Ask a vision-language model about an image");
    ns.require_subcommand(1, 1);
    configure_vlm_generate(ns.add_subcommand("generate", "Answer a prompt about an image"));
}

pub fn register_llm_aliases(app: &mut App) {
    // `run` is the interactive model runner (prompt, or a REPL when
    // omitted). `llm generate` / `llm stream` are the explicit, manual entry
    // points. The canonical (origin/main) C++ source registers only `run`
    // here -- no separate `chat` alias.
    configure_llm(
        app.add_subcommand("run", "Run a model")
            .footer(&examples_footer(&[
                Example::new("wally run qwen3-4b-instruct-2507", "Chat interactively"),
                Example::new(
                    "wally run qwen3-4b-instruct-2507 \"write a haiku\"",
                    "Answer one prompt",
                ),
            ])),
        LlmVerb::Chat,
        ModelArg::Positional,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lora_apply_error_with_empty_message_is_failure_not_success() {
        // Present error envelope, empty message, rc == SUCCESS: C++'s
        // `result.has_error()` check fails regardless of message content.
        let result = v1::LoraApplyResult {
            error: Some(v1::SdkError {
                message: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let outcome = lora_apply_outcome(&result, sys::SUCCESS);
        assert_eq!(outcome, Err(sys::SUCCESS.to_string()));
    }

    #[test]
    fn lora_apply_no_error_and_success_rc_is_success() {
        let result = v1::LoraApplyResult::default();
        assert_eq!(lora_apply_outcome(&result, sys::SUCCESS), Ok(()));
    }

    #[test]
    fn lora_apply_error_with_message_reports_it() {
        let result = v1::LoraApplyResult {
            error: Some(v1::SdkError {
                message: "adapter not found".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            lora_apply_outcome(&result, sys::SUCCESS),
            Err("adapter not found".to_string())
        );
    }

    #[test]
    fn generation_error_with_empty_message_is_failure_not_success() {
        // Present error envelope, empty message: without the fix,
        // generate_once fell through and rendered the empty `result.text` as
        // a successful response.
        let result = v1::LlmGenerationResult {
            error: Some(v1::SdkError {
                message: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            generation_outcome(&result),
            Err("unknown error".to_string())
        );
    }

    #[test]
    fn generation_error_with_message_reports_it() {
        let result = v1::LlmGenerationResult {
            error: Some(v1::SdkError {
                message: "context window exceeded".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            generation_outcome(&result),
            Err("context window exceeded".to_string())
        );
    }

    #[test]
    fn generation_no_error_is_success() {
        assert_eq!(
            generation_outcome(&v1::LlmGenerationResult::default()),
            Ok(())
        );
    }

    #[test]
    fn vlm_generation_error_with_empty_message_is_failure_not_success() {
        // Same failure mode as generate_once, on the VLM result envelope:
        // an empty message must not fall through to a 0 exit with empty text.
        let result = v1::VlmResult {
            error: Some(v1::SdkError {
                message: String::new(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            vlm_generation_outcome(&result),
            Err("unknown error".to_string())
        );
    }

    #[test]
    fn vlm_generation_error_with_message_reports_it() {
        let result = v1::VlmResult {
            error: Some(v1::SdkError {
                message: "image decode failed".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            vlm_generation_outcome(&result),
            Err("image decode failed".to_string())
        );
    }

    #[test]
    fn vlm_generation_no_error_is_success() {
        assert_eq!(vlm_generation_outcome(&v1::VlmResult::default()), Ok(()));
    }

    #[test]
    fn set_temperature_trailing_garbage_uses_leading_number() {
        assert_eq!(strtof_prefix("0.7 please"), 0.7);
        assert_eq!(strtof_prefix("256 tokens"), 256.0);
        assert_eq!(strtof_prefix("not a number"), 0.0);
        assert_eq!(strtof_prefix(""), 0.0);
    }

    #[test]
    fn set_max_output_tokens_trailing_garbage_uses_leading_number() {
        assert_eq!(strtol_prefix("256 tokens"), 256);
        assert_eq!(strtol_prefix("-12abc"), -12);
        assert_eq!(strtol_prefix("not a number"), 0);
        assert_eq!(strtol_prefix(""), 0);
    }

    #[test]
    fn piped_prompt_invalid_utf8_is_kept_lossily_not_dropped() {
        // Previously: any invalid UTF-8 byte anywhere made read_to_string
        // fail and leave the destination String untouched (empty), silently
        // dropping the whole prompt instead of forwarding it like C++ does.
        let bytes = b"describe this\xFFplease\n";
        let prompt = decode_piped_prompt(bytes);
        assert_eq!(prompt, "describe this\u{FFFD}please");
        assert!(!prompt.is_empty());
    }

    #[test]
    fn piped_prompt_trims_trailing_newline_and_cr() {
        assert_eq!(decode_piped_prompt(b"hello\r\n"), "hello");
        assert_eq!(decode_piped_prompt(b"hello"), "hello");
    }
}
