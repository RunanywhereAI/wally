//! `wally diarize <audio.wav> --model <id-or-path>` — offline speaker
//! diarization via the commons diarization service (audio-in → typed
//! speaker segments out).
//!
//! Audio loading mirrors cmd_stt (16-bit PCM WAV → mono 16 kHz float). The
//! model lifecycle is the standard service handle sequence the C ABI exposes:
//!   rac_diarization_create(model_id)  → route to the ONNX Sortformer provider
//!     → rac_diarization_initialize(model_path)  → load the ONNX graph
//!     → rac_diarization_diarize(samples, …, &result)  → typed segments
//!     → rac_diarization_result_free / rac_diarization_cleanup / rac_diarization_destroy
//! A `--model` naming an on-disk path is used verbatim (the provider resolves
//! the .onnx inside a directory); otherwise it is treated as a catalog id and
//! pulled with the shared ensure-downloaded flow.
//!
//! Port of src/commands/cmd_diarize.cpp.

use std::ffi::{c_char, CStr, CString};

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Parsed, Validator, ValueType};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{self as out, JsonWriter};
use crate::io::wav_io as wav;
use crate::sys;

const DIARIZATION_SAMPLE_RATE: i32 = 16000;

/// Literal mirror of `RAC_DIARIZATION_OPTIONS_DEFAULT` from
/// rac_diarization_types.h: header-only `static const` (internal C linkage
/// per translation unit), so there is no symbol to link against from Rust.
fn default_diarization_options() -> sys::rac_diarization_options_t {
    sys::rac_diarization_options_t {
        sample_rate_hz: 16000,  // header default: 16000 (never overridden by this CLI)
        channel_count: 1,       // header default: 1 (never overridden by this CLI)
        threshold: 0.5,         // header default: 0.5
        minimum_duration_ms: 0, // header default: 0
        merge_gap_ms: 0,        // header default: 0
    }
}

// SDK strings must not embed a NUL; sanitize defensively instead of
// panicking on file/model-derived input.
fn to_cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn ptr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: non-null pointers from the SDK's result structs are
    // NUL-terminated strings owned by the result until it is freed.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn print_result(options: &GlobalOptions, model_ref: &str, result: &sys::rac_diarization_result_t) {
    // SAFETY: segments/segment_count come straight from a result just
    // populated by a successful diarize call and are valid until
    // rac_diarization_result_free.
    let segments: &[sys::rac_diarization_segment_t] = if result.segments.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(result.segments, result.segment_count) }
    };

    if options.json {
        let model_id = if result.model_id.is_null() {
            model_ref.to_string()
        } else {
            ptr_to_string(result.model_id)
        };
        let mut json = JsonWriter::new();
        json.begin_object()
            .field_str("model", &model_id)
            .field_i64("speaker_count", result.speaker_count as i64)
            .field_i64("segment_count", result.segment_count as i64)
            .field_i64("audio_duration_ms", result.audio_duration_ms)
            .field_i64("processing_time_ms", result.processing_time_ms);
        json.begin_array("segments");
        for segment in segments {
            json.begin_array_object()
                .field_str("speaker", &ptr_to_string(segment.speaker_id))
                .field_i64("speaker_index", segment.speaker_index as i64)
                .field_i64("start_ms", segment.start_ms)
                .field_i64("end_ms", segment.end_ms)
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return;
    }

    if segments.is_empty() {
        out::result_line("(no speech segments detected)");
    } else {
        let header = vec![
            "speaker".to_string(),
            "start".to_string(),
            "end".to_string(),
            "duration".to_string(),
        ];
        let rows: Vec<Vec<String>> = segments
            .iter()
            .map(|segment| {
                let speaker = if segment.speaker_id.is_null() {
                    segment.speaker_index.to_string()
                } else {
                    ptr_to_string(segment.speaker_id)
                };
                vec![
                    speaker,
                    format!("{} ms", segment.start_ms),
                    format!("{} ms", segment.end_ms),
                    format!("{} ms", segment.end_ms - segment.start_ms),
                ]
            })
            .collect();
        out::table(&header, &rows);
    }
    if options.verbose {
        out::status_line(&format!(
            "({} speakers, {} ms)",
            result.speaker_count, result.processing_time_ms
        ));
    }
}

fn run_diarize(
    options: &GlobalOptions,
    p: &Parsed,
    diar_options: sys::rac_diarization_options_t,
) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let model_ref = p.get_str("--model").unwrap_or_default();
    if model_ref.is_empty() {
        out::error_line("--model is required (a diarization model id or on-disk path)");
        return 2;
    }

    let model = match ensure_model_ready(options, &model_ref) {
        Ok(m) => m,
        Err(code) => return code,
    };
    let model_id = model.model_id;
    let model_path = model.primary_path;

    let audio_path = p.get_str("audio").unwrap_or_default();
    let audio_data = match wav::read_wav(&audio_path) {
        Ok(a) => a,
        Err(e) => {
            out::error_line(&e);
            return 1;
        }
    };
    let pcm16 = wav::resample(
        &audio_data.samples,
        audio_data.sample_rate,
        DIARIZATION_SAMPLE_RATE,
    );
    let pcm = wav::to_float(&pcm16);
    if pcm.is_empty() {
        out::error_line(&format!("no audio samples in {audio_path}"));
        return 1;
    }

    let model_id_c = to_cstring(&model_id);
    let model_path_c = to_cstring(&model_path);
    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: model_id_c is kept alive for the call; out_handle is a valid
    // pointer to a local variable.
    let rc = unsafe { sys::rac_diarization_create(model_id_c.as_ptr(), &mut handle) };
    if rc != sys::SUCCESS || handle.is_null() {
        out::error_line(&format!(
            "failed to create diarization service: {}",
            out::describe_result(rc)
        ));
        return 1;
    }

    // SAFETY: handle was just created; model_path_c is kept alive for the call.
    let rc = unsafe { sys::rac_diarization_initialize(handle, model_path_c.as_ptr()) };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "failed to load diarization model: {}",
            out::describe_result(rc)
        ));
        // SAFETY: handle is a valid, live handle.
        unsafe { sys::rac_diarization_destroy(handle) };
        return 1;
    }

    let mut result: sys::rac_diarization_result_t = unsafe { std::mem::zeroed() };
    // SAFETY: handle is live; pcm is a valid buffer kept alive for the call;
    // diar_options/result are valid stack values.
    let rc = unsafe {
        sys::rac_diarization_diarize(handle, pcm.as_ptr(), pcm.len(), &diar_options, &mut result)
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!("diarization failed: {}", out::describe_result(rc)));
        // SAFETY: result was reset by the provider before dispatch and is
        // safe to free (possibly-partial) whether or not diarize succeeded.
        unsafe {
            sys::rac_diarization_result_free(&mut result);
            sys::rac_diarization_cleanup(handle);
            sys::rac_diarization_destroy(handle);
        }
        return 1;
    }

    print_result(options, &model_ref, &result);

    // SAFETY: result was populated by a successful diarize call above.
    unsafe {
        sys::rac_diarization_result_free(&mut result);
        sys::rac_diarization_cleanup(handle);
        sys::rac_diarization_destroy(handle);
    }
    0
}

pub fn register_diarize(app: &mut App) {
    let cmd = app.add_subcommand("diarize", "Label who spoke when in an audio file");

    cmd.add_option("audio", ValueType::Text, "16-bit PCM WAV file")
        .required()
        .check(Validator::ExistingFile);
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        "Diarization model id or on-disk path",
    )
    .required();
    cmd.add_option(
        "--threshold",
        ValueType::Float,
        "Speaker activity needed to open a segment, in [0,1] (default 0.5)",
    );
    cmd.add_option(
        "--minimum-duration-ms,--min-duration",
        // rac_diarization_options_t::minimum_duration_ms is int64_t.
        ValueType::Int64,
        "Drop segments shorter than this many ms (default 0)",
    );
    cmd.add_option(
        "--merge-gap-ms,--merge-gap",
        // rac_diarization_options_t::merge_gap_ms is int64_t.
        ValueType::Int64,
        "Merge same-speaker segments closer than this many ms (default 0)",
    );

    cmd.callback(|p, g| {
        // sample_rate_hz/channel_count are never touched by the CLI (the
        // WAV resample step handles the sample rate separately), so they
        // keep default_diarization_options()'s header default for the life
        // of this struct.
        let mut diar_options = default_diarization_options();
        if let Some(v) = p.get_f64("--threshold") {
            diar_options.threshold = v as f32;
        }
        if let Some(v) = p.get_i64("--minimum-duration-ms") {
            diar_options.minimum_duration_ms = v;
        }
        if let Some(v) = p.get_i64("--merge-gap-ms") {
            diar_options.merge_gap_ms = v;
        }
        run_diarize(g, p, diar_options)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins default_diarization_options() to rac_diarization_types.h's
    // RAC_DIARIZATION_OPTIONS_DEFAULT field by field.
    #[test]
    fn default_diarization_options_matches_header_default() {
        let options = default_diarization_options();
        assert_eq!(options.sample_rate_hz, 16000);
        assert_eq!(options.channel_count, 1);
        assert_eq!(options.threshold, 0.5);
        assert_eq!(options.minimum_duration_ms, 0);
        assert_eq!(options.merge_gap_ms, 0);
    }
}
