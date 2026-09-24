//! `wally voice --input a.wav` — one-shot voice turn (STT → LLM → TTS) via
//! the commons voice agent, mirroring tests/test_voice_agent.cpp.
//!
//! Port of src/commands/cmd_voice.cpp.

use std::ffi::{c_void, CString};

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::{App, Parsed, Validator, ValueType};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{self as out, JsonWriter};
use crate::io::proto::{self, ProtoBuffer};
use crate::io::wav_io as wav;
use crate::sys;

const DEFAULT_STT: &str = "sherpa-onnx-whisper-tiny.en";
const DEFAULT_LLM: &str = "qwen3-0.6b";
const DEFAULT_TTS: &str = "vits-piper-en_US-lessac-medium";
const TURN_SAMPLE_RATE: i32 = 16000;

// SDK strings must not embed a NUL; sanitize defensively instead of
// panicking on file/model-derived input.
fn to_cstring(value: &str) -> CString {
    CString::new(value).unwrap_or_default()
}

fn run_voice(options: &GlobalOptions, p: &Parsed) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let audio = p.get_str("audio").unwrap_or_default();
    let input = p.get_str("--input").unwrap_or_default();
    let input_path = if !audio.is_empty() { audio } else { input };
    if input_path.is_empty() {
        out::error_line("an audio file is required (positional or --input)");
        return 2;
    }

    let stt_ref = p.get_str("--stt").unwrap_or_default();
    let stt_ref: &str = if stt_ref.is_empty() {
        DEFAULT_STT
    } else {
        &stt_ref
    };
    let stt = match ensure_model_ready(options, stt_ref) {
        Ok(m) => m,
        Err(code) => return code,
    };

    let llm_ref = p.get_str("--llm").unwrap_or_default();
    let llm_ref: &str = if llm_ref.is_empty() {
        DEFAULT_LLM
    } else {
        &llm_ref
    };
    let llm = match ensure_model_ready(options, llm_ref) {
        Ok(m) => m,
        Err(code) => return code,
    };

    let tts_ref = p.get_str("--tts").unwrap_or_default();
    let tts_ref: &str = if tts_ref.is_empty() {
        DEFAULT_TTS
    } else {
        &tts_ref
    };
    let tts = match ensure_model_ready(options, tts_ref) {
        Ok(m) => m,
        Err(code) => return code,
    };

    let output = p.get_str("--output").unwrap_or_default();

    let audio_data = match wav::read_wav(&input_path) {
        Ok(a) => a,
        Err(e) => {
            out::error_line(&e);
            return 1;
        }
    };
    let pcm16 = wav::resample(
        &audio_data.samples,
        audio_data.sample_rate,
        TURN_SAMPLE_RATE,
    );
    if pcm16.is_empty() {
        out::error_line("resampled audio is empty");
        return 1;
    }

    let mut agent: sys::rac_voice_agent_handle_t = std::ptr::null_mut();
    // SAFETY: out_handle is a valid pointer to a local variable.
    let rc = unsafe { sys::rac_voice_agent_create_standalone(&mut agent) };
    if rc != sys::SUCCESS || agent.is_null() {
        out::error_line(&format!(
            "failed to create voice agent: {}",
            out::describe_result(rc)
        ));
        return 1;
    }

    let stt_path_c = to_cstring(&stt.primary_path);
    let stt_id_c = to_cstring(&stt.model_id);
    let stt_name_c = to_cstring(&stt.display_name);
    let llm_path_c = to_cstring(&llm.primary_path);
    let llm_id_c = to_cstring(&llm.model_id);
    let llm_name_c = to_cstring(&llm.display_name);
    let tts_path_c = to_cstring(&tts.primary_path);
    let tts_id_c = to_cstring(&tts.model_id);
    let tts_name_c = to_cstring(&tts.display_name);

    // RAC_VOICE_AGENT_CONFIG_DEFAULT is a header-only `static const`
    // (internal C linkage per translation unit), so there is no symbol to
    // link against from Rust; the header's own default values are
    // reproduced here instead. This CLI never sets vad_config, so it keeps
    // the header default for the life of this struct; every stt/llm/tts
    // field is overridden below.
    let mut config = sys::rac_voice_agent_config_t {
        vad_config: sys::rac_voice_agent_vad_config_t {
            sample_rate: 16000,
            frame_length: 0.1,
            energy_threshold: 0.005,
        },
        stt_config: sys::rac_voice_agent_stt_config_t {
            model_path: std::ptr::null(),
            model_id: std::ptr::null(),
            model_name: std::ptr::null(),
        },
        llm_config: sys::rac_voice_agent_llm_config_t {
            model_path: std::ptr::null(),
            model_id: std::ptr::null(),
            model_name: std::ptr::null(),
        },
        tts_config: sys::rac_voice_agent_tts_config_t {
            voice_path: std::ptr::null(),
            voice_id: std::ptr::null(),
            voice_name: std::ptr::null(),
        },
    };
    config.stt_config.model_path = stt_path_c.as_ptr();
    config.stt_config.model_id = stt_id_c.as_ptr();
    config.stt_config.model_name = stt_name_c.as_ptr();
    config.llm_config.model_path = llm_path_c.as_ptr();
    config.llm_config.model_id = llm_id_c.as_ptr();
    config.llm_config.model_name = llm_name_c.as_ptr();
    config.tts_config.voice_path = tts_path_c.as_ptr();
    config.tts_config.voice_id = tts_id_c.as_ptr();
    config.tts_config.voice_name = tts_name_c.as_ptr();

    // SAFETY: agent was just created; config's string pointers are kept
    // alive by the CStrings above for the duration of this call.
    let rc = unsafe { sys::rac_voice_agent_initialize(agent, &config) };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "voice agent init failed: {}",
            out::describe_result(rc)
        ));
        // SAFETY: agent is a valid, live voice agent handle.
        unsafe { sys::rac_voice_agent_destroy(agent) };
        return 1;
    }

    out::status_line(&format!(
        "processing voice turn (stt={}, llm={}, tts={})",
        stt.model_id, llm.model_id, tts.model_id
    ));

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: agent is live; pcm16 is a valid buffer kept alive for the
    // call; out_buffer is a valid, initialized proto buffer.
    let rc = unsafe {
        sys::rac_voice_agent_process_voice_turn_proto(
            agent,
            pcm16.as_ptr() as *const c_void,
            pcm16.len() * std::mem::size_of::<i16>(),
            out_buffer.as_mut_ptr(),
        )
    };

    let mut exit_code = 0;
    match proto::parse_proto_buffer::<proto::v1::VoiceAgentResult>(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => {
            let transcription = result.transcription.unwrap_or_default();
            let assistant_response = result.assistant_response.unwrap_or_default();
            let synthesized_audio = result.synthesized_audio.unwrap_or_default();

            let mut reply_path = String::new();
            // commons (voice_agent_proto_abi.cpp) already wraps the TTS
            // float32 PCM into a complete WAV container via
            // rac_audio_float32_to_wav before setting synthesized_audio, so
            // this is a ready-to-write WAV file, not raw PCM -- no
            // reinterpret/resample here.
            if !output.is_empty() && !synthesized_audio.is_empty() {
                match std::fs::write(&output, &synthesized_audio) {
                    Ok(()) => reply_path = output.clone(),
                    Err(_) => out::status_line(&format!("warning: cannot write {output}")),
                }
            }

            if options.json {
                let mut json = JsonWriter::new();
                json.begin_object()
                    .field_str("transcription", &transcription)
                    .field_str("response", &assistant_response)
                    .field_str("reply_audio", &reply_path)
                    .end_object();
                out::result_line(json.str());
            } else {
                out::result_line(&format!("you   {transcription}"));
                out::result_line(&format!("agent {assistant_response}"));
                if !reply_path.is_empty() {
                    out::result_line(&format!("audio {reply_path}"));
                }
            }
        }
        Ok(_) => {
            out::error_line(&format!("voice turn failed: {}", out::describe_result(rc)));
            exit_code = 1;
        }
        Err(e) => {
            let msg = if rc != sys::SUCCESS {
                out::describe_result(rc)
            } else {
                e
            };
            out::error_line(&format!("voice turn failed: {msg}"));
            exit_code = 1;
        }
    }

    // SAFETY: agent is a valid, live voice agent handle.
    unsafe { sys::rac_voice_agent_destroy(agent) };
    exit_code
}

pub fn register_voice(app: &mut App) {
    let cmd = app.add_subcommand("voice", "Hold one spoken turn: listen, answer, speak");

    cmd.add_option(
        "audio",
        ValueType::Text,
        "16-bit PCM WAV file with the user's speech",
    )
    .check(Validator::ExistingFile);
    cmd.add_option(
        "--input,-i",
        ValueType::Text,
        "16-bit PCM WAV file with the user's speech",
    )
    .check(Validator::ExistingFile);
    cmd.add_option(
        "--stt",
        ValueType::Text,
        &format!("Transcription model (default: {DEFAULT_STT})"),
    );
    cmd.add_option(
        "--llm",
        ValueType::Text,
        &format!("Answering model (default: {DEFAULT_LLM})"),
    );
    cmd.add_option(
        "--tts",
        ValueType::Text,
        &format!("Voice that speaks the reply (default: {DEFAULT_TTS})"),
    );
    cmd.add_option(
        "--output,-o",
        ValueType::Text,
        "WAV file for the spoken reply",
    );

    cmd.callback(|p, g| run_voice(g, p));
}
