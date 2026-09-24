//! Port of src/commands/cmd_telemetry.cpp. Owner: the maintenance/diagnostics port.
//!
//! `wally telemetry emit|blast` — model-free control-plane telemetry.
//!
//! Drives the real commons telemetry pipeline end-to-end: payloads are queued
//! with rac_telemetry_manager_track, batched + serialized by commons
//! (one POST per modality to /api/v2/sdk/telemetry/{modality}), and delivered
//! through the CLI's HTTP callback over the registered curl transport.
//!
//! Development (keyless): no JWT — anonymous POST -> staging backend PUBLIC org.
//! Production: login handshake first (API key -> JWT), then flush.
//! Exits non-zero when any POST fails or any tracked event never reached the
//! backend.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::panic::catch_unwind;

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Validator, ValueType};
use crate::io::output as out;
use crate::net::control_plane as net;
use crate::sys;

/// One of the 12 modalities the V2 telemetry pipeline recognizes (one backend
/// endpoint each), paired with a realistic terminal event type drawn from the
/// canonical names the SDK emits (telemetry_manager.cpp / the backend's
/// normalizer treats *.completed as terminal).
struct ModalitySpec {
    name: &'static str,
    default_event_type: &'static str,
    // Synthetic probe identity — blast/emit always stamp these so quality
    // gates never see null model_id/framework on the control-plane path.
    probe_model_id: &'static str,
    probe_framework: &'static str,
}

const MODALITIES: &[ModalitySpec] = &[
    ModalitySpec {
        name: "llm",
        default_event_type: "llm.generation.completed",
        probe_model_id: "probe-llm-qwen2.5-0.5b",
        probe_framework: "llamacpp",
    },
    ModalitySpec {
        name: "stt",
        default_event_type: "stt.transcription.completed",
        probe_model_id: "probe-stt-whisper-tiny",
        probe_framework: "sherpa",
    },
    ModalitySpec {
        name: "tts",
        default_event_type: "tts.synthesis.completed",
        probe_model_id: "probe-tts-piper",
        probe_framework: "sherpa",
    },
    ModalitySpec {
        name: "vlm",
        default_event_type: "vlm.process.completed",
        probe_model_id: "probe-vlm-llava-1.5",
        probe_framework: "llamacpp",
    },
    ModalitySpec {
        name: "rag",
        default_event_type: "rag.query.completed",
        probe_model_id: "probe-rag-minilm",
        probe_framework: "llamacpp",
    },
    // `neurt` is the ENGINE identity; the framework dimension the SDK stamps
    // for the Apple engine is still `coreml` (RAC_FRAMEWORK_COREML's
    // analytics key).
    ModalitySpec {
        name: "imagegen",
        default_event_type: "imagegen.generate.completed",
        probe_model_id: "probe-imagegen-sd-turbo",
        probe_framework: "coreml",
    },
    ModalitySpec {
        name: "embeddings",
        default_event_type: "embeddings.embed.completed",
        probe_model_id: "probe-embed-minilm",
        probe_framework: "onnx",
    },
    ModalitySpec {
        name: "vad",
        default_event_type: "vad.stopped",
        probe_model_id: "probe-vad-silero",
        probe_framework: "onnx",
    },
    ModalitySpec {
        name: "voice",
        default_event_type: "voice.turn.metrics",
        probe_model_id: "probe-voice-pipeline",
        probe_framework: "llamacpp",
    },
    ModalitySpec {
        name: "lora",
        default_event_type: "lora.attach.completed",
        probe_model_id: "probe-lora-base",
        probe_framework: "llamacpp",
    },
    ModalitySpec {
        name: "model",
        default_event_type: "model.download.completed",
        probe_model_id: "probe-model-qwen2.5-0.5b",
        probe_framework: "llamacpp",
    },
    ModalitySpec {
        name: "system",
        default_event_type: "sdk.init.completed",
        probe_model_id: "probe-sdk-system",
        probe_framework: "llamacpp",
    },
];

fn find_modality(name: &str) -> Option<&'static ModalitySpec> {
    MODALITIES.iter().find(|spec| spec.name == name)
}

fn modality_names() -> Vec<String> {
    MODALITIES
        .iter()
        .map(|spec| spec.name.to_string())
        .collect()
}

/// `CString::new` never fails for the strings this file feeds it in practice
/// (argv elements can't contain a NUL byte; the const probe strings don't
/// either), so any failure silently becomes an empty C string rather than a
/// panic — never unwrap/expect on a value ultimately sourced from argv.
fn to_cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

/// Random version-4 UUID, formatted like C++'s hand-rolled generator
/// (mt19937_64 seeded from random_device). The RNG source differs (OS CSPRNG
/// via the `getrandom` crate, already a direct dependency) but the observable
/// contract — a fresh, unpredictable v4 UUID string — is identical.
fn uuid4() -> String {
    let mut bytes = [0u8; 16];
    // A fill failure (exhausted OS entropy source) is not something the CLI
    // can recover from usefully for a session id; fall back to zeroed bytes
    // rather than panicking.
    let _ = getrandom::fill(&mut bytes);
    let mut hi = u64::from_be_bytes(bytes[0..8].try_into().unwrap_or([0; 8]));
    let mut lo = u64::from_be_bytes(bytes[8..16].try_into().unwrap_or([0; 8]));
    hi = (hi & 0xFFFF_FFFF_FFFF_0FFF) | 0x0000_0000_0000_4000; // version 4
    lo = (lo & 0x3FFF_FFFF_FFFF_FFFF) | 0x8000_0000_0000_0000; // RFC-4122 variant
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        hi >> 32,
        (hi >> 16) & 0xFFFF,
        hi & 0xFFFF,
        lo >> 48,
        lo & 0xFFFF_FFFF_FFFF,
    )
}

// Minimal field extraction from the backend's SDKTelemetryBatchResponse JSON
// ({"success":true,"events_received":N,"events_stored":N,"events_skipped":N,
// "storage_version":"V2"}). The CLI deliberately carries no JSON parser.
/// C's `isspace` in the "C" locale: space, \t, \n, \v (0x0B), \f, \r. id 33:
/// Rust's `u8::is_ascii_whitespace()` deliberately excludes \v (vertical
/// tab), so it is not a drop-in replacement here — a response body with a
/// literal vertical tab between the key's `:` and its value would stop
/// C++'s skip loop one character later than Rust's, shifting where value
/// parsing starts.
fn is_c_isspace(byte: u8) -> bool {
    matches!(byte, b' ' | 0x09 | 0x0A | 0x0B | 0x0C | 0x0D)
}

fn value_offset_after_key(json: &[u8], key: &str) -> Option<usize> {
    let needle = format!("\"{key}\":");
    let needle = needle.as_bytes();
    if needle.len() > json.len() {
        return None;
    }
    let pos = json.windows(needle.len()).position(|w| w == needle)?;
    let mut i = pos + needle.len();
    while i < json.len() && is_c_isspace(json[i]) {
        i += 1;
    }
    Some(i)
}

/// `atoi`-equivalent: optional sign, then digits, stops at the first
/// non-digit; -1 (matching the C++ port's "key not found" sentinel) belongs
/// to the caller, not this helper.
fn atoi(bytes: &[u8]) -> i32 {
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let negative = i < bytes.len() && bytes[i] == b'-';
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut value: i64 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        value = value * 10 + i64::from(bytes[i] - b'0');
        i += 1;
    }
    let signed = if negative { -value } else { value };
    signed.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn extract_int_field(json: &str, key: &str) -> i32 {
    let bytes = json.as_bytes();
    match value_offset_after_key(bytes, key) {
        Some(i) => atoi(&bytes[i..]),
        None => -1,
    }
}

fn extract_bool_field(json: &str, key: &str) -> bool {
    let bytes = json.as_bytes();
    match value_offset_after_key(bytes, key) {
        Some(i) => bytes.get(i..i + 4) == Some(b"true"),
        None => false,
    }
}

/// Per-endpoint accounting accumulated inside the telemetry HTTP callback.
#[derive(Debug, Clone, Default)]
struct EndpointStats {
    posts: i32,
    failures: i32,
    last_status: i32,
    received: i32,
    stored: i32,
    skipped: i32,
    last_error: String,
}

/// key: endpoint path. `BTreeMap` (not `HashMap`) to match C++'s ordered
/// `std::map` iteration in table/JSON rendering.
#[derive(Debug, Clone, Default)]
struct TelemetryHttpContext {
    endpoints: BTreeMap<String, EndpointStats>,
}

/// Accumulates one HTTP exchange's outcome into `stats`, matching C++'s
/// `telemetry_http_callback` accounting. `success`/`events_received`/
/// `events_stored`/`events_skipped` are read from the backend's RESPONSE body
/// (`result.body`), never the outgoing request payload — id 32 was Rust
/// reading the request JSON here instead.
fn record_http_result(stats: &mut EndpointStats, result: &net::HttpResult) {
    stats.posts += 1;
    stats.last_status = result.status;
    if !result.ok() {
        stats.failures += 1;
        stats.last_error = result.describe();
        return;
    }
    if !extract_bool_field(&result.body, "success") {
        stats.failures += 1;
        stats.last_error = format!("backend reported success=false: {}", result.body);
    }
    let received = extract_int_field(&result.body, "events_received");
    let stored = extract_int_field(&result.body, "events_stored");
    let skipped = extract_int_field(&result.body, "events_skipped");
    stats.received += received.max(0);
    stats.stored += stored.max(0);
    stats.skipped += skipped.max(0);
}

/// Registered with the SDK as the telemetry manager's HTTP transport. Plain
/// (not `unsafe`) `extern "C" fn`: it self-checks every pointer before
/// touching it, so it is sound to call with any input the type allows.
/// `catch_unwind`-wrapped because a panic must never unwind across the FFI
/// boundary back into commons.
extern "C" fn telemetry_http_callback(
    user_data: *mut c_void,
    endpoint: *const c_char,
    json_body: *const c_char,
    json_length: usize,
    requires_auth: sys::rac_bool_t,
) {
    let _ = catch_unwind(|| {
        if user_data.is_null() || endpoint.is_null() {
            return;
        }
        // SAFETY: user_data is the `&mut TelemetryHttpContext` pointer that
        // run_telemetry_session registered via
        // rac_telemetry_manager_set_http_callback, and it is only ever
        // invoked synchronously within that same function's call to flush(),
        // before the referenced `report.context` goes out of scope.
        let context = unsafe { &mut *(user_data as *mut TelemetryHttpContext) };
        // SAFETY: checked non-null above; the SDK documents `endpoint` as a
        // NUL-terminated string valid for the duration of this call.
        let endpoint_str = unsafe { CStr::from_ptr(endpoint) }
            .to_string_lossy()
            .into_owned();
        let body = if json_body.is_null() {
            String::new()
        } else {
            // SAFETY: checked non-null; `json_length` bounds a byte slice the
            // SDK guarantees is valid for the call's duration. Uses the
            // explicit length (not NUL-scanning) to match the C++
            // `std::string(json_body, json_length)` construction exactly.
            let slice = unsafe { std::slice::from_raw_parts(json_body as *const u8, json_length) };
            String::from_utf8_lossy(slice).into_owned()
        };

        let result = net::control_plane_post(&endpoint_str, &body, requires_auth == sys::TRUE);

        let stats = context.endpoints.entry(endpoint_str).or_default();
        record_http_result(stats, &result);
    });
}

/// Optional metric flags shared by emit and blast. Negative = unset.
#[derive(Debug, Clone, Copy)]
struct MetricOptions {
    processing_ms: f64,
    input_tokens: i32,
    output_tokens: i32,
    audio_duration_ms: f64,
}

impl Default for MetricOptions {
    fn default() -> Self {
        MetricOptions {
            processing_ms: -1.0,
            input_tokens: -1,
            output_tokens: -1,
            audio_duration_ms: -1.0,
        }
    }
}

fn track_events(
    manager: *mut sys::rac_telemetry_manager_t,
    spec: &ModalitySpec,
    event_type: &str,
    session_id: &str,
    count: i32,
    metrics: MetricOptions,
) {
    for _ in 0..count {
        // CString locals kept alive across the FFI call below —
        // rac_telemetry_payload_t's string fields are raw, borrowed pointers,
        // not owned.
        let c_id = to_cstring(&uuid4());
        let c_event_type = to_cstring(event_type);
        let c_modality = to_cstring(spec.name);
        let c_session_id = to_cstring(session_id);
        let c_model_id = to_cstring(spec.probe_model_id);
        let c_framework = to_cstring(spec.probe_framework);

        // SAFETY: rac_telemetry_payload_default returns a plain-old-data
        // struct by value with every pointer field null/empty; no FFI state
        // to manage beyond the struct itself.
        let mut payload = unsafe { sys::rac_telemetry_payload_default() };
        payload.id = c_id.as_ptr();
        payload.event_type = c_event_type.as_ptr();
        payload.modality = c_modality.as_ptr();
        payload.session_id = c_session_id.as_ptr();
        payload.model_id = c_model_id.as_ptr();
        payload.model_name = c_model_id.as_ptr();
        payload.framework = c_framework.as_ptr();
        // SAFETY: no arguments; reads the SDK's monotonic/wall clock.
        let now_ms = unsafe { sys::rac_get_current_time_ms() };
        payload.timestamp_ms = now_ms;
        payload.created_at_ms = now_ms;
        payload.success = sys::TRUE;
        payload.has_success = sys::TRUE;
        if metrics.processing_ms >= 0.0 {
            payload.processing_time_ms = metrics.processing_ms;
            payload.has_processing_time_ms = sys::TRUE;
        }
        if metrics.input_tokens >= 0 {
            payload.input_tokens = metrics.input_tokens;
        }
        if metrics.output_tokens >= 0 {
            payload.output_tokens = metrics.output_tokens;
            payload.total_tokens = (if metrics.input_tokens > 0 {
                metrics.input_tokens
            } else {
                0
            }) + metrics.output_tokens;
        }
        // Emit only caller-supplied metrics. Do not fabricate TTFT/TPS/context
        // or STT audio-length/RTF/word-count from processing_ms.
        if metrics.audio_duration_ms >= 0.0 {
            payload.audio_duration_ms = metrics.audio_duration_ms;
        }
        // SAFETY: `manager` is the live handle run_telemetry_session created
        // and has not been destroyed; `payload`'s pointer fields (c_id,
        // c_event_type, ...) all outlive this call.
        let _ = unsafe { sys::rac_telemetry_manager_track(manager, &payload) };
        // rac_telemetry_manager_track's result is intentionally ignored —
        // matching the C++ port, which never inspects it. `report.tracked`
        // comes from the caller's own count, not from a per-call success
        // tally.
    }
}

#[derive(Debug, Clone, Default)]
struct FlushReport {
    context: TelemetryHttpContext,
    tracked: i32,
}

/// Login (JWT), create a manager wired to the real transport, run `track_fn`,
/// flush, and account per-endpoint results. Returns false on login/bootstrap
/// failure or if the manager could not be created.
fn run_telemetry_session(
    options: &GlobalOptions,
    report: &mut FlushReport,
    track_fn: impl FnOnce(*mut sys::rac_telemetry_manager_t) -> i32,
) -> bool {
    if bootstrap(options).is_err() {
        return false;
    }

    // Authenticated environments need a JWT before flush. Keyless development
    // posts anonymously to staging backend (PUBLIC org) — skip login.
    // SAFETY: no arguments; reads global SDK state bootstrap() just set up.
    let sdk_env = unsafe { sys::rac_state_get_environment() };
    // SAFETY: reads global SDK state; returns null or a static, NUL-terminated,
    // process-lifetime string owned by commons.
    let api_key_ptr = unsafe { sys::rac_state_get_api_key() };
    // SAFETY: rac_env_auth_expected accepts a null or NUL-terminated pointer
    // for api_key; api_key_ptr is exactly that.
    if unsafe { sys::rac_env_auth_expected(sdk_env, api_key_ptr) } {
        if let Err((_, error)) = net::login() {
            out::error_line(&error);
            return false;
        }
    }

    // SAFETY: reads global SDK state; returns null or a static, NUL-terminated
    // string owned by commons.
    let device_id_ptr = unsafe { sys::rac_state_get_device_id() };
    let device_id = if device_id_ptr.is_null() {
        String::new()
    } else {
        // SAFETY: checked non-null above.
        unsafe { CStr::from_ptr(device_id_ptr) }
            .to_string_lossy()
            .into_owned()
    };

    let c_device_id = to_cstring(&device_id);
    let c_platform = to_cstring(net::platform_name());
    let c_sdk_version = to_cstring(env!("WALLY_VERSION"));
    // SAFETY: `sdk_env` is a plain enum value; the three string pointers are
    // valid, NUL-terminated C strings kept alive for the duration of this
    // call.
    let manager = unsafe {
        sys::rac_telemetry_manager_create(
            sdk_env,
            c_device_id.as_ptr(),
            c_platform.as_ptr(),
            c_sdk_version.as_ptr(),
        )
    };
    if manager.is_null() {
        out::error_line("telemetry manager creation failed");
        return false;
    }

    let c_device_model = to_cstring(net::device_model());
    let c_os_version = to_cstring(net::os_version_string());
    // SAFETY: `manager` is the just-created, non-null handle; the two string
    // pointers are valid, NUL-terminated C strings kept alive for this call.
    unsafe {
        sys::rac_telemetry_manager_set_device_info(
            manager,
            c_device_model.as_ptr(),
            c_os_version.as_ptr(),
        );
    }
    // SAFETY: `manager` is valid. `&mut report.context` is passed as the
    // opaque user_data pointer telemetry_http_callback receives back
    // verbatim on every subsequent call; `report` (owned by this function's
    // caller) outlives every callback invocation because they all happen
    // synchronously below, before the callback is cleared and the manager is
    // destroyed.
    unsafe {
        sys::rac_telemetry_manager_set_http_callback(
            manager,
            Some(telemetry_http_callback),
            (&mut report.context) as *mut TelemetryHttpContext as *mut c_void,
        );
    }

    report.tracked = track_fn(manager);

    // SAFETY: `manager` is valid and owned exclusively by this function; the
    // result is discarded, matching the C++ port (which never checks it
    // either).
    let _ = unsafe { sys::rac_telemetry_manager_flush(manager) };
    // SAFETY: clearing the callback before destroy; `manager` is still valid
    // and no callback invocation can be in flight (this SDK's flush is
    // synchronous).
    unsafe { sys::rac_telemetry_manager_set_http_callback(manager, None, std::ptr::null_mut()) };
    // SAFETY: `manager` was created by rac_telemetry_manager_create above and
    // has not been destroyed yet; this is its one matching destroy call.
    unsafe { sys::rac_telemetry_manager_destroy(manager) };
    true
}

fn total_received(report: &FlushReport) -> i32 {
    report
        .context
        .endpoints
        .values()
        .map(|stats| stats.received)
        .sum()
}

fn report_failed(report: &FlushReport) -> bool {
    if report.context.endpoints.is_empty() {
        return true; // nothing was POSTed — flush deferred or dropped
    }
    if report
        .context
        .endpoints
        .values()
        .any(|stats| stats.failures > 0)
    {
        return true;
    }
    total_received(report) != report.tracked
}

fn render_endpoint_results(options: &GlobalOptions, report: &FlushReport) {
    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_i64("tracked", i64::from(report.tracked))
            .field_bool("success", !report_failed(report));
        json.begin_array("endpoints");
        for (endpoint, stats) in &report.context.endpoints {
            json.begin_array_object()
                .field_str("endpoint", endpoint)
                .field_i64("posts", i64::from(stats.posts))
                .field_i64("http_status", i64::from(stats.last_status))
                .field_i64("events_received", i64::from(stats.received))
                .field_i64("events_stored", i64::from(stats.stored))
                .field_i64("events_skipped", i64::from(stats.skipped));
            if !stats.last_error.is_empty() {
                json.field_str("error", &stats.last_error);
            }
            json.end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return;
    }

    if report.context.endpoints.is_empty() {
        out::error_line("no telemetry batch was sent (flush deferred?)");
        return;
    }
    for (endpoint, stats) in &report.context.endpoints {
        let mut line = format!(
            "{endpoint}  HTTP {}  received={} stored={} skipped={}",
            stats.last_status, stats.received, stats.stored, stats.skipped
        );
        if !stats.last_error.is_empty() {
            line.push_str(&format!("  error: {}", stats.last_error));
        }
        out::result_line(&line);
    }
}

fn run_telemetry_emit(
    options: &GlobalOptions,
    modality: &str,
    event_type: &str,
    count: i32,
    session_id: &str,
    metrics: MetricOptions,
) -> i32 {
    let Some(spec) = find_modality(modality) else {
        out::error_line(&format!("unknown modality '{modality}'"));
        return 2;
    };
    let resolved_event_type = if event_type.is_empty() {
        spec.default_event_type.to_string()
    } else {
        event_type.to_string()
    };
    let resolved_session = if session_id.is_empty() {
        uuid4()
    } else {
        session_id.to_string()
    };

    let mut report = FlushReport::default();
    let session_ok = run_telemetry_session(options, &mut report, |manager| {
        track_events(
            manager,
            spec,
            &resolved_event_type,
            &resolved_session,
            count,
            metrics,
        );
        count
    });
    if !session_ok {
        return 1;
    }

    if !options.json {
        out::status_line(&format!(
            "emitted {count} × {resolved_event_type} (modality {modality}, session {resolved_session})"
        ));
    }
    render_endpoint_results(options, &report);
    if report_failed(&report) {
        1
    } else {
        0
    }
}

struct BlastRow {
    modality: &'static str,
    ok: bool,
    status: String,
    received: i32,
    stored: i32,
    skipped: i32,
}

fn run_telemetry_blast(
    options: &GlobalOptions,
    count: i32,
    session_id: &str,
    metrics: MetricOptions,
) -> i32 {
    let resolved_session = if session_id.is_empty() {
        uuid4()
    } else {
        session_id.to_string()
    };

    let mut report = FlushReport::default();
    let session_ok = run_telemetry_session(options, &mut report, |manager| {
        for spec in MODALITIES {
            track_events(
                manager,
                spec,
                spec.default_event_type,
                &resolved_session,
                count,
                metrics,
            );
        }
        count * MODALITIES.len() as i32
    });
    if !session_ok {
        return 1;
    }

    let mut all_ok = true;
    let mut rows: Vec<BlastRow> = Vec::new();
    for spec in MODALITIES {
        let endpoint = format!("/api/v2/sdk/telemetry/{}", spec.name);
        let mut status = "NO POST".to_string();
        let mut received = 0;
        let mut stored = 0;
        let mut skipped = 0;
        let mut row_ok = false;
        if let Some(stats) = report.context.endpoints.get(&endpoint) {
            received = stats.received;
            stored = stats.stored;
            skipped = stats.skipped;
            row_ok = stats.failures == 0
                && stats.last_status == 200
                && stats.received >= count
                && stats.stored >= count;
            status = if row_ok || stats.last_error.is_empty() {
                format!("HTTP {}", stats.last_status)
            } else {
                stats.last_error.clone()
            };
        }
        all_ok = all_ok && row_ok;
        rows.push(BlastRow {
            modality: spec.name,
            ok: row_ok,
            status,
            received,
            stored,
            skipped,
        });
    }

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_i64("tracked", i64::from(report.tracked))
            .field_bool("success", all_ok)
            .field_str("session_id", &resolved_session);
        json.begin_array("modalities");
        for row in &rows {
            json.begin_array_object()
                .field_str("modality", row.modality)
                .field_bool("ok", row.ok)
                .field_str("status", &row.status)
                .field_i64("events_received", i64::from(row.received))
                .field_i64("events_stored", i64::from(row.stored))
                .field_i64("events_skipped", i64::from(row.skipped))
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
    } else {
        out::status_line(&format!(
            "blast session {resolved_session} — {} event(s) across {} modalities",
            report.tracked,
            MODALITIES.len()
        ));
        let header = [
            "MODALITY", "RESULT", "STATUS", "RECEIVED", "STORED", "SKIPPED",
        ]
        .map(String::from);
        let table_rows: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                vec![
                    row.modality.to_string(),
                    if row.ok {
                        "ok".to_string()
                    } else {
                        "FAILED".to_string()
                    },
                    row.status.clone(),
                    row.received.to_string(),
                    row.stored.to_string(),
                    row.skipped.to_string(),
                ]
            })
            .collect();
        out::table(&header, &table_rows);
    }
    if all_ok {
        0
    } else {
        1
    }
}

pub fn register_telemetry(app: &mut App) {
    let cmd = app.add_subcommand(
        "telemetry",
        "Emit model-free telemetry through the real control-plane pipeline",
    );
    // C++'s single-argument `require_subcommand(1)` sets min == max == 1
    // (CLI11's `App::require_subcommand(int)` overload); the two-argument
    // form here is the faithful translation, not `(1, 0)` ("at least one").
    cmd.require_subcommand(1, 1);

    // ---- telemetry emit ----------------------------------------------------
    let emit_cmd = cmd.add_subcommand(
        "emit",
        "Track N events of one modality, flush to /api/v2/sdk/telemetry/{modality} \
         and report the backend's accounting. Production runs the auth handshake \
         first; development is keyless. Exits non-zero when any POST fails.",
    );
    emit_cmd
        .add_option("--modality", ValueType::Text, "Telemetry modality")
        .required()
        .check(Validator::IsMember(modality_names()));
    emit_cmd.add_option(
        "--event-type",
        ValueType::Text,
        "Event type string (default: the modality's terminal event, e.g. \
         llm.generation.completed)",
    );
    // No ->default_val() in the C++ port either: --count silently defaults to
    // 1 via the bound variable's initializer, so CLI11's help text (and this
    // port's) shows no "[1]" default annotation.
    emit_cmd
        .add_option(
            "--count",
            ValueType::Int,
            "Number of events to emit (default 1)",
        )
        .check(Validator::PositiveNumber);
    emit_cmd.add_option(
        "--session-id",
        ValueType::Text,
        "Session id attached to every event (default: fresh UUID)",
    );
    emit_cmd.add_option(
        "--processing-ms",
        ValueType::Double,
        "processing_time_ms metric for every event",
    );
    emit_cmd.add_option(
        "--input-tokens",
        ValueType::Int,
        "input_tokens metric (llm/vlm modalities)",
    );
    emit_cmd.add_option(
        "--output-tokens",
        ValueType::Int,
        "output_tokens metric (llm/vlm modalities)",
    );
    emit_cmd.add_option(
        "--audio-duration-ms",
        ValueType::Double,
        "audio_duration_ms metric (stt modality)",
    );
    emit_cmd.callback(|parsed, options| {
        let modality = parsed.get_str("--modality").unwrap_or_default();
        let event_type = parsed.get_str("--event-type").unwrap_or_default();
        let count = parsed.get_i64("--count").unwrap_or(1) as i32;
        let session_id = parsed.get_str("--session-id").unwrap_or_default();
        let metrics = MetricOptions {
            processing_ms: parsed.get_f64("--processing-ms").unwrap_or(-1.0),
            input_tokens: parsed.get_i64("--input-tokens").unwrap_or(-1) as i32,
            output_tokens: parsed.get_i64("--output-tokens").unwrap_or(-1) as i32,
            audio_duration_ms: parsed.get_f64("--audio-duration-ms").unwrap_or(-1.0),
        };
        run_telemetry_emit(options, &modality, &event_type, count, &session_id, metrics)
    });

    // ---- telemetry blast ----------------------------------------------------
    let blast_cmd = cmd.add_subcommand(
        "blast",
        "Emit --count events of EVERY modality (all 12) in one run, flush, and \
         print a per-modality result table parsed from the backend's batch \
         responses. Emits one event of every modality.",
    );
    blast_cmd
        .add_option("--count", ValueType::Int, "Events per modality (default 1)")
        .check(Validator::PositiveNumber);
    blast_cmd.add_option(
        "--session-id",
        ValueType::Text,
        "Session id attached to every event (default: fresh UUID)",
    );
    blast_cmd.add_option(
        "--processing-ms",
        ValueType::Double,
        "processing_time_ms metric for every event",
    );
    blast_cmd.add_option(
        "--input-tokens",
        ValueType::Int,
        "input_tokens metric (llm/vlm modalities)",
    );
    blast_cmd.add_option(
        "--output-tokens",
        ValueType::Int,
        "output_tokens metric (llm/vlm modalities)",
    );
    blast_cmd.callback(|parsed, options| {
        let count = parsed.get_i64("--count").unwrap_or(1) as i32;
        let session_id = parsed.get_str("--session-id").unwrap_or_default();
        let metrics = MetricOptions {
            processing_ms: parsed.get_f64("--processing-ms").unwrap_or(-1.0),
            input_tokens: parsed.get_i64("--input-tokens").unwrap_or(-1) as i32,
            output_tokens: parsed.get_i64("--output-tokens").unwrap_or(-1) as i32,
            audio_duration_ms: -1.0,
        };
        run_telemetry_blast(options, count, &session_id, metrics)
    });
}

#[cfg(test)]
mod fix_models_regression_tests {
    use super::*;

    // id 32: success/received/stored/skipped must come from the backend's
    // RESPONSE body, not whatever the request happened to contain. A
    // request body that (adversarially or coincidentally) looks like a
    // failing response must not affect accounting; only `result.body` may.
    #[test]
    fn accounting_reads_the_response_body_not_the_request_payload() {
        let mut stats = EndpointStats::default();
        let result = net::HttpResult {
            transport: sys::SUCCESS,
            status: 200,
            body: r#"{"success":true,"events_received":3,"events_stored":3,"events_skipped":0}"#
                .to_string(),
        };
        record_http_result(&mut stats, &result);
        assert_eq!(stats.posts, 1);
        assert_eq!(stats.failures, 0);
        assert_eq!(stats.received, 3);
        assert_eq!(stats.stored, 3);
        assert_eq!(stats.skipped, 0);
    }

    #[test]
    fn a_response_reporting_failure_is_counted_as_a_failure() {
        let mut stats = EndpointStats::default();
        let result = net::HttpResult {
            transport: sys::SUCCESS,
            status: 200,
            body: r#"{"success":false,"events_received":0}"#.to_string(),
        };
        record_http_result(&mut stats, &result);
        assert_eq!(stats.failures, 1);
        assert!(stats.last_error.contains("backend reported success=false"));
    }

    // id 33: C's isspace() (C locale) treats vertical tab (0x0B) as
    // whitespace; Rust's is_ascii_whitespace() does not. The skip loop must
    // still step past a literal \v between the key's ':' and its value.
    #[test]
    fn value_offset_skips_a_vertical_tab_like_c_isspace() {
        let json = b"{\"success\":\x0btrue}";
        let offset = value_offset_after_key(json, "success").expect("key found");
        assert_eq!(&json[offset..offset + 4], b"true");
    }

    #[test]
    fn value_offset_skips_ordinary_ascii_whitespace_too() {
        let json = b"{\"events_received\": 5}";
        let offset = value_offset_after_key(json, "events_received").expect("key found");
        assert_eq!(&json[offset..offset + 1], b"5");
    }
}
