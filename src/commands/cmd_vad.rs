//! `wally vad detect <a.wav>` — speech segment detection.
//!
//! `wally vad --input a.wav` is the same command: the options live on the
//! `vad` namespace and `detect` is a fallthrough alias.
//!
//! Feeds 16 kHz float frames through the VAD component (Silero when the model
//! is loaded, energy-based otherwise) and derives segments from
//! is_speech_active transitions.
//!
//! Port of src/commands/cmd_vad.cpp.

use std::ffi::CString;

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Parsed, Validator, ValueType};
use crate::commands::add_verb_alias;
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{self as out, JsonWriter};
use crate::io::wav_io as wav;
use crate::sys;

const DEFAULT_VAD_MODEL: &str = "silero-vad";
const VAD_SAMPLE_RATE: i32 = 16000;
const VAD_FRAME_SAMPLES: usize = 512; // Silero's native frame size @16 kHz

// SDK strings must not embed a NUL; sanitize defensively instead of
// panicking on file/model-derived input.
fn to_cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

struct Segment {
    start_s: f64,
    end_s: f64,
}

fn run_vad(options: &GlobalOptions, p: &Parsed) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let audio = p.get_str("audio").unwrap_or_default();
    let input = p.get_str("--input").unwrap_or_default();
    let audio_path = if !audio.is_empty() { audio } else { input };
    if audio_path.is_empty() {
        out::error_line("an audio file is required (positional or --input)");
        return 2;
    }

    let model_ref = p.get_str("--model").unwrap_or_default();
    let model_ref = if model_ref.is_empty() {
        DEFAULT_VAD_MODEL
    } else {
        &model_ref
    };
    let model = match ensure_model_ready(options, model_ref) {
        Ok(m) => m,
        Err(code) => return code,
    };

    let audio_data = match wav::read_wav(&audio_path) {
        Ok(a) => a,
        Err(e) => {
            out::error_line(&e);
            return 1;
        }
    };
    let pcm16 = wav::resample(&audio_data.samples, audio_data.sample_rate, VAD_SAMPLE_RATE);
    let samples = wav::to_float(&pcm16);

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: out_handle is a valid pointer to a local variable.
    let rc = unsafe { sys::rac_vad_component_create(&mut handle) };
    if rc != sys::SUCCESS {
        out::error_line("failed to create VAD component");
        return 1;
    }

    let model_path_c = to_cstring(&model.primary_path);
    let model_id_c = to_cstring(&model.model_id);
    let model_name_c = to_cstring(&model.display_name);
    // SAFETY: handle was just created; the C strings are kept alive for the
    // duration of this call.
    let rc = unsafe {
        sys::rac_vad_component_load_model(
            handle,
            model_path_c.as_ptr(),
            model_id_c.as_ptr(),
            model_name_c.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "failed to load VAD model: {}",
            out::describe_result(rc)
        ));
        // SAFETY: handle is a valid, live component handle.
        unsafe { sys::rac_vad_component_destroy(handle) };
        return 1;
    }

    let activation_threshold = p.get_f64("--activation-threshold").unwrap_or(0.0) as f32;
    if activation_threshold > 0.0 {
        // SAFETY: handle is a valid, live component handle.
        let rc =
            unsafe { sys::rac_vad_component_set_energy_threshold(handle, activation_threshold) };
        if rc != sys::SUCCESS {
            out::error_line("invalid --activation-threshold (expected 0.0-1.0)");
            // SAFETY: handle is a valid, live component handle.
            unsafe { sys::rac_vad_component_destroy(handle) };
            return 2;
        }
    }

    // SAFETY: handle is a valid, live component handle.
    let mut rc = unsafe { sys::rac_vad_component_initialize(handle) };
    if rc == sys::SUCCESS {
        // SAFETY: handle is a valid, live component handle.
        rc = unsafe { sys::rac_vad_component_start(handle) };
    }
    if rc != sys::SUCCESS {
        out::error_line("failed to start VAD");
        // SAFETY: handle is a valid, live component handle.
        unsafe { sys::rac_vad_component_destroy(handle) };
        return 1;
    }

    let mut segments: Vec<Segment> = Vec::new();
    let mut in_speech = false;
    let mut segment_start = 0.0f64;
    let mut offset = 0usize;
    while offset + VAD_FRAME_SAMPLES <= samples.len() {
        let frame = &samples[offset..offset + VAD_FRAME_SAMPLES];
        let mut frame_is_speech: sys::rac_bool_t = sys::FALSE;
        // SAFETY: handle is live; frame is a valid buffer of
        // VAD_FRAME_SAMPLES floats; frame_is_speech is a valid out-param.
        let rc = unsafe {
            sys::rac_vad_component_process(
                handle,
                frame.as_ptr(),
                VAD_FRAME_SAMPLES,
                &mut frame_is_speech,
            )
        };
        if rc != sys::SUCCESS {
            out::error_line("VAD process failed");
            // SAFETY: handle is a valid, live component handle.
            unsafe {
                sys::rac_vad_component_stop(handle);
                sys::rac_vad_component_destroy(handle);
            }
            return 1;
        }
        let active = frame_is_speech == sys::TRUE;
        let t = (offset + VAD_FRAME_SAMPLES) as f64 / VAD_SAMPLE_RATE as f64;
        if active && !in_speech {
            in_speech = true;
            segment_start = offset as f64 / VAD_SAMPLE_RATE as f64;
        } else if !active && in_speech {
            in_speech = false;
            segments.push(Segment {
                start_s: segment_start,
                end_s: t,
            });
        }
        offset += VAD_FRAME_SAMPLES;
    }
    if in_speech {
        segments.push(Segment {
            start_s: segment_start,
            end_s: samples.len() as f64 / VAD_SAMPLE_RATE as f64,
        });
    }
    // SAFETY: handle is a valid, live component handle.
    unsafe {
        sys::rac_vad_component_stop(handle);
        sys::rac_vad_component_destroy(handle);
    }

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object()
            .field_str("model", &model.model_id)
            .begin_array("segments");
        for segment in &segments {
            json.begin_array_object()
                .field_f64("start_s", segment.start_s)
                .field_f64("end_s", segment.end_s)
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return 0;
    }

    if segments.is_empty() {
        out::result_line("no speech detected");
        return 0;
    }
    let header = vec![
        "START".to_string(),
        "END".to_string(),
        "DURATION".to_string(),
    ];
    let rows: Vec<Vec<String>> = segments
        .iter()
        .map(|segment| {
            vec![
                format!("{:.2}s", segment.start_s),
                format!("{:.2}s", segment.end_s),
                format!("{:.2}s", segment.end_s - segment.start_s),
            ]
        })
        .collect();
    out::table(&header, &rows);
    0
}

pub fn register_vad(app: &mut App) {
    let cmd = app.add_subcommand("vad", "Find the speech in an audio file");
    cmd.require_subcommand(0, 1);
    add_verb_alias(cmd, "detect", "Report speech segments with timestamps");

    cmd.add_option("audio", ValueType::Text, "16-bit PCM WAV file")
        .check(Validator::ExistingFile);
    cmd.add_option("--input,-i", ValueType::Text, "16-bit PCM WAV file")
        .check(Validator::ExistingFile);
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("VAD model to use (default: {DEFAULT_VAD_MODEL})"),
    );
    cmd.add_option(
        "--activation-threshold",
        ValueType::Float,
        "Speech probability needed to open a segment (0 = model default)",
    );

    cmd.callback(|p, g| run_vad(g, p));
}
