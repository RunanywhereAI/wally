//! Port of src/commands/cmd_bench.cpp.
//!
//! `wally bench [model]` — auto-benchmark installed models, like the Android
//! app's benchmark screen.
//!
//! With no model argument it enumerates every downloaded, non-built-in model
//! from the registry and benchmarks each in its category (LLM / STT / TTS /
//! VLM). Faithful port of the Android BenchmarkRunner / BenchmarkMetricPolicy
//! flow: per (model, scenario), repeat `trials` times {
//!     unload -> sample avail RAM -> load (timed) -> 1 warmup (discarded)
//!     -> 1 measured pass -> sample avail RAM -> per-trial metrics }
//!   -> aggregate trials by MEDIAN, report [min,max] where useful.
//!
//! Metrics come from commons result protos (TokenUsage + measured phase times).
//! Missing values stay zero — no tok/s, decode_ms, RTF, or chars/s
//! reconstruction. Harness wall clocks cover load / warmup / measured e2e only.
//! No telemetry.

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, Validator, ValueType};
use crate::commands::bench_metrics;
use crate::commands::engine_options;
use crate::io::output as out;
use crate::io::proto::{self, v1};
use crate::sys;

// Prompts / text mirror the Android BenchmarkRunner constants so numbers are
// comparable across the CLI and the app.
const LLM_SYSTEM_PROMPT: &str = "You are a helpful assistant. Always give extremely detailed, thorough responses. Never stop early. Use the full response length available to you. Elaborate on every point with examples and explanations.";
const LLM_PROMPT: &str = "Write a very long and detailed explanation of how neural networks work, covering perceptrons, activation functions, backpropagation, gradient descent, loss functions, convolutional layers, recurrent layers, transformers, attention mechanisms, and training procedures. Be as thorough as possible.";
const VLM_PROMPT: &str = "Describe this image in detail.";
const TTS_SHORT: &str = "Hello, this is a test.";
const TTS_MEDIUM: &str = "The quick brown fox jumps over the lazy dog. Machine learning models can generate speech from text with remarkable quality and natural intonation.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Modality {
    Llm,
    Stt,
    Tts,
    Vlm,
}

fn modality_label(m: Modality) -> &'static str {
    match m {
        Modality::Llm => "llm",
        Modality::Stt => "stt",
        Modality::Tts => "tts",
        Modality::Vlm => "vlm",
    }
}

fn modality_of(category: v1::ModelCategory) -> Option<Modality> {
    use v1::ModelCategory::*;
    match category {
        Language => Some(Modality::Llm),
        SpeechRecognition => Some(Modality::Stt),
        SpeechSynthesis => Some(Modality::Tts),
        Multimodal | Vision => Some(Modality::Vlm),
        // vad, embedding, image-generation are not benchmarked
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
struct Scenario {
    label: &'static str,
    max_tokens: i32,            // LLM/VLM
    seconds: f64,               // STT audio length
    sine: bool,                 // STT: 440 Hz tone vs silence
    text: Option<&'static str>, // TTS input
}

fn scenarios_for(m: Modality) -> &'static [Scenario] {
    const LLM: &[Scenario] = &[
        Scenario {
            label: "Short (50)",
            max_tokens: 50,
            seconds: 0.0,
            sine: false,
            text: None,
        },
        Scenario {
            label: "Medium (256)",
            max_tokens: 256,
            seconds: 0.0,
            sine: false,
            text: None,
        },
        Scenario {
            label: "Long (512)",
            max_tokens: 512,
            seconds: 0.0,
            sine: false,
            text: None,
        },
    ];
    const STT: &[Scenario] = &[
        Scenario {
            label: "Silent 2s",
            max_tokens: 0,
            seconds: 2.0,
            sine: false,
            text: None,
        },
        Scenario {
            label: "Sine Tone 3s",
            max_tokens: 0,
            seconds: 3.0,
            sine: true,
            text: None,
        },
    ];
    const TTS: &[Scenario] = &[
        Scenario {
            label: "Short Text",
            max_tokens: 0,
            seconds: 0.0,
            sine: false,
            text: Some(TTS_SHORT),
        },
        Scenario {
            label: "Medium Text",
            max_tokens: 0,
            seconds: 0.0,
            sine: false,
            text: Some(TTS_MEDIUM),
        },
    ];
    const VLM: &[Scenario] = &[Scenario {
        label: "Image Description",
        max_tokens: 128,
        seconds: 0.0,
        sine: false,
        text: None,
    }];
    match m {
        Modality::Llm => LLM,
        Modality::Stt => STT,
        Modality::Tts => TTS,
        Modality::Vlm => VLM,
    }
}

/// Per-trial metrics; aggregated to medians across trials.
#[derive(Debug, Clone, Copy, Default)]
struct Metrics {
    load_ms: f64,
    warmup_ms: f64,
    end_to_end_ms: f64,
    tokens_per_second: f64, // LLM/VLM
    prompt_eval_ms: f64,    // LLM/VLM prefill
    decode_ms: f64,         // LLM/VLM
    output_tokens: i32,     // LLM/VLM
    real_time_factor: f64,  // STT
    chars_per_second: f64,  // TTS
    audio_duration_ms: f64, // TTS
    memory_delta_bytes: i64,
}

// --- small utilities --------------------------------------------------------

fn available_ram_bytes() -> i64 {
    let Ok(content) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: i64 = rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            return kb * 1024;
        }
    }
    0
}

fn median(values: &[f64]) -> f64 {
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

fn human_bytes(bytes: i64) -> String {
    if bytes <= 0 {
        return "-".to_string();
    }
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.0} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

// 16 kHz, 16-bit mono PCM: silence or a 440 Hz sine at 60% amplitude (matches
// Android SyntheticInput.silentPcm / sinePcm).
fn make_pcm16(seconds: f64, sine: bool) -> Vec<u8> {
    const SAMPLE_RATE: i32 = 16000;
    let n = (SAMPLE_RATE as f64 * seconds) as i32;
    let mut out = Vec::with_capacity((n.max(0) as usize) * 2);
    for i in 0..n.max(0) {
        let v = if sine {
            (2.0 * std::f64::consts::PI * 440.0 * i as f64 / SAMPLE_RATE as f64).sin()
                * 32767.0
                * 0.6
        } else {
            0.0
        };
        out.extend_from_slice(&(v as i16).to_le_bytes());
    }
    out
}

fn now_ms() -> i64 {
    // SAFETY: plain scalar getter, no pointers involved.
    unsafe { sys::rac_monotonic_now_ms() }
}

fn rac_error_message_string(rc: sys::rac_result_t) -> String {
    // SAFETY: rac_error_message always returns a non-null, static string for
    // any rac_result_t value.
    unsafe { std::ffi::CStr::from_ptr(sys::rac_error_message(rc)) }
        .to_string_lossy()
        .into_owned()
}

/// Serialize `request`, call `call` with the bytes and a fresh out-buffer, and
/// parse the result into `Res`. Shared by the four inference-call functions,
/// which all share this exact FFI shape; `load_model_timed` (extra registry
/// handle, distinct "load failed" fallback) and `unload_category` (discards
/// the result) stay hand-written below.
fn call_proto<Req, Res>(
    call: unsafe extern "C" fn(*const u8, usize, *mut sys::rac_proto_buffer_t) -> sys::rac_result_t,
    request: &Req,
) -> Result<Res, String>
where
    Req: prost::Message,
    Res: prost::Message + Default + prost::Name,
{
    let bytes = proto::serialize(request);
    let mut buf = proto::ProtoBuffer::new();
    // SAFETY: `bytes` is a valid, initialised buffer of `bytes.len()` bytes for
    // the duration of this call; `buf.as_mut_ptr()` is a freshly initialised,
    // writable out-parameter the callee owns exclusively until it returns.
    let rc = unsafe { call(bytes.as_ptr(), bytes.len(), buf.as_mut_ptr()) };
    match proto::parse_proto_buffer::<Res>(buf) {
        Ok(result) if rc == sys::SUCCESS => Ok(result),
        Ok(_) => Err(rac_error_message_string(rc)),
        Err(message) => Err(message),
    }
}

// --- lifecycle helpers -------------------------------------------------------

fn unload_category(category: v1::ModelCategory) {
    let request = v1::ModelUnloadRequest {
        category: Some(category as i32),
        ..Default::default()
    };
    let bytes = proto::serialize(&request);
    let mut buf = proto::ProtoBuffer::new();
    // SAFETY: `bytes`/`buf` are valid for the call's duration; the result is
    // intentionally discarded, matching the C++ fire-and-forget unload.
    let _ = unsafe {
        sys::rac_model_lifecycle_unload_proto(bytes.as_ptr(), bytes.len(), buf.as_mut_ptr())
    };
}

fn load_model_timed(
    model_id: &str,
    category: v1::ModelCategory,
    framework: v1::InferenceFramework,
) -> Result<f64, String> {
    let mut request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        category: Some(category as i32),
        validate_availability: true,
        ..Default::default()
    };
    // An explicit --engine is honoured whatever the ref resolved to (catalog
    // entries included); absent the flag this stays UNSPECIFIED and the
    // model's own declared framework is used, exactly as before. Mirrors
    // cmd_run.cpp.
    if framework != v1::InferenceFramework::Unspecified {
        request.framework = Some(framework as i32);
    }
    let bytes = proto::serialize(&request);
    let mut buf = proto::ProtoBuffer::new();
    // SAFETY: rac_get_model_registry returns a handle the SDK keeps valid for
    // the process lifetime; `bytes`/`buf` are valid for the duration of the
    // call below, same as call_proto above.
    let (rc, t0, t1) = unsafe {
        let registry = sys::rac_get_model_registry();
        let t0 = sys::rac_monotonic_now_ms();
        let rc = sys::rac_model_lifecycle_load_proto(
            registry,
            bytes.as_ptr(),
            bytes.len(),
            buf.as_mut_ptr(),
        );
        let t1 = sys::rac_monotonic_now_ms();
        (rc, t0, t1)
    };
    // NOTE: the fallback here is the literal "load failed", not
    // rac_error_message(rc) — distinct from the four inference-call functions
    // below, which is how the C++ was written.
    let result: v1::ModelLoadResult = match proto::parse_proto_buffer(buf) {
        Ok(result) if rc == sys::SUCCESS => result,
        Ok(_) => return Err("load failed".to_string()),
        Err(message) => {
            return Err(if message.is_empty() {
                "load failed".to_string()
            } else {
                message
            });
        }
    };
    if let Some(error) = result.error.as_ref() {
        return Err(if error.message.is_empty() {
            "load failed".to_string()
        } else {
            error.message.clone()
        });
    }
    Ok((t1 - t0) as f64)
}

// --- per-modality inference calls -------------------------------------------

fn llm_generate(max_tokens: i32, system_prompt: bool) -> Result<v1::LlmGenerationResult, String> {
    let request = v1::LlmGenerateRequest {
        messages: vec![v1::ChatMessage {
            role: v1::MessageRole::User as i32,
            content: LLM_PROMPT.to_string(),
            ..Default::default()
        }],
        options: Some(v1::LlmGenerationOptions {
            max_output_tokens: Some(max_tokens),
            temperature: Some(0.0),
            system_prompt: system_prompt.then(|| LLM_SYSTEM_PROMPT.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    call_proto(sys::rac_llm_generate_proto, &request)
}

fn stt_transcribe(pcm: &[u8]) -> Result<v1::SttOutput, String> {
    let request = v1::SttTranscriptionRequest {
        audio: Some(v1::SttAudioSource {
            encoding: v1::AudioEncoding::PcmS16Le as i32,
            sample_rate: 16000,
            channels: 1,
            // bits_per_sample deleted: sample width is determined by
            // `encoding` (AUDIO_ENCODING_PCM_S16_LE above already says 16-bit).
            source: Some(v1::stt_audio_source::Source::AudioData(pcm.to_vec())),
            ..Default::default()
        }),
        options: Some(v1::SttOptions {
            language: Some("en".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    call_proto(sys::rac_stt_transcribe_lifecycle_proto, &request)
}

fn tts_synthesize(text: &str) -> Result<v1::TtsOutput, String> {
    let request = v1::TtsSynthesisRequest {
        text: text.to_string(),
        options: Some(v1::TtsOptions {
            sample_rate: 22050,
            ..Default::default()
        }),
        ..Default::default()
    };
    call_proto(sys::rac_tts_synthesize_lifecycle_proto, &request)
}

fn vlm_process(image_path: &str, max_tokens: i32) -> Result<v1::VlmResult, String> {
    let request = v1::VlmGenerationRequest {
        images: vec![v1::VlmImage {
            source: Some(v1::vlm_image::Source::FilePath(image_path.to_string())),
            ..Default::default()
        }],
        prompt: VLM_PROMPT.to_string(),
        options: Some(v1::LlmGenerationOptions {
            max_output_tokens: Some(max_tokens),
            temperature: Some(0.0),
            ..Default::default()
        }),
        ..Default::default()
    };
    call_proto(sys::rac_vlm_generate_proto, &request)
}

// --- per-trial runners (one load -> warmup -> measured pass) ---------------

#[derive(Debug, Clone)]
struct TrialCtx {
    model_id: String,
    category: v1::ModelCategory,
    scenario: Scenario,
    vlm_image: String,
    framework: v1::InferenceFramework,
}

fn llm_trial(c: &TrialCtx, m: &mut Metrics) -> Result<(), String> {
    unload_category(c.category);
    let mem_before = available_ram_bytes();
    m.load_ms = load_model_timed(&c.model_id, c.category, c.framework)?;

    let w0 = now_ms();
    llm_generate(5, false).inspect_err(|_| {
        unload_category(c.category);
    })?;
    m.warmup_ms = (now_ms() - w0) as f64;

    let t0 = now_ms();
    let r = llm_generate(c.scenario.max_tokens, true).inspect_err(|_| {
        unload_category(c.category);
    })?;
    let measured_e2e = (now_ms() - t0) as f64;
    m.memory_delta_bytes = mem_before - available_ram_bytes();
    unload_category(c.category);

    let filled =
        bench_metrics::fill_llm(&r, measured_e2e).ok_or_else(|| "no output tokens".to_string())?;
    m.end_to_end_ms = filled.end_to_end_ms;
    m.tokens_per_second = filled.tokens_per_second;
    m.decode_ms = filled.decode_ms;
    m.prompt_eval_ms = filled.prompt_eval_ms;
    m.output_tokens = filled.output_tokens;
    Ok(())
}

fn stt_trial(c: &TrialCtx, m: &mut Metrics) -> Result<(), String> {
    unload_category(c.category);
    let mem_before = available_ram_bytes();
    m.load_ms = load_model_timed(&c.model_id, c.category, c.framework)?;

    let _ = stt_transcribe(&make_pcm16(0.5, false)); // warmup, errors ignored

    let t0 = now_ms();
    // The transcript text itself isn't reported (RTF is commons-owned, not
    // derived here) -- only that transcription succeeded. An empty
    // transcript is a valid, successful result for the `Silent 2s` scenario
    // on any engine that correctly suppresses silence, so it must not be
    // rejected as a failed trial; it keeps the timing/memory metrics below.
    stt_transcribe(&make_pcm16(c.scenario.seconds, c.scenario.sine)).inspect_err(|_| {
        unload_category(c.category);
    })?;
    m.end_to_end_ms = (now_ms() - t0) as f64;
    m.memory_delta_bytes = mem_before - available_ram_bytes();
    unload_category(c.category);

    // RTF is commons-owned; do not derive from wall / scenario / duration.
    m.real_time_factor = 0.0;
    Ok(())
}

fn tts_trial(c: &TrialCtx, m: &mut Metrics) -> Result<(), String> {
    unload_category(c.category);
    let mem_before = available_ram_bytes();
    m.load_ms = load_model_timed(&c.model_id, c.category, c.framework)?;

    let _ = tts_synthesize("Hi."); // warmup, errors ignored

    let text = c.scenario.text.unwrap_or("");
    let t0 = now_ms();
    let r = tts_synthesize(text).inspect_err(|_| {
        unload_category(c.category);
    })?;
    m.end_to_end_ms = (now_ms() - t0) as f64;
    m.memory_delta_bytes = mem_before - available_ram_bytes();
    unload_category(c.category);

    m.audio_duration_ms = r.duration_ms as f64;
    // chars/s is commons-owned; do not derive from wall / input bytes.
    m.chars_per_second = 0.0;
    Ok(())
}

/// Categories to clear before loading a VLM trial's model: its own category
/// (Multimodal or Vision) plus Language, which a multimodal model may also
/// occupy. Pulled out as a pure function so the pairing is testable without
/// the unload FFI call itself.
fn vlm_preload_unload_targets(category: v1::ModelCategory) -> [v1::ModelCategory; 2] {
    [category, v1::ModelCategory::Language]
}

fn vlm_trial(c: &TrialCtx, m: &mut Metrics) -> Result<(), String> {
    for category in vlm_preload_unload_targets(c.category) {
        unload_category(category);
    }
    let mem_before = available_ram_bytes();
    m.load_ms = load_model_timed(&c.model_id, c.category, c.framework)?;

    let _ = vlm_process(&c.vlm_image, 1); // warmup, errors ignored

    let t0 = now_ms();
    let r = vlm_process(&c.vlm_image, c.scenario.max_tokens).inspect_err(|_| {
        unload_category(c.category);
    })?;
    let measured_e2e = (now_ms() - t0) as f64;
    m.memory_delta_bytes = mem_before - available_ram_bytes();
    unload_category(c.category);

    let filled =
        bench_metrics::fill_vlm(&r, measured_e2e).ok_or_else(|| "no output tokens".to_string())?;
    m.end_to_end_ms = filled.end_to_end_ms;
    m.tokens_per_second = filled.tokens_per_second;
    m.decode_ms = filled.decode_ms;
    m.prompt_eval_ms = filled.prompt_eval_ms;
    m.output_tokens = filled.output_tokens;
    Ok(())
}

// --- aggregation + report ----------------------------------------------------

struct BenchRow {
    model_id: String,
    modality: Modality,
    scenario: String,
    success: bool,
    error: String,
    trials: i32,
    med: Metrics,
}

// `wally bench` reports per-row success/error but, until this, always
// returned 0 -- a CI step piping bench into a pass/fail gate saw every run as
// green even when every row failed. Non-zero iff at least one row failed.
fn bench_exit_code(rows: &[BenchRow]) -> i32 {
    if rows.iter().any(|r| !r.success) {
        1
    } else {
        0
    }
}

type TrialFn = fn(&TrialCtx, &mut Metrics) -> Result<(), String>;

fn aggregate(
    options: &GlobalOptions,
    ctx: &TrialCtx,
    modality: Modality,
    trials: i32,
    trial: TrialFn,
) -> BenchRow {
    let mut row = BenchRow {
        model_id: ctx.model_id.clone(),
        modality,
        scenario: ctx.scenario.label.to_string(),
        success: false,
        error: String::new(),
        trials,
        med: Metrics::default(),
    };

    let mut load = Vec::new();
    let mut warmup = Vec::new();
    let mut e2e = Vec::new();
    let mut tps = Vec::new();
    let mut prefill = Vec::new();
    let mut decode = Vec::new();
    let mut mem: Vec<f64> = Vec::new();
    let mut rtf = Vec::new();
    let mut cps = Vec::new();
    let mut adur = Vec::new();
    let mut out_tok: Vec<i32> = Vec::new();

    for t in 0..trials {
        let mut m = Metrics::default();
        if let Err(err) = trial(ctx, &mut m) {
            row.error = err;
            return row;
        }
        load.push(m.load_ms);
        warmup.push(m.warmup_ms);
        e2e.push(m.end_to_end_ms);
        tps.push(m.tokens_per_second);
        prefill.push(m.prompt_eval_ms);
        decode.push(m.decode_ms);
        mem.push(m.memory_delta_bytes as f64);
        rtf.push(m.real_time_factor);
        cps.push(m.chars_per_second);
        adur.push(m.audio_duration_ms);
        out_tok.push(m.output_tokens);
        if options.verbose {
            out::status_line(&format!("  trial {}/{} ok", t + 1, trials));
        }
    }

    row.success = true;
    row.med.load_ms = median(&load);
    row.med.warmup_ms = median(&warmup);
    row.med.end_to_end_ms = median(&e2e);
    row.med.tokens_per_second = median(&tps);
    row.med.prompt_eval_ms = median(&prefill);
    row.med.decode_ms = median(&decode);
    row.med.memory_delta_bytes = median(&mem) as i64;
    row.med.real_time_factor = median(&rtf);
    row.med.chars_per_second = median(&cps);
    row.med.audio_duration_ms = median(&adur);
    // NOTE: unlike every other field, output_tokens is NOT a median — it's a
    // raw middle-index pick on the UNSORTED per-trial vector, matching the
    // C++ exactly.
    row.med.output_tokens = if out_tok.is_empty() {
        0
    } else {
        out_tok[out_tok.len() / 2]
    };
    row
}

/// Left-justifies/truncates `s` to exactly `width` BYTES, matching C's
/// `%-N.Ns` (snprintf) field semantics: it counts and cuts raw bytes, not
/// Unicode scalar values, so a multi-byte UTF-8 character straddling the
/// width boundary is silently cut in half — the resulting bytes can be
/// invalid UTF-8, same as C++'s `%.30s` on a byte string. Rust's
/// `std::fmt` width/precision count `char`s, so this must not go through
/// `format!("{:<N.N}", ...)`.
fn ljust_bytes(s: &str, width: usize) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = if bytes.len() > width {
        bytes[..width].to_vec()
    } else {
        bytes.to_vec()
    };
    if out.len() < width {
        out.resize(width, b' ');
    }
    out
}

/// Writes one bench table row to stdout. The row may contain invalid UTF-8
/// (see `ljust_bytes`), so this bypasses `out::result_line` (which requires
/// a `&str`) and writes the raw bytes directly, exactly as C++'s
/// `std::printf("%s", line)` on a `char[256]` buffer would.
fn write_bench_row(line: &[u8]) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(line);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

// Modality-specific "primary" throughput/latency string for the report.
fn primary_metric(r: &BenchRow) -> String {
    match r.modality {
        Modality::Llm | Modality::Vlm => {
            format!(
                "{:.1} tok/s  {:.0}ms pf",
                r.med.tokens_per_second, r.med.prompt_eval_ms
            )
        }
        Modality::Stt => format!(
            "RTF {:.3} ({:.0}x rt)",
            r.med.real_time_factor,
            if r.med.real_time_factor > 0.0 {
                1.0 / r.med.real_time_factor
            } else {
                0.0
            }
        ),
        Modality::Tts => format!("{:.0} chars/s", r.med.chars_per_second),
    }
}

// --- enumeration + driver ----------------------------------------------------

struct BenchModel {
    id: String,
    category: v1::ModelCategory,
    modality: Modality,
}

fn collect_models(only_model: &str) -> Result<Vec<BenchModel>, String> {
    let mut buf = proto::ProtoBuffer::new();
    // SAFETY: rac_get_model_registry returns a process-lifetime handle; `buf`
    // is a freshly initialised, writable out-parameter for the call's duration.
    let rc = unsafe {
        sys::rac_model_registry_list_downloaded_proto_buffer(
            sys::rac_get_model_registry(),
            buf.as_mut_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        return Err("failed to list downloaded models".to_string());
    }
    let list: v1::ModelInfoList = proto::parse_proto_buffer(buf)?;

    let mut models = Vec::new();
    for m in list.models {
        if !only_model.is_empty() && m.id != only_model {
            continue;
        }
        let framework = v1::InferenceFramework::try_from(m.framework)
            .unwrap_or(v1::InferenceFramework::Unspecified);
        if framework == v1::InferenceFramework::FoundationModels
            || framework == v1::InferenceFramework::SystemTts
        {
            continue; // builtin
        }
        let Ok(category) = v1::ModelCategory::try_from(m.category) else {
            continue;
        };
        let Some(modality) = modality_of(category) else {
            continue;
        };
        models.push(BenchModel {
            id: m.id,
            category,
            modality,
        });
    }
    Ok(models)
}

/// Message for a VLM row skipped because no usable `--vlm-image` was given.
/// wally ships no built-in sample, so an empty `path` (the flag was never
/// passed) and a non-empty one that doesn't exist on disk get distinct
/// wording rather than both claiming a nonexistent in-tree default.
/// Message for a resolved model that isn't in the downloaded registry.
/// Names the fix with the ref the caller actually typed (`model_ref_arg`),
/// which `wally models pull` accepts directly, rather than only reporting
/// the resolved registry id (`only_model`) as unrecognized.
fn model_not_downloaded_error(only_model: &str, model_ref_arg: &str) -> String {
    format!("model '{only_model}' is not downloaded; pull it first with `wally models pull {model_ref_arg}`")
}

fn vlm_image_missing_error(path: &str) -> String {
    if path.is_empty() {
        "wally ships no built-in VLM sample image; pass --vlm-image <path>".to_string()
    } else {
        format!("VLM sample image not found: '{path}' (pass --vlm-image <path> pointing at a real file)")
    }
}

pub fn run_bench(
    options: &GlobalOptions,
    model_ref_arg: &str,
    trials: i32,
    vlm_image: &str,
    engine: &str,
) -> i32 {
    if bootstrap(options).is_err() {
        return 1;
    }
    // `--trials,-n` is Validator::Range(1, i32::MAX)-checked, so this is
    // always >= 1 by the time it gets here; a caller running `-n -5` gets the
    // parser's own rejection before a model ever loads, not a silently
    // clamped run.

    // Parsed once, up front: an explicit --engine both narrows ref resolution
    // and pins the framework every trial loads with, whether one model was
    // named or the whole registry is being benchmarked.
    let engine_hint = match engine_options::resolve_engine_hint(engine) {
        Ok(hint) => hint,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };

    // Resolve the argument the same way every other command does, so a local
    // bundle directory, an HF ref or a URL all work here too. collect_models
    // only ever scans the registry, so without this an unregistered ref --
    // which is what a freshly staged bundle on disk is -- reported "not a
    // downloaded benchmarkable model" even though `wally run` could load it
    // fine.
    let mut only_model = model_ref_arg.to_string();
    if !model_ref_arg.is_empty() {
        match model_ref::resolve(model_ref_arg, Some(&engine_hint.resolve_options)) {
            Ok(resolved) => only_model = resolved.model_id,
            Err((_, message)) => {
                out::error_line(&message);
                return 1;
            }
        }
    }

    let models = match collect_models(&only_model) {
        Ok(models) => models,
        Err(error) => {
            out::error_line(&error);
            return 1;
        }
    };
    if models.is_empty() {
        // collect_models only ever scans already-downloaded registry
        // entries, so a resolved-but-not-yet-pulled local/HF/URL ref lands
        // here too. wally does not auto-pull it for a benchmark run; name
        // the fix instead of just reporting the ref as unrecognized.
        let message = if only_model.is_empty() {
            "no downloaded models to benchmark (pull one with `wally models pull`)".to_string()
        } else {
            model_not_downloaded_error(&only_model, model_ref_arg)
        };
        out::error_line(&message);
        return 1;
    }

    // wally ships no built-in VLM sample image (there is no default path
    // that resolves inside an installed binary), so --vlm-image is required
    // to benchmark VLM models. Checked once, outside the loop: the
    // llama.cpp load failure it otherwise causes reports "Input is invalid"
    // with the real cause buried in the engine's own stderr lines above it.
    let vlm_image_exists = !vlm_image.is_empty() && std::path::Path::new(vlm_image).exists();

    let mut rows: Vec<BenchRow> = Vec::new();
    for model in &models {
        for scenario in scenarios_for(model.modality) {
            if model.modality == Modality::Vlm && !vlm_image_exists {
                let error = vlm_image_missing_error(vlm_image);
                out::status_line(&format!(
                    "skipping {} {} — {}: {error}",
                    modality_label(model.modality),
                    model.id,
                    scenario.label
                ));
                rows.push(BenchRow {
                    model_id: model.id.clone(),
                    modality: model.modality,
                    scenario: scenario.label.to_string(),
                    success: false,
                    error,
                    trials,
                    med: Metrics::default(),
                });
                continue;
            }
            out::status_line(&format!(
                "benchmarking {} {} — {} ({trials} trials)",
                modality_label(model.modality),
                model.id,
                scenario.label
            ));
            let ctx = TrialCtx {
                model_id: model.id.clone(),
                category: model.category,
                scenario: *scenario,
                vlm_image: vlm_image.to_string(),
                framework: engine_hint.framework,
            };
            let trial_fn: TrialFn = match model.modality {
                Modality::Llm => llm_trial,
                Modality::Stt => stt_trial,
                Modality::Tts => tts_trial,
                Modality::Vlm => vlm_trial,
            };
            rows.push(aggregate(options, &ctx, model.modality, trials, trial_fn));
        }
    }

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object().begin_array("results");
        for r in &rows {
            json.begin_array_object()
                .field_str("model", &r.model_id)
                .field_str("modality", modality_label(r.modality))
                .field_str("scenario", &r.scenario)
                .field_bool("success", r.success)
                .field_i64("trials", r.trials as i64);
            if r.success {
                json.field_f64("tokens_per_second", r.med.tokens_per_second)
                    .field_f64("prompt_eval_ms", r.med.prompt_eval_ms)
                    .field_f64("decode_ms", r.med.decode_ms)
                    .field_f64("end_to_end_ms", r.med.end_to_end_ms)
                    .field_f64("real_time_factor", r.med.real_time_factor)
                    .field_f64("chars_per_second", r.med.chars_per_second)
                    .field_i64("output_tokens", r.med.output_tokens as i64)
                    .field_f64("load_ms", r.med.load_ms)
                    .field_i64("memory_delta_bytes", r.med.memory_delta_bytes);
            } else {
                json.field_str("error", &r.error);
            }
            json.end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return bench_exit_code(&rows);
    }

    out::result_line("");
    out::result_line(
        "MODEL                          MOD  SCENARIO         PRIMARY                 LOAD     MEMΔ",
    );
    for r in &rows {
        if r.success {
            let mut line = ljust_bytes(&r.model_id, 30);
            line.push(b' ');
            line.extend(ljust_bytes(modality_label(r.modality), 4));
            line.push(b' ');
            line.extend(ljust_bytes(&r.scenario, 15));
            line.push(b' ');
            line.extend(ljust_bytes(&primary_metric(r), 22));
            line.extend(
                format!(
                    " {:6.0}ms  {}",
                    r.med.load_ms,
                    human_bytes(r.med.memory_delta_bytes)
                )
                .into_bytes(),
            );
            write_bench_row(&line);
        } else {
            let mut line = ljust_bytes(&r.model_id, 30);
            line.push(b' ');
            line.extend(ljust_bytes(modality_label(r.modality), 4));
            line.push(b' ');
            line.extend(ljust_bytes(&r.scenario, 15));
            line.extend(format!(" FAILED: {}", r.error).into_bytes());
            write_bench_row(&line);
        }
    }
    bench_exit_code(&rows)
}

pub fn register_bench(app: &mut App) {
    let cmd = app.add_subcommand(
        "bench",
        "Measure throughput and load time of downloaded models",
    );
    cmd.add_option(
        "model",
        ValueType::Text,
        "Model id, local bundle path, hf.co/... or URL; must already be \
         downloaded (`wally models pull <ref>` first) -- default: all downloaded",
    );
    // engine_choices() is computed eagerly, at registration time, matching the
    // C++ (`std::string("Engine hint (") + engine_choices() + ")"` was itself
    // an eager call, not deferred to the callback).
    cmd.add_option(
        "--engine",
        ValueType::Text,
        &format!("Engine hint ({})", engine_options::engine_choices()),
    );
    cmd.add_option(
        "--trials,-n",
        ValueType::Int,
        "Measured trials per scenario (median reported)",
    )
    .default_val("3")
    // Range, not PositiveNumber, for the message alone. PositiveNumber
    // renders its bounds as doubles, so `-n -5` was rejected with "not in
    // range [2.22507e-308 - 1.79769e+308]". The accepted set is unchanged:
    // trials is an int, so anything above INT_MAX already failed to parse.
    .check(Validator::Range(1, i32::MAX as i64));
    // No `default_val` here: wally ships no built-in VLM sample image, so a
    // default that looked like a real path (previously a wally-source-tree
    // path that doesn't exist in an installed binary) only hid that the flag
    // is required. Its absence is now explicit -- see vlm_image_missing_error.
    cmd.add_option(
        "--vlm-image",
        ValueType::Text,
        "Image file for VLM benchmarking (required to benchmark VLM models)",
    );
    cmd.callback(|parsed, options| {
        let model = parsed.get_str("model").unwrap_or_default();
        let trials = parsed.get_i64("--trials").unwrap_or(3) as i32;
        let vlm_image = parsed.get_str("--vlm-image").unwrap_or_default();
        let engine = parsed.get_str("--engine").unwrap_or_default();
        run_bench(options, &model, trials, &vlm_image, &engine)
    });
}

#[cfg(test)]
mod ljust_bytes_tests {
    use super::ljust_bytes;

    // C's "%-N.Ns" (and Rust's old `format!("{:<N.N}", ...)`) differ
    // on multi-byte UTF-8 — C counts/cuts bytes, Rust's std::fmt counts/cuts
    // chars. ljust_bytes must match the C (byte) semantics.
    #[test]
    fn pads_short_ascii_strings_with_spaces_to_byte_width() {
        assert_eq!(ljust_bytes("abc", 5), b"abc  ".to_vec());
    }

    #[test]
    fn truncates_ascii_strings_longer_than_width() {
        assert_eq!(ljust_bytes("abcdefgh", 5), b"abcde".to_vec());
    }

    #[test]
    fn pads_by_byte_count_not_char_count_for_multibyte_utf8() {
        // "café" is 4 chars but 5 bytes (é is 2 bytes); snprintf("%-10.10s")
        // pads to 10 BYTES (5 spaces after the 5-byte string), whereas
        // Rust's format!("{:<10.10}", "café") would pad to 10 CHARS (6
        // spaces), one column wider than C++.
        let padded = ljust_bytes("café", 10);
        assert_eq!(padded.len(), 10);
        assert_eq!(&padded[..5], "café".as_bytes());
        assert_eq!(&padded[5..], b"     ");
    }

    #[test]
    fn truncation_can_cut_a_multibyte_character_in_half() {
        // "é" alone is the 2-byte UTF-8 sequence 0xC3 0xA9. Truncating "café"
        // (bytes: c a f 0xC3 0xA9) to 4 bytes keeps only the first byte of
        // "é", producing invalid UTF-8 — exactly what C's %.4s does on the
        // same byte string.
        let truncated = ljust_bytes("café", 4);
        assert_eq!(truncated, vec![b'c', b'a', b'f', 0xC3]);
        assert!(std::str::from_utf8(&truncated).is_err());
    }
}

#[cfg(test)]
mod vlm_preload_unload_targets_tests {
    use super::vlm_preload_unload_targets;
    use crate::io::proto::v1::ModelCategory;

    #[test]
    fn unloads_vision_when_trial_category_is_vision() {
        // Before the fix this hardcoded Multimodal, so a Vision-categorized
        // model's own slot was never cleared before reloading into it.
        let targets = vlm_preload_unload_targets(ModelCategory::Vision);
        assert!(targets.contains(&ModelCategory::Vision));
    }

    #[test]
    fn still_unloads_multimodal_when_trial_category_is_multimodal() {
        let targets = vlm_preload_unload_targets(ModelCategory::Multimodal);
        assert!(targets.contains(&ModelCategory::Multimodal));
    }

    #[test]
    fn always_unloads_language_too() {
        for category in [ModelCategory::Vision, ModelCategory::Multimodal] {
            assert!(vlm_preload_unload_targets(category).contains(&ModelCategory::Language));
        }
    }
}

#[cfg(test)]
mod model_not_downloaded_error_tests {
    use super::model_not_downloaded_error;

    #[test]
    fn names_wally_models_pull_with_the_ref_the_caller_typed() {
        // The caller's own ref (an hf.co/URL/local-path form) is what
        // `wally models pull` accepts, not necessarily the resolved
        // registry id, so the message must echo the former.
        let message = model_not_downloaded_error("resolved-id", "hf.co/org/repo");
        assert!(message.contains("wally models pull hf.co/org/repo"));
        assert!(message.contains("resolved-id"));
    }
}

#[cfg(test)]
mod vlm_image_missing_error_tests {
    use super::vlm_image_missing_error;

    #[test]
    fn does_not_claim_an_in_tree_default_resolves() {
        // Regression: the old message said "the built-in default only
        // resolves from inside the wally source tree", implying a fallback
        // that never actually existed for an installed binary.
        for path in [
            "",
            "docs/gifs/npu-model-tag-screenshot.png",
            "/no/such/file.png",
        ] {
            let message = vlm_image_missing_error(path);
            assert!(
                !message.contains("resolves from inside the wally source tree"),
                "message still implies an in-tree default: {message}"
            );
        }
    }

    #[test]
    fn says_no_built_in_sample_ships_when_flag_is_absent() {
        assert!(vlm_image_missing_error("").contains("ships no built-in"));
    }
}

#[cfg(test)]
mod bench_exit_code_tests {
    use super::{bench_exit_code, BenchRow, Metrics, Modality};

    fn row(success: bool) -> BenchRow {
        BenchRow {
            model_id: "m".to_string(),
            modality: Modality::Llm,
            scenario: "s".to_string(),
            success,
            error: String::new(),
            trials: 1,
            med: Metrics::default(),
        }
    }

    #[test]
    fn nonzero_when_any_row_failed() {
        let rows = vec![row(true), row(false)];
        assert_eq!(bench_exit_code(&rows), 1);
    }

    #[test]
    fn zero_when_every_row_succeeded() {
        let rows = vec![row(true), row(true)];
        assert_eq!(bench_exit_code(&rows), 0);
    }
}
