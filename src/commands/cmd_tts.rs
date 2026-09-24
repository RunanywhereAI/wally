//! `wally tts synthesize "text" --output o.wav` — speech synthesis.
//!
//! `wally tts --text "…" --output o.wav` is the same command: the options
//! live on the `tts` namespace and `synthesize` is a fallthrough alias.
//!
//! The sherpa TTS engine returns float PCM at the voice's native sample rate;
//! converted to int16 WAV.
//!
//! Port of src/commands/cmd_tts.cpp. Owner: the dormant audio modalities port.

use std::ffi::CString;
use std::time::Instant;

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Parsed, ValueType};
use crate::commands::add_verb_alias;
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{self as out, JsonWriter};
use crate::io::wav_io as wav;
use crate::sys;

const DEFAULT_VOICE: &str = "vits-piper-en_US-lessac-medium";

// SDK strings must not embed a NUL; sanitize defensively instead of
// panicking on file/model-derived input.
fn to_cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn run_tts(options: &GlobalOptions, p: &Parsed) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let positional_text = p.get_str("TEXT").unwrap_or_default();
    let text_flag = p.get_str("--text").unwrap_or_default();
    let text = if !positional_text.is_empty() {
        positional_text
    } else {
        text_flag
    };
    if text.is_empty() {
        out::error_line("text to speak is required (positional or --text)");
        return 2;
    }

    let output = p.get_str("--output").unwrap_or_default();
    if output.is_empty() {
        out::error_line("--output is required");
        return 2;
    }

    let model = p.get_str("--model").unwrap_or_default();
    let voice_field = p.get_str("--voice").unwrap_or_default();
    let model_ref = if !model.is_empty() { &model } else { &voice_field };
    let model_ref: &str = if model_ref.is_empty() {
        DEFAULT_VOICE
    } else {
        model_ref
    };
    let voice = match ensure_model_ready(options, model_ref) {
        Ok(v) => v,
        Err(code) => return code,
    };

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: out_handle is a valid pointer to a local variable.
    let rc = unsafe { sys::rac_tts_component_create(&mut handle) };
    if rc != sys::SUCCESS {
        out::error_line("failed to create TTS component");
        return 1;
    }

    let voice_path_c = to_cstring(&voice.primary_path);
    let voice_id_c = to_cstring(&voice.model_id);
    let voice_name_c = to_cstring(&voice.display_name);
    // SAFETY: handle was just created; the C strings are kept alive for the
    // duration of this call.
    let rc = unsafe {
        sys::rac_tts_component_load_voice(
            handle,
            voice_path_c.as_ptr(),
            voice_id_c.as_ptr(),
            voice_name_c.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!("failed to load voice: {}", out::describe_result(rc)));
        // SAFETY: handle is a valid, live component handle.
        unsafe { sys::rac_tts_component_destroy(handle) };
        return 1;
    }

    let voice_field_c = to_cstring(&voice_field);
    let language = p.get_str("--language").unwrap_or_default();
    let language_c = to_cstring(&language);
    let speed = p.get_f64("--speed").unwrap_or(1.0) as f32;
    let pitch = p.get_f64("--pitch").unwrap_or(1.0) as f32;
    let sample_rate = p.get_i64("--sample-rate").unwrap_or(0) as i32;

    // SAFETY: reading a kit-provided default struct (plain data, no pointers).
    let mut tts_options = unsafe { sys::RAC_TTS_OPTIONS_DEFAULT };
    tts_options.voice = if voice_field.is_empty() {
        std::ptr::null()
    } else {
        voice_field_c.as_ptr()
    };
    if !language.is_empty() {
        tts_options.language = language_c.as_ptr();
    }
    tts_options.rate = speed;
    tts_options.pitch = pitch;
    if sample_rate > 0 {
        tts_options.sample_rate = sample_rate;
    }

    let text_c = to_cstring(&text);
    let started = Instant::now();
    let mut result: sys::rac_tts_result_t = unsafe { std::mem::zeroed() };
    // SAFETY: handle is live, text_c/tts_options are valid for the call, and
    // result is a valid stack out-param.
    let rc = unsafe {
        sys::rac_tts_component_synthesize(handle, text_c.as_ptr(), &tts_options, &mut result)
    };
    let elapsed_ms = started.elapsed().as_millis() as i64;

    let mut exit_code = 0;
    if rc != sys::SUCCESS || result.audio_data.is_null() || result.audio_size == 0 {
        out::error_line(&format!("synthesis failed: {}", out::describe_result(rc)));
        exit_code = 1;
    } else {
        let sample_count = result.audio_size / std::mem::size_of::<f32>();
        // SAFETY: audio_data/audio_size were just populated by a successful
        // synthesize call and are valid float32 PCM until rac_tts_result_free.
        let float_samples =
            unsafe { std::slice::from_raw_parts(result.audio_data as *const f32, sample_count) };

        match wav::write_wav_f32(&output, float_samples, result.sample_rate) {
            Err(e) => {
                out::error_line(&e);
                exit_code = 1;
            }
            Ok(()) => {
                if options.json {
                    let mut json = JsonWriter::new();
                    json.begin_object()
                        .field_str("voice", &voice.model_id)
                        .field_str("path", &output)
                        .field_i64("sample_rate", result.sample_rate as i64)
                        .field_i64("duration_ms", result.duration_ms)
                        .field_i64("total_ms", elapsed_ms)
                        .end_object();
                    out::result_line(json.str());
                } else {
                    out::result_line(&output);
                    if options.verbose {
                        out::status_line(&format!("({elapsed_ms} ms, {} Hz)", result.sample_rate));
                    }
                }
            }
        }
        // SAFETY: result was populated by a successful synthesize call above.
        unsafe { sys::rac_tts_result_free(&mut result) };
    }

    // SAFETY: handle is a valid, live component handle.
    unsafe { sys::rac_tts_component_destroy(handle) };
    exit_code
}

pub fn register_tts(app: &mut App) {
    let cmd = app.add_subcommand("tts", "Speak text with an on-device voice");
    cmd.require_subcommand(0, 1);
    add_verb_alias(cmd, "synthesize", "Write spoken audio to a WAV file");

    // CLI11 matches option names without their dashes, so the positional
    // cannot also be called "text" while `--text` exists.
    cmd.add_option("TEXT", ValueType::Text, "Text to speak");
    cmd.add_option("--text,-t", ValueType::Text, "Text to speak");
    cmd.add_option("--output,-o", ValueType::Text, "WAV file to write");
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("Voice model to load (default: {DEFAULT_VOICE})"),
    );
    cmd.add_option(
        "--voice",
        ValueType::Text,
        "Voice inside the model to speak with",
    );
    cmd.add_option(
        "--language",
        ValueType::Text,
        "BCP-47 language to speak (default en-US)",
    );
    cmd.add_option("--speed", ValueType::Float, "Speak faster or slower than 1.0")
        .default_val("1.0");
    cmd.add_option("--pitch", ValueType::Float, "Raise or lower the pitch from 1.0")
        .default_val("1.0");
    cmd.add_option(
        "--sample-rate",
        ValueType::Int,
        "Output sample rate in Hz (0 = the voice's own)",
    );

    cmd.callback(|p, g| run_tts(g, p));
}
