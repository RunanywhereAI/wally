//! `wally stt transcribe <audio.wav>` — file transcription via the STT
//! component (same call sequence as the commons real-inference tests).
//!
//! `wally stt --input a.wav` is the same command: the options live on the
//! `stt` namespace and `transcribe` is a fallthrough alias, so both spellings
//! reach one callback.
//!
//! Port of src/commands/cmd_stt.cpp. Owner: the dormant audio modalities port.

use std::ffi::{c_char, c_void, CStr, CString};

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Parsed, Validator, ValueType};
use crate::commands::add_verb_alias;
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{self as out, JsonWriter};
use crate::io::wav_io as wav;
use crate::sys;

const DEFAULT_STT_MODEL: &str = "sherpa-onnx-whisper-tiny.en";
const STT_SAMPLE_RATE: i32 = 16000;

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

fn run_stt(options: &GlobalOptions, p: &Parsed) -> i32 {
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
        DEFAULT_STT_MODEL
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
    let pcm16 = wav::resample(&audio_data.samples, audio_data.sample_rate, STT_SAMPLE_RATE);

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: out_handle is a valid pointer to a local variable.
    let rc = unsafe { sys::rac_stt_component_create(&mut handle) };
    if rc != sys::SUCCESS {
        out::error_line("failed to create STT component");
        return 1;
    }

    let model_path_c = to_cstring(&model.primary_path);
    let model_id_c = to_cstring(&model.model_id);
    let model_name_c = to_cstring(&model.display_name);
    // SAFETY: handle was just created; the C strings are kept alive for the
    // duration of this call.
    let rc = unsafe {
        sys::rac_stt_component_load_model(
            handle,
            model_path_c.as_ptr(),
            model_id_c.as_ptr(),
            model_name_c.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "failed to load STT model: {}",
            out::describe_result(rc)
        ));
        // SAFETY: handle is a valid, live component handle.
        unsafe { sys::rac_stt_component_destroy(handle) };
        return 1;
    }

    let language = p.get_str("--language").unwrap_or_default();
    let language_c = to_cstring(&language);
    let punctuation = p.flag("--punctuation");
    let word_timestamps = p.flag("--word-timestamps");
    let diarization = p.flag("--diarization");
    let max_speakers = p.get_i64("--max-speakers").unwrap_or(0) as i32;

    // RAC_STT_OPTIONS_DEFAULT is a header-only `static const` (internal C
    // linkage per translation unit), so there is no symbol to link against
    // from Rust; every field below is set explicitly instead. Every field
    // this CLI does not otherwise control keeps the header's own default
    // (only audio_format, since every other field is overridden below).
    // SAFETY: rac_stt_options_t is plain data; zeroed is a valid bit pattern
    // for every field, all of which are set below.
    let mut stt_options: sys::rac_stt_options_t = unsafe { std::mem::zeroed() };
    stt_options.language = if language.is_empty() {
        std::ptr::null()
    } else {
        language_c.as_ptr()
    };
    stt_options.detect_language = if language.is_empty() {
        sys::TRUE
    } else {
        sys::FALSE
    };
    stt_options.enable_punctuation = if punctuation { sys::TRUE } else { sys::FALSE };
    stt_options.enable_timestamps = if word_timestamps {
        sys::TRUE
    } else {
        sys::FALSE
    };
    stt_options.enable_diarization = if diarization { sys::TRUE } else { sys::FALSE };
    stt_options.max_speakers = max_speakers;
    stt_options.audio_format = sys::RAC_AUDIO_FORMAT_PCM;
    stt_options.sample_rate = STT_SAMPLE_RATE;

    let mut result: sys::rac_stt_result_t = unsafe { std::mem::zeroed() };
    // SAFETY: handle is live, pcm16 is a valid buffer kept alive for the
    // call, stt_options/result are valid stack values.
    let rc = unsafe {
        sys::rac_stt_component_transcribe(
            handle,
            pcm16.as_ptr() as *const c_void,
            pcm16.len() * std::mem::size_of::<i16>(),
            &stt_options,
            &mut result,
        )
    };

    let mut exit_code = 0;
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "transcription failed: {}",
            out::describe_result(rc)
        ));
        exit_code = 1;
    } else {
        let text = ptr_to_string(result.text);
        if options.json {
            let mut json = JsonWriter::new();
            json.begin_object()
                .field_str("model", &model.model_id)
                .field_str("text", &text)
                .field_str("language", &ptr_to_string(result.detected_language))
                .field_f64("confidence", result.confidence as f64)
                .field_i64("total_ms", result.processing_time_ms);
            json.begin_array("words");
            if !result.words.is_null() && result.num_words > 0 {
                // SAFETY: words/num_words come straight from the SDK result
                // just populated above and are valid until rac_stt_result_free.
                let words = unsafe { std::slice::from_raw_parts(result.words, result.num_words) };
                for word in words {
                    json.begin_array_object()
                        .field_str("text", &ptr_to_string(word.text))
                        .field_i64("start_ms", word.start_ms)
                        .field_i64("end_ms", word.end_ms)
                        .field_f64("confidence", word.confidence as f64)
                        .end_object();
                }
            }
            json.end_array().end_object();
            out::result_line(json.str());
        } else {
            out::result_line(&text);
            if options.verbose {
                out::status_line(&format!("({} ms)", result.processing_time_ms));
            }
        }
        // SAFETY: result was populated by a successful transcribe call above.
        unsafe { sys::rac_stt_result_free(&mut result) };
    }

    // SAFETY: handle is a valid, live component handle.
    unsafe { sys::rac_stt_component_destroy(handle) };
    exit_code
}

pub fn register_stt(app: &mut App) {
    let cmd = app.add_subcommand("stt", "Turn recorded speech into text");
    cmd.require_subcommand(0, 1);
    add_verb_alias(cmd, "transcribe", "Transcribe an audio file");

    cmd.add_option("audio", ValueType::Text, "16-bit PCM WAV file")
        .check(Validator::ExistingFile);
    cmd.add_option("--input,-i", ValueType::Text, "16-bit PCM WAV file")
        .check(Validator::ExistingFile);
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("STT model to use (default: {DEFAULT_STT_MODEL})"),
    );
    cmd.add_option(
        "--language",
        ValueType::Text,
        "BCP-47 language of the speech (omit to auto-detect)",
    );
    cmd.add_flag(
        "--punctuation,!--no-punctuation",
        "Punctuate the transcript (default on)",
    )
    .default_val("true");
    cmd.add_flag(
        "--word-timestamps,!--no-word-timestamps",
        "Report per-word timings (default on)",
    )
    .default_val("true");
    cmd.add_flag("--diarization", "Attribute words to speakers");
    cmd.add_option(
        "--max-speakers",
        ValueType::Int,
        "Cap the speakers diarization may find",
    );

    cmd.callback(|p, g| run_stt(g, p));
}
