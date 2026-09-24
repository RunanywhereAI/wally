//! In-process wally E2E coverage for the MLX backend contract, ported from
//! tests/test_wally_mlx_e2e.cpp.
//!
//! The production MLX runtime is Swift/MLX. This test installs the same C
//! callback table that the Swift runtime installs, then invokes the actual
//! wally command stack against a local MLX-style folder. That keeps the test
//! offline and fast while exercising wally parsing, bootstrap, backend
//! registration, commons lifecycle loading, MLX callback dispatch, and
//! streaming output.
//!
//! Gated exactly like the CMake target (`if(APPLE AND RunAnywhere_HAS_MLX)`):
//! Apple only, and only when the linked kit has the MLX backend.
#![cfg(all(target_os = "macos", wally_has_mlx))]

mod common;
#[path = "common/mlx_fakes.rs"]
mod mlx_fakes;

use std::ffi::CString;
use std::path::Path;

use wally::io::proto::v1;
use wally::sys;

use mlx_fakes as fakes;

/// Captures everything written to fd 1 (`STDOUT_FILENO`) for its lifetime.
/// wally's `io::output` writes via `std::io::stdout().lock()`, which bypasses
/// libtest's own stdout capture (that only intercepts the `print!` family),
/// so the C++ file's raw-fd pipe trick is ported as-is.
///
/// Unlike the C++ version (whose `finish()` is the only cleanup path, safe
/// because `app.parse()` cannot escape past the try/catch above it), this
/// guard restores fd 1 in `Drop` too: `common::run_in_process` calls into
/// still-unported `wally::app::run()`, which currently panics because its
/// body is an unimplemented placeholder. If that panic unwinds through a
/// capture, `Drop` still runs and restores stdout, so one blocked test
/// cannot corrupt output capture for
/// every other test in this binary (cargo test runs `#[test]` fns on
/// parallel threads within one process).
struct StdoutCapture {
    saved_stdout: i32,
    pipe_read: i32,
    pipe_write: i32,
    restored: bool,
}

impl StdoutCapture {
    fn start() -> Option<Self> {
        // SAFETY: flushes Rust's own stdout buffer before swapping the raw
        // fd, matching the C++ file's `std::fflush(stdout)` rationale: text
        // already queued for fd 1 must land before the fd is repointed, or
        // it corrupts a later capture.
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let mut fds: [i32; 2] = [-1, -1];
        // SAFETY: `fds` is a valid 2-element buffer for libc::pipe to fill.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: STDOUT_FILENO is always a valid fd to dup.
        let saved_stdout = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if saved_stdout < 0 {
            return None;
        }
        // SAFETY: fds[1] is the just-created pipe write end; STDOUT_FILENO
        // is a valid dup2 target.
        if unsafe { libc::dup2(fds[1], libc::STDOUT_FILENO) } < 0 {
            return None;
        }
        Some(StdoutCapture {
            saved_stdout,
            pipe_read: fds[0],
            pipe_write: fds[1],
            restored: false,
        })
    }

    /// Restores fd 1 and drains the pipe, returning everything captured.
    fn finish(mut self) -> String {
        self.restore();
        let mut output = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            // SAFETY: pipe_read is a valid, open fd owned by this capture
            // until closed just below; buffer is a valid 4096-byte target.
            let n = unsafe {
                libc::read(
                    self.pipe_read,
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if n <= 0 {
                break;
            }
            output.extend_from_slice(&buffer[..n as usize]);
        }
        // SAFETY: pipe_read is open and owned by this capture.
        unsafe { libc::close(self.pipe_read) };
        self.pipe_read = -1;
        String::from_utf8_lossy(&output).into_owned()
    }

    /// Restores the saved stdout fd onto fd 1 and closes the pipe write end.
    /// Idempotent so both `finish()` and a panic-triggered `Drop` can call
    /// it safely.
    fn restore(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        use std::io::Write;
        let _ = std::io::stdout().flush();
        if self.saved_stdout >= 0 {
            // SAFETY: saved_stdout was produced by `dup` in `start()` and is
            // still open; STDOUT_FILENO is a valid dup2 target.
            unsafe { libc::dup2(self.saved_stdout, libc::STDOUT_FILENO) };
            // SAFETY: saved_stdout is open and owned by this capture.
            unsafe { libc::close(self.saved_stdout) };
            self.saved_stdout = -1;
        }
        if self.pipe_write >= 0 {
            // SAFETY: pipe_write is open and owned by this capture.
            unsafe { libc::close(self.pipe_write) };
            self.pipe_write = -1;
        }
    }
}

impl Drop for StdoutCapture {
    fn drop(&mut self) {
        self.restore();
        if self.pipe_read >= 0 {
            // SAFETY: pipe_read is open and owned by this capture; only
            // reached if `finish()` was never called (e.g. a panic).
            unsafe { libc::close(self.pipe_read) };
        }
    }
}

/// Runs the wally entry point in-process with captured stdout, porting the
/// C++ file's `run_cli_capture`. The C++ version builds its own `CLI::App`
/// and calls `wally::configure_app` + `app.parse()` directly; here that same
/// parse-and-dispatch sequence lives behind `wally::app::run()` (via
/// `common::run_in_process`), so this wraps that call instead of duplicating
/// CLI parser setup the CLI area already owns.
fn run_cli_capture(args: &[&str]) -> (i32, String) {
    let Some(capture) = StdoutCapture::start() else {
        return (1, String::new());
    };
    let code = common::run_in_process(args);
    (code, capture.finish())
}

/// Runs the CLI, failing `result` (returns `Err`) on a non-zero exit code.
/// Ports `run_cli_or_fail`.
fn run_cli_or_fail(args: &[&str], expected_label: &str) -> Result<String, String> {
    let (code, stdout_text) = run_cli_capture(args);
    if code == 0 {
        Ok(stdout_text)
    } else {
        Err(format!(
            "{expected_label} exit 0, got exit {code}: {stdout_text}"
        ))
    }
}

/// Checks the `backends` command's JSON output for a backend with the given
/// name that reports every primitive in `expected`. Ports
/// `backend_has_primitives`, using `serde_json` instead of the C++ file's
/// hand-rolled bracket scanner since this is test-side assertion logic, not
/// CLI output that itself needs byte-identical behavior.
fn backend_has_primitives(
    json_text: &str,
    backend_name: &str,
    expected: &[&str],
) -> Result<(), String> {
    let parsed: serde_json::Value =
        serde_json::from_str(json_text).map_err(|e| format!("invalid JSON: {e}"))?;
    let backends = parsed
        .get("backends")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "no backends field".to_string())?;
    let backend = backends
        .iter()
        .find(|b| b.get("name").and_then(|n| n.as_str()) == Some(backend_name))
        .ok_or_else(|| format!("backend not found: {backend_name}"))?;
    let primitives: std::collections::HashSet<&str> = backend
        .get("primitives")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    for want in expected {
        if !primitives.contains(want) {
            return Err(format!("missing primitive {want}"));
        }
    }
    Ok(())
}

/// Registers a local MLX model folder with the SDK's model registry, ported
/// from `register_local_mlx_model`.
fn register_local_mlx_model(
    model_dir: &Path,
    id: &str,
    name: &str,
    category: v1::ModelCategory,
) -> bool {
    let model = v1::ModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        category: category as i32,
        format: v1::ModelFormat::Safetensors as i32,
        framework: v1::InferenceFramework::Mlx as i32,
        local_path: model_dir.to_string_lossy().into_owned(),
        is_available: Some(true),
        registry_status: Some(v1::ModelRegistryStatus::Downloaded as i32),
        artifact: Some(v1::model_info::Artifact::MultiFile(v1::MultiFileArtifact {
            files: vec![
                v1::ModelFileDescriptor {
                    filename: "config.json".into(),
                    destination_path: Some("config.json".into()),
                    is_optional: false,
                    role: Some(v1::ModelFileRole::Companion as i32),
                    ..Default::default()
                },
                v1::ModelFileDescriptor {
                    filename: "model.safetensors".into(),
                    destination_path: Some("model.safetensors".into()),
                    is_optional: false,
                    role: Some(v1::ModelFileRole::PrimaryModel as i32),
                    ..Default::default()
                },
                v1::ModelFileDescriptor {
                    filename: "tokenizer.json".into(),
                    destination_path: Some("tokenizer.json".into()),
                    is_optional: false,
                    role: Some(v1::ModelFileRole::Tokenizer as i32),
                    ..Default::default()
                },
            ],
        })),
        ..Default::default()
    };
    let bytes = wally::io::proto::serialize(&model);
    // SAFETY: bytes is a valid, exclusively-owned byte buffer for the
    // duration of this call; rac_get_model_registry() returns the process
    // singleton registry handle.
    unsafe {
        sys::rac_model_registry_register_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
        ) == sys::SUCCESS
    }
}

/// Writes `contents` to `path`, creating parent directories as needed.
fn write_file(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)
}

/// Ports `test_mlx_callback_bridge_all_slots`: installs the fake MLX
/// callback table, looks up the registered MLX engine vtable for each of the
/// five modalities the test covers, and drives every op directly (bypassing
/// the CLI and bootstrap, which other areas of the port still own).
#[test]
fn mlx_callback_bridge_all_slots() {
    let _lock = fakes::mlx_lock();
    fakes::reset_state();
    assert!(
        fakes::install_fake_mlx_callbacks(),
        "install fake MLX callbacks"
    );
    fakes::register_mlx_backend_or_fail().expect("register MLX backend");
    let _backend_guard = fakes::MlxBackendGuard;

    // SAFETY: "mlx" is a 'static NUL-terminated engine name; the FFI call has
    // no other preconditions.
    let llm_vt = unsafe {
        sys::rac_plugin_find_for_engine(
            sys::RAC_PRIMITIVE_GENERATE_TEXT as sys::rac_primitive_t,
            c"mlx".as_ptr(),
        )
    };
    // SAFETY: as above.
    let vlm_vt = unsafe {
        sys::rac_plugin_find_for_engine(
            sys::RAC_PRIMITIVE_VLM as sys::rac_primitive_t,
            c"mlx".as_ptr(),
        )
    };
    // SAFETY: as above.
    let embed_vt = unsafe {
        sys::rac_plugin_find_for_engine(
            sys::RAC_PRIMITIVE_EMBED as sys::rac_primitive_t,
            c"mlx".as_ptr(),
        )
    };
    // SAFETY: as above.
    let stt_vt = unsafe {
        sys::rac_plugin_find_for_engine(
            sys::RAC_PRIMITIVE_TRANSCRIBE as sys::rac_primitive_t,
            c"mlx".as_ptr(),
        )
    };
    // SAFETY: as above.
    let tts_vt = unsafe {
        sys::rac_plugin_find_for_engine(
            sys::RAC_PRIMITIVE_SYNTHESIZE as sys::rac_primitive_t,
            c"mlx".as_ptr(),
        )
    };

    assert!(
        !llm_vt.is_null()
            && !vlm_vt.is_null()
            && !embed_vt.is_null()
            && !stt_vt.is_null()
            && !tts_vt.is_null(),
        "registered MLX vtable for every modality"
    );
    // SAFETY: each pointer above was just null-checked and, once
    // registered, is valid for the registry's lifetime per
    // rac_plugin_find_for_engine's documented contract.
    let (llm_vt, vlm_vt, embed_vt, stt_vt, tts_vt) =
        unsafe { (&*llm_vt, &*vlm_vt, &*embed_vt, &*stt_vt, &*tts_vt) };
    assert!(
        !llm_vt.llm_ops.is_null()
            && !vlm_vt.vlm_ops.is_null()
            && !embed_vt.embedding_ops.is_null()
            && !stt_vt.stt_ops.is_null()
            && !tts_vt.tts_ops.is_null(),
        "registered MLX vtable with all modality op slots"
    );
    // SAFETY: each *_ops pointer was just null-checked above and is valid
    // for the registry's lifetime.
    let (llm_ops, vlm_ops, embed_ops, stt_ops, tts_ops) = unsafe {
        (
            &*llm_vt.llm_ops,
            &*vlm_vt.vlm_ops,
            &*embed_vt.embedding_ops,
            &*stt_vt.stt_ops,
            &*tts_vt.tts_ops,
        )
    };

    // --- LLM ---
    let model_id = c"mlx.direct.llm";
    let model_path = c"/tmp/mlx-direct-llm";
    let prompt_direct = c"direct";
    let prompt_stream = c"stream";
    let mut llm: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: every ops fn pointer is Some (checked via null-vtable check
    // above implies non-null service structs from the SDK, which always
    // populate every required slot); every pointer argument is a live,
    // correctly-typed local for the duration of the call.
    unsafe {
        assert_eq!(
            (llm_ops.create.unwrap())(model_id.as_ptr(), std::ptr::null(), &mut llm),
            sys::SUCCESS
        );
        assert_eq!(
            (llm_ops.initialize.unwrap())(llm, model_path.as_ptr()),
            sys::SUCCESS
        );
    }
    let mut llm_result: sys::rac_llm_result_t = unsafe { std::mem::zeroed() };
    let mut llm_stream = String::new();
    let mut llm_info: sys::rac_llm_info_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above; llm_stream's address is passed as callback_user_data
    // and only accessed synchronously within this call.
    unsafe {
        assert_eq!(
            (llm_ops.generate.unwrap())(
                llm,
                prompt_direct.as_ptr(),
                std::ptr::null(),
                &mut llm_result
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (llm_ops.generate_stream.unwrap())(
                llm,
                prompt_stream.as_ptr(),
                std::ptr::null(),
                Some(fakes::append_llm_token_callback),
                &mut llm_stream as *mut String as *mut std::ffi::c_void,
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (llm_ops.get_info.unwrap())(llm, &mut llm_info),
            sys::SUCCESS
        );
        assert_eq!((llm_ops.cancel.unwrap())(llm), sys::SUCCESS);
        assert_eq!((llm_ops.cleanup.unwrap())(llm), sys::SUCCESS);
    }
    let llm_text = unsafe { std::ffi::CStr::from_ptr(llm_result.text) }.to_string_lossy();
    assert_eq!(llm_text, "mlx-stub: direct");
    assert_eq!(llm_stream, "mlx-stub: stream");
    assert_eq!(llm_info.is_ready, sys::TRUE);
    {
        let state = fakes::lock_state();
        assert_eq!(state.llm_generate_count, 1);
        assert_eq!(state.stream_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_LLM as sys::rac_mlx_session_kind_t
        );
    }
    // SAFETY: llm_result was populated by generate() above and is freed
    // exactly once here; llm is destroyed exactly once after.
    unsafe {
        sys::rac_llm_result_free(&mut llm_result);
        (llm_ops.destroy.unwrap())(llm);
    }

    // --- VLM ---
    let vlm_model_id = c"mlx.direct.vlm";
    let vlm_model_path = c"/tmp/mlx-direct-vlm";
    let image_path = c"/tmp/mlx-direct-image.jpg";
    let prompt_look = c"look";
    let prompt_watch = c"watch";
    let mut vlm: *mut std::ffi::c_void = std::ptr::null_mut();
    let image = sys::rac_vlm_image_t {
        format: sys::RAC_VLM_IMAGE_FORMAT_FILE_PATH,
        file_path: image_path.as_ptr(),
        pixel_data: std::ptr::null(),
        base64_data: std::ptr::null(),
        width: 0,
        height: 0,
        data_size: 0,
    };
    let mut vlm_result: sys::rac_vlm_result_t = unsafe { std::mem::zeroed() };
    let mut vlm_stream = String::new();
    let mut vlm_info: sys::rac_vlm_info_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe {
        assert_eq!(
            (vlm_ops.create.unwrap())(vlm_model_id.as_ptr(), std::ptr::null(), &mut vlm),
            sys::SUCCESS
        );
        assert_eq!(
            (vlm_ops.initialize.unwrap())(vlm, vlm_model_path.as_ptr(), std::ptr::null()),
            sys::SUCCESS
        );
        assert_eq!(
            (vlm_ops.process.unwrap())(
                vlm,
                &image,
                prompt_look.as_ptr(),
                std::ptr::null(),
                &mut vlm_result
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (vlm_ops.process_stream.unwrap())(
                vlm,
                &image,
                prompt_watch.as_ptr(),
                std::ptr::null(),
                Some(fakes::append_token_callback),
                &mut vlm_stream as *mut String as *mut std::ffi::c_void,
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (vlm_ops.get_info.unwrap())(vlm, &mut vlm_info),
            sys::SUCCESS
        );
        assert_eq!((vlm_ops.cancel.unwrap())(vlm), sys::SUCCESS);
        assert_eq!((vlm_ops.cleanup.unwrap())(vlm), sys::SUCCESS);
    }
    let vlm_text = unsafe { std::ffi::CStr::from_ptr(vlm_result.text) }.to_string_lossy();
    assert_eq!(vlm_text, "mlx-vlm-stub: look");
    assert_eq!(vlm_stream, "watch");
    assert_eq!(vlm_info.is_ready, sys::TRUE);
    {
        let state = fakes::lock_state();
        assert_eq!(state.vlm_process_count, 1);
        assert_eq!(state.vlm_stream_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_VLM as sys::rac_mlx_session_kind_t
        );
    }
    // SAFETY: vlm_result was populated by process() above and is freed
    // exactly once here; vlm is destroyed exactly once after.
    unsafe {
        sys::rac_vlm_result_free(&mut vlm_result);
        (vlm_ops.destroy.unwrap())(vlm);
    }

    // --- Embeddings ---
    let embed_model_id = c"mlx.direct.embed";
    let embed_model_path = c"/tmp/mlx-direct-embed";
    let single_text = c"single";
    let mut embed: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut embed_result: sys::rac_embeddings_result_t = unsafe { std::mem::zeroed() };
    let mut embed_info: sys::rac_embeddings_info_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe {
        assert_eq!(
            (embed_ops.create.unwrap())(embed_model_id.as_ptr(), std::ptr::null(), &mut embed),
            sys::SUCCESS
        );
        assert_eq!(
            (embed_ops.initialize.unwrap())(embed, embed_model_path.as_ptr()),
            sys::SUCCESS
        );
        assert_eq!(
            (embed_ops.embed.unwrap())(
                embed,
                single_text.as_ptr(),
                std::ptr::null(),
                &mut embed_result
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (embed_ops.get_info.unwrap())(embed, &mut embed_info),
            sys::SUCCESS
        );
        sys::rac_embeddings_result_free(&mut embed_result);
    }
    let text_one = CString::new("one").unwrap();
    let text_two = CString::new("two").unwrap();
    let embed_texts = [text_one.as_ptr(), text_two.as_ptr()];
    // SAFETY: as above; embed_texts has exactly 2 entries, matching the
    // num_texts argument.
    unsafe {
        assert_eq!(
            (embed_ops.embed_batch.unwrap())(
                embed,
                embed_texts.as_ptr(),
                2,
                std::ptr::null(),
                &mut embed_result
            ),
            sys::SUCCESS
        );
        assert_eq!((embed_ops.cleanup.unwrap())(embed), sys::SUCCESS);
    }
    assert_eq!(embed_result.num_embeddings, 2);
    assert_eq!(embed_info.is_ready, sys::TRUE);
    {
        let state = fakes::lock_state();
        assert_eq!(state.embed_batch_count, 2);
        assert_eq!(state.embedding_info_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_EMBEDDINGS as sys::rac_mlx_session_kind_t
        );
    }
    // SAFETY: embed_result was populated by embed_batch() above and is freed
    // exactly once here; embed is destroyed exactly once after.
    unsafe {
        sys::rac_embeddings_result_free(&mut embed_result);
        (embed_ops.destroy.unwrap())(embed);
    }

    // --- STT ---
    let stt_model_id = c"mlx.direct.stt";
    let stt_model_path = c"/tmp/mlx-direct-stt";
    let samples: [i16; 4] = [0, 128, -128, 256];
    let mut stt: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut stt_result: sys::rac_stt_result_t = unsafe { std::mem::zeroed() };
    let mut stt_stream = String::new();
    let mut stt_info: sys::rac_stt_info_t = unsafe { std::mem::zeroed() };
    let audio_size = std::mem::size_of_val(&samples);
    // SAFETY: as above; samples is a valid Int16 buffer for audio_size
    // bytes, per the STT ops audio_data/audio_size contract.
    unsafe {
        assert_eq!(
            (stt_ops.create.unwrap())(stt_model_id.as_ptr(), std::ptr::null(), &mut stt),
            sys::SUCCESS
        );
        assert_eq!(
            (stt_ops.initialize.unwrap())(stt, stt_model_path.as_ptr()),
            sys::SUCCESS
        );
        assert_eq!(
            (stt_ops.transcribe.unwrap())(
                stt,
                samples.as_ptr() as *const std::ffi::c_void,
                audio_size,
                std::ptr::null(),
                &mut stt_result
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (stt_ops.transcribe_stream.unwrap())(
                stt,
                samples.as_ptr() as *const std::ffi::c_void,
                audio_size,
                std::ptr::null(),
                Some(fakes::append_stt_callback),
                &mut stt_stream as *mut String as *mut std::ffi::c_void,
            ),
            sys::SUCCESS
        );
        assert_eq!(
            (stt_ops.get_info.unwrap())(stt, &mut stt_info),
            sys::SUCCESS
        );
        assert_eq!((stt_ops.cleanup.unwrap())(stt), sys::SUCCESS);
    }
    let stt_text = unsafe { std::ffi::CStr::from_ptr(stt_result.text) }.to_string_lossy();
    assert!(stt_text.contains("mlx-stt-stub"));
    assert_eq!(stt_stream, "partial:mlx-stt-partial|final:mlx-stt-final");
    assert_eq!(stt_info.is_ready, sys::TRUE);
    {
        let state = fakes::lock_state();
        assert_eq!(state.stt_transcribe_count, 1);
        assert_eq!(state.stt_stream_count, 1);
        assert_eq!(state.stt_info_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_STT as sys::rac_mlx_session_kind_t
        );
    }
    // SAFETY: stt_result was populated by transcribe() above and is freed
    // exactly once here; stt is destroyed exactly once after.
    unsafe {
        sys::rac_stt_result_free(&mut stt_result);
        (stt_ops.destroy.unwrap())(stt);
    }

    // --- TTS ---
    let tts_model_id = c"/tmp/mlx-direct-tts";
    let say_it = c"say it";
    let stream_it = c"stream it";
    let mut tts: *mut std::ffi::c_void = std::ptr::null_mut();
    let mut tts_result: sys::rac_tts_result_t = unsafe { std::mem::zeroed() };
    let mut streamed_tts_bytes: usize = 0;
    let mut tts_info: sys::rac_tts_info_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above; TTS create/initialize take no model_path (voice ID
    // is passed via create's model_id).
    unsafe {
        assert_eq!(
            (tts_ops.create.unwrap())(tts_model_id.as_ptr(), std::ptr::null(), &mut tts),
            sys::SUCCESS
        );
        assert_eq!((tts_ops.initialize.unwrap())(tts), sys::SUCCESS);
        assert_eq!(
            (tts_ops.synthesize.unwrap())(tts, say_it.as_ptr(), std::ptr::null(), &mut tts_result),
            sys::SUCCESS
        );
        assert_eq!(
            (tts_ops.synthesize_stream.unwrap())(
                tts,
                stream_it.as_ptr(),
                std::ptr::null(),
                Some(fakes::count_tts_chunk_callback),
                &mut streamed_tts_bytes as *mut usize as *mut std::ffi::c_void,
            ),
            sys::SUCCESS
        );
        assert_eq!((tts_ops.stop.unwrap())(tts), sys::SUCCESS);
        assert_eq!(
            (tts_ops.get_info.unwrap())(tts, &mut tts_info),
            sys::SUCCESS
        );
        assert_eq!((tts_ops.cleanup.unwrap())(tts), sys::SUCCESS);
    }
    // tts_stop_count is intentionally NOT asserted here: the kit only
    // forwards stop/cancel to the backend while the originating operation is
    // still active -- a stop() called after synthesize_stream() has already
    // returned is a late interrupt and is deliberately dropped, so it cannot
    // poison the next inference on this session. The LLM/VLM sections above
    // hold cancel() to the same bar: they only check for RAC_SUCCESS, not
    // that the fake callback fired.
    assert!(!tts_result.audio_data.is_null());
    assert_eq!(tts_result.audio_size, 8 * std::mem::size_of::<f32>());
    assert_eq!(streamed_tts_bytes, 2 * std::mem::size_of::<f32>());
    assert_eq!(tts_info.is_ready, sys::TRUE);
    {
        let state = fakes::lock_state();
        assert_eq!(state.tts_synthesize_count, 1);
        assert_eq!(state.tts_stream_count, 1);
        assert_eq!(state.tts_info_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_TTS as sys::rac_mlx_session_kind_t
        );
    }
    // SAFETY: tts_result was populated by synthesize() above and is freed
    // exactly once here; tts is destroyed exactly once after.
    unsafe {
        sys::rac_tts_result_free(&mut tts_result);
        (tts_ops.destroy.unwrap())(tts);
    }

    {
        let state = fakes::lock_state();
        assert_eq!(state.create_count, 5, "one create() per modality");
        assert_eq!(state.initialize_count, 5, "one initialize() per modality");
    }
    // _backend_guard's Drop calls rac_backend_mlx_unregister() here (and on
    // any earlier panic), replacing every `rac_backend_mlx_unregister();
    // return result;` cleanup branch the C++ version repeats by hand.
}

/// Ports `test_wally_mlx_run_end_to_end`. Drives the CLI (`backends`,
/// `models list --all`, `run`) against local MLX model folders through
/// `wally::bootstrap` / `common::run_in_process`, which other, not-yet-ported
/// areas of this migration own (the CLI parser/dispatch and bootstrap). This
/// is expected to fail until those areas land; see the final report for
/// which piece it is currently blocked on.
///
/// Ports only the currently-active branch of the C++ source: that file has
/// `#define WALLY_LLM_ONLY_CUT 1` compiling out its embed/STT/TTS CLI
/// assertions (dead code today, matching src/app.cpp's LLM-only command
/// registration), and this test mirrors exactly that: it verifies the
/// backend/list/LLM/VLM behavior, then shuts down and checks
/// `create_count == 2 && initialize_count == 2`, same as the compiled C++
/// branch. Flip both together when the LLM-only cut is reverted upstream.
#[test]
fn wally_mlx_run_end_to_end() {
    let _lock = fakes::mlx_lock();
    fakes::reset_state();
    assert!(
        fakes::install_fake_mlx_callbacks(),
        "install fake MLX callbacks"
    );

    let home = tempfile::tempdir().expect("temp home");
    let llm_dir = tempfile::tempdir().expect("temp llm dir");
    let vlm_dir = tempfile::tempdir().expect("temp vlm dir");
    let embedding_dir = tempfile::tempdir().expect("temp embedding dir");
    let stt_dir = tempfile::tempdir().expect("temp stt dir");
    let tts_dir = tempfile::tempdir().expect("temp tts dir");
    for dir in [&llm_dir, &vlm_dir, &embedding_dir, &stt_dir, &tts_dir] {
        write_file(&dir.path().join("config.json"), r#"{"model_type":"qwen3"}"#)
            .expect("write config.json");
        write_file(&dir.path().join("model.safetensors"), "fake-weights")
            .expect("write model.safetensors");
        write_file(&dir.path().join("tokenizer.json"), "{}").expect("write tokenizer.json");
    }

    let input_wav = home.path().join("input.wav");
    let input_image = home.path().join("image.rgb");
    write_file(&input_image, "fake image").expect("write fake VLM image");
    let pcm_samples: [i16; 8] = [0, 1024, -1024, 2048, -2048, 1024, -1024, 0];
    wally::io::wav_io::write_wav(input_wav.to_str().expect("utf-8 path"), &pcm_samples, 16000)
        .expect("write input wav");

    let mut options = wally::bootstrap::GlobalOptions::default();
    options.home_override = home.path().to_string_lossy().into_owned();
    options.json = true;
    options.no_progress = true;
    let _bootstrapped = wally::bootstrap::bootstrap(&options).expect("bootstrap");

    assert!(
        register_local_mlx_model(
            llm_dir.path(),
            "mlx.fake.llm",
            "Fake MLX LLM",
            v1::ModelCategory::Language
        ),
        "register local MLX LLM model"
    );
    assert!(
        register_local_mlx_model(
            vlm_dir.path(),
            "mlx.fake.vlm",
            "Fake MLX VLM",
            v1::ModelCategory::Multimodal
        ),
        "register local MLX VLM model"
    );
    assert!(
        register_local_mlx_model(
            embedding_dir.path(),
            "mlx.fake.embed",
            "Fake MLX Embeddings",
            v1::ModelCategory::Embedding
        ),
        "register local MLX embedding model"
    );
    assert!(
        register_local_mlx_model(
            stt_dir.path(),
            "mlx.fake.stt",
            "Fake MLX STT",
            v1::ModelCategory::SpeechRecognition
        ),
        "register local MLX STT model"
    );
    assert!(
        register_local_mlx_model(
            tts_dir.path(),
            "mlx.fake.tts",
            "Fake MLX TTS",
            v1::ModelCategory::SpeechSynthesis
        ),
        "register local MLX TTS model"
    );

    let home_str = home.path().to_string_lossy().into_owned();

    let backends_json = run_cli_or_fail(
        &[
            "wally",
            "--json",
            "--no-progress",
            "--home",
            &home_str,
            "backends",
        ],
        "backends",
    )
    .expect("backends command");
    backend_has_primitives(&backends_json, "mlx", &["generate_text", "vlm", "embed", "transcribe", "synthesize"])
        .unwrap_or_else(|e| panic!("mlx backend with generate_text/vlm/embed/transcribe/synthesize primitives: {e}: {backends_json}"));

    let list_json = run_cli_or_fail(
        &[
            "wally",
            "--json",
            "--no-progress",
            "--home",
            &home_str,
            "models",
            "list",
            "--all",
        ],
        "models list",
    )
    .expect("models list command");
    // The LLM-only surface lists language models only; the non-LLM fakes are
    // still registered and exercised by the run checks below, just not shown
    // here.
    assert!(
        list_json.contains("\"id\":\"mlx.fake.llm\""),
        "MLX fake LLM row present: {list_json}"
    );
    assert!(
        list_json.contains("\"modality\":\"llm\""),
        "MLX fake LLM row present: {list_json}"
    );
    assert!(
        list_json.contains("\"backend\":\"mlx\""),
        "MLX fake LLM row present: {list_json}"
    );

    let run_json = run_cli_or_fail(
        &[
            "wally",
            "--json",
            "--no-progress",
            "--home",
            &home_str,
            "run",
            "mlx.fake.llm",
            "Hello MLX",
            "--engine",
            "mlx",
            "--max-tokens",
            "4",
        ],
        "LLM",
    )
    .expect("run LLM command");
    assert!(
        run_json.contains("\"model\":\"mlx.fake.llm\""),
        "JSON response from MLX stream callback: {run_json}"
    );
    assert!(
        run_json.contains("\"response\":\"mlx-stub: Hello MLX\""),
        "JSON response from MLX stream callback: {run_json}"
    );
    {
        let state = fakes::lock_state();
        assert_eq!(state.create_count, 1);
        assert_eq!(state.initialize_count, 1);
        assert_eq!(state.stream_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_LLM as sys::rac_mlx_session_kind_t
        );
        assert_eq!(
            state.last_model_path,
            llm_dir.path().to_string_lossy(),
            "MLX LLM runtime should receive the model folder, not model.safetensors"
        );
    }

    let vlm_json = run_cli_or_fail(
        &[
            "wally",
            "--json",
            "--no-progress",
            "--home",
            &home_str,
            "run",
            "mlx.fake.vlm",
            "What is in the image?",
            "--image",
            input_image.to_str().expect("utf-8 path"),
            "--engine",
            "mlx",
            "--max-tokens",
            "4",
        ],
        "VLM",
    )
    .expect("run VLM command");
    assert!(
        vlm_json.contains("\"model\":\"mlx.fake.vlm\""),
        "JSON VLM response from MLX callback: {vlm_json}"
    );
    assert!(
        vlm_json.contains("\"response\":\"mlx-vlm-stub: What is in the image?\""),
        "JSON VLM response from MLX callback: {vlm_json}"
    );
    {
        let state = fakes::lock_state();
        assert_eq!(state.vlm_process_count, 1);
        assert_eq!(
            state.last_kind,
            sys::RAC_MLX_SESSION_KIND_VLM as sys::rac_mlx_session_kind_t
        );
        assert_eq!(
            state.last_model_path,
            vlm_dir.path().to_string_lossy(),
            "MLX VLM runtime should receive the model folder, not model.safetensors"
        );
    }

    // TEMP(llm-only cut): embed/STT/TTS are standalone top-level commands
    // (register_embed/register_stt/register_tts in the C++ source) that
    // src/app.cpp currently leaves unregistered, so the C++ reference test
    // compiles out its embed/STT/TTS CLI assertions here
    // (`#if WALLY_LLM_ONLY_CUT`) and instead shuts down and checks
    // create/initialize counts for exactly the two models `run` exercised
    // above (LLM + VLM). This port matches that active branch; flip both
    // together (`WALLY_LLM_ONLY_CUT -> 0` there, restoring the embed/STT/TTS
    // section here) when the full command surface returns.
    wally::bootstrap::shutdown();
    let state = fakes::lock_state();
    assert_eq!(
        state.create_count, 2,
        "MLX create/initialize should run once per LLM/VLM model"
    );
    assert_eq!(
        state.initialize_count, 2,
        "MLX create/initialize should run once per LLM/VLM model"
    );
}
