//! Fake MLX backend callback table + shared state, ported from the
//! FakeMlxSession / FakeMlxState / fake_* machinery in
//! tests/test_wally_mlx_e2e.cpp. Owned by the MLX e2e test file; declared
//! there with `#[path = "common/mlx_fakes.rs"]` per tests/common/mod.rs's
//! area-specific-helper convention.
#![allow(dead_code)]

use std::ffi::{c_char, c_void, CStr};
use std::sync::{Mutex, MutexGuard, OnceLock};

use wally::sys;

/// One fake backend "session" the MLX callback table hands back as an opaque
/// rac_handle_t (C++'s FakeMlxSession).
pub struct FakeMlxSession {
    pub kind: sys::rac_mlx_session_kind_t,
    pub model_id: String,
    pub model_path: String,
}

/// Call counters/observations the tests assert on (C++'s FakeMlxState).
#[derive(Default)]
pub struct FakeMlxState {
    pub create_count: i32,
    pub initialize_count: i32,
    pub llm_generate_count: i32,
    pub stream_count: i32,
    pub vlm_process_count: i32,
    pub vlm_stream_count: i32,
    pub embed_batch_count: i32,
    pub embedding_info_count: i32,
    pub stt_transcribe_count: i32,
    pub stt_stream_count: i32,
    pub stt_info_count: i32,
    pub tts_synthesize_count: i32,
    pub tts_stream_count: i32,
    pub tts_stop_count: i32,
    pub tts_info_count: i32,
    pub last_kind: sys::rac_mlx_session_kind_t,
    pub last_model_path: String,
    pub last_embed_batch_size: usize,
    pub last_audio_size: usize,
    pub last_tts_text: String,
}

/// Global fake-backend state (the fakes are `extern "C" fn` with no user_data
/// slot free for this, matching the C++ file's `g_mlx_state` global).
fn state_cell() -> &'static Mutex<FakeMlxState> {
    static STATE: OnceLock<Mutex<FakeMlxState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(FakeMlxState::default()))
}

/// Locks the shared fake-backend state (poison-tolerant: a prior panicking
/// test must not wedge every test after it).
pub fn lock_state() -> MutexGuard<'static, FakeMlxState> {
    state_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resets counters/observations to zero, mirroring the C++ file's
/// `g_mlx_state = {};` at the top of each test.
pub fn reset_state() {
    *lock_state() = FakeMlxState::default();
}

/// Serializes the two tests in this file: both touch the process-wide MLX
/// backend/plugin registry, which cargo test's parallel test threads would
/// otherwise race.
pub fn mlx_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Allocates a NUL-terminated C string with libc::malloc, matching what the
/// SDK's rac_*_result_free() functions call libc::free() on (Rust's own
/// allocator would be an ABI mismatch here). Null on allocation failure.
unsafe fn c_strdup(text: &str) -> *mut c_char {
    let len = text.len();
    // SAFETY: the returned pointer is freed with libc::free, directly or via
    // an SDK *_result_free() call, matching this libc::malloc.
    let ptr = unsafe { libc::malloc(len + 1) } as *mut u8;
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `ptr` was just allocated with `len + 1` bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), ptr, len);
        *ptr.add(len) = 0;
    }
    ptr as *mut c_char
}

/// Allocates `count` zeroed f32 slots with libc::calloc, paired with the same
/// libc::free contract as `c_strdup`. Null on allocation failure.
unsafe fn c_calloc_f32(count: usize) -> *mut f32 {
    // SAFETY: caller frees via libc::free (directly, or via an SDK
    // *_result_free() call), matching this libc::calloc.
    unsafe { libc::calloc(count, std::mem::size_of::<f32>()) as *mut f32 }
}

// ---------------------------------------------------------------------------
// Fake MLX backend callbacks (installed via rac_mlx_set_callbacks). Each one
// is handed to the SDK, so each wraps its body in catch_unwind and never lets
// a panic cross the FFI boundary.
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn fake_create(
    kind: sys::rac_mlx_session_kind_t,
    model_id: *const c_char,
    out_handle: *mut sys::rac_handle_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if out_handle.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        let model_id = if model_id.is_null() {
            String::new()
        } else {
            // SAFETY: model_id is a NUL-terminated C string owned by the
            // caller for the duration of this call.
            unsafe { CStr::from_ptr(model_id) }
                .to_string_lossy()
                .into_owned()
        };
        let session = Box::new(FakeMlxSession {
            kind,
            model_id,
            model_path: String::new(),
        });
        // SAFETY: out_handle is non-null (checked above) and valid for one
        // rac_handle_t write, per the callback contract.
        unsafe { *out_handle = Box::into_raw(session) as sys::rac_handle_t };
        let mut state = lock_state();
        state.create_count += 1;
        state.last_kind = kind;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_initialize(
    handle: sys::rac_handle_t,
    model_path: *const c_char,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if handle.is_null() || model_path.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: handle is a live FakeMlxSession returned by fake_create and
        // not yet destroyed (the callback contract forbids reuse after
        // destroy).
        let session = unsafe { &mut *(handle as *mut FakeMlxSession) };
        // SAFETY: model_path is a NUL-terminated string valid for this call.
        let path = unsafe { CStr::from_ptr(model_path) }
            .to_string_lossy()
            .into_owned();
        session.model_path = path.clone();
        let mut state = lock_state();
        state.initialize_count += 1;
        state.last_model_path = path;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_llm_generate(
    _handle: sys::rac_handle_t,
    prompt: *const c_char,
    _options: *const sys::rac_llm_options_t,
    out_result: *mut sys::rac_llm_result_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if prompt.is_null() || out_result.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: prompt is a NUL-terminated string valid for this call.
        let prompt_text = unsafe { CStr::from_ptr(prompt) }.to_string_lossy();
        let text = format!("mlx-stub: {prompt_text}");
        // SAFETY: out_result is a valid, exclusive rac_llm_result_t for the
        // duration of this call.
        let out = unsafe { &mut *out_result };
        *out = unsafe { std::mem::zeroed() };
        // SAFETY: c_strdup allocates with libc::malloc, matching
        // rac_llm_result_free's libc::free.
        out.text = unsafe { c_strdup(&text) };
        if out.text.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        out.prompt_tokens = 2;
        out.completion_tokens = 3;
        out.total_tokens = 5;
        out.total_time_ms = 7;
        out.tokens_per_second = 100.0;
        lock_state().llm_generate_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_llm_generate_stream(
    _handle: sys::rac_handle_t,
    prompt: *const c_char,
    _options: *const sys::rac_llm_options_t,
    callback: sys::rac_llm_stream_callback_fn,
    callback_user_data: *mut c_void,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        let Some(callback) = callback else {
            return sys::RAC_ERROR_NULL_POINTER;
        };
        if prompt.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        lock_state().stream_count += 1;
        // SAFETY: prompt is a NUL-terminated string valid for this call.
        let prompt_text = unsafe { CStr::from_ptr(prompt) }.to_string_lossy();
        let token = format!("mlx-stub: {prompt_text}");
        let token_c = std::ffi::CString::new(token).unwrap_or_default();
        // SAFETY: callback is the caller-supplied function pointer; token_c
        // stays alive for the duration of this call.
        let accepted =
            unsafe { callback(token_c.as_ptr(), sys::FALSE, std::ptr::null(), 1, callback_user_data) };
        if accepted != sys::TRUE {
            return sys::RAC_ERROR_STREAM_CANCELLED;
        }
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_vlm_process(
    _handle: sys::rac_handle_t,
    _image: *const sys::rac_vlm_image_t,
    prompt: *const c_char,
    _options: *const sys::rac_vlm_options_t,
    out_result: *mut sys::rac_vlm_result_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if prompt.is_null() || out_result.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: prompt is a NUL-terminated string valid for this call.
        let prompt_text = unsafe { CStr::from_ptr(prompt) }.to_string_lossy();
        let text = format!("mlx-vlm-stub: {prompt_text}");
        // SAFETY: out_result is a valid, exclusive rac_vlm_result_t for the
        // duration of this call.
        let out = unsafe { &mut *out_result };
        *out = unsafe { std::mem::zeroed() };
        out.text = unsafe { c_strdup(&text) };
        if out.text.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        out.completion_tokens = 3;
        out.total_tokens = 8;
        out.tokens_per_second = 50.0;
        lock_state().vlm_process_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_vlm_process_stream(
    _handle: sys::rac_handle_t,
    _image: *const sys::rac_vlm_image_t,
    prompt: *const c_char,
    _options: *const sys::rac_vlm_options_t,
    callback: sys::rac_vlm_stream_callback_fn,
    callback_user_data: *mut c_void,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        let Some(callback) = callback else {
            return sys::RAC_ERROR_NULL_POINTER;
        };
        if prompt.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        lock_state().vlm_stream_count += 1;
        // SAFETY: callback is the caller-supplied function pointer; prompt
        // stays valid for the duration of this call (owned by the caller, as
        // rac_vlm_stream_callback_fn documents).
        let accepted = unsafe { callback(prompt, callback_user_data) };
        if accepted == sys::TRUE {
            sys::SUCCESS
        } else {
            sys::RAC_ERROR_CANCELLED
        }
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_embed_batch(
    _handle: sys::rac_handle_t,
    texts: *const *const c_char,
    num_texts: usize,
    _options: *const sys::rac_embeddings_options_t,
    out_result: *mut sys::rac_embeddings_result_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if texts.is_null() || out_result.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        let out = unsafe { &mut *out_result };
        *out = unsafe { std::mem::zeroed() };
        out.num_embeddings = num_texts;
        out.dimension = 2;
        // SAFETY: freed by rac_embeddings_result_free (libc::free), matching
        // this libc::calloc.
        let embeddings = unsafe {
            libc::calloc(
                num_texts,
                std::mem::size_of::<sys::rac_embedding_vector_t>(),
            )
        } as *mut sys::rac_embedding_vector_t;
        if embeddings.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        out.embeddings = embeddings;
        for i in 0..num_texts {
            // SAFETY: texts has num_texts entries per the callback contract;
            // i < num_texts.
            let text_ptr = unsafe { *texts.add(i) };
            // SAFETY: a non-null text_ptr is a NUL-terminated C string; only
            // the first byte is read to check for non-emptiness.
            let has_text = !text_ptr.is_null() && unsafe { *text_ptr } != 0;
            // SAFETY: embeddings[i] is within the just-allocated num_texts
            // array.
            let slot = unsafe { &mut *embeddings.add(i) };
            slot.dimension = 2;
            // SAFETY: freed by rac_embeddings_result_free, matching calloc.
            let data = unsafe { c_calloc_f32(2) };
            if data.is_null() {
                // SAFETY: out_result now owns a partially-filled embeddings
                // array (earlier slots have real data pointers, this and any
                // later slots are still zeroed); rac_embeddings_result_free
                // frees every allocated piece and tolerates NULL data.
                unsafe { sys::rac_embeddings_result_free(out_result) };
                return sys::RAC_ERROR_OUT_OF_MEMORY;
            }
            // SAFETY: data was just calloc'd for 2 f32 slots.
            unsafe {
                *data = if has_text { 1.0 } else { 0.0 };
                *data.add(1) = 0.5;
            }
            slot.data = data;
        }
        out.total_tokens = num_texts as i32;
        let mut state = lock_state();
        state.embed_batch_count += 1;
        state.last_embed_batch_size = num_texts;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_embedding_info(
    _handle: sys::rac_handle_t,
    out_info: *mut sys::rac_embeddings_info_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if out_info.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: out_info is a valid, exclusive rac_embeddings_info_t for
        // the duration of this call.
        let out = unsafe { &mut *out_info };
        *out = unsafe { std::mem::zeroed() };
        out.is_ready = sys::TRUE;
        out.dimension = 2;
        out.max_tokens = 512;
        lock_state().embedding_info_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_stt_transcribe(
    _handle: sys::rac_handle_t,
    audio_data: *const c_void,
    audio_size: usize,
    _options: *const sys::rac_stt_options_t,
    out_result: *mut sys::rac_stt_result_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if audio_data.is_null() || audio_size == 0 || out_result.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: out_result is a valid, exclusive rac_stt_result_t for the
        // duration of this call.
        let out = unsafe { &mut *out_result };
        *out = unsafe { std::mem::zeroed() };
        let text = format!("mlx-stt-stub: {audio_size} bytes");
        out.text = unsafe { c_strdup(&text) };
        out.detected_language = unsafe { c_strdup("en") };
        out.confidence = 0.95;
        out.processing_time_ms = 11;
        let mut state = lock_state();
        state.stt_transcribe_count += 1;
        state.last_audio_size = audio_size;
        if out.text.is_null() || out.detected_language.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_stt_transcribe_stream(
    _handle: sys::rac_handle_t,
    audio_data: *const c_void,
    audio_size: usize,
    _options: *const sys::rac_stt_options_t,
    callback: sys::rac_stt_stream_callback_t,
    callback_user_data: *mut c_void,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        let Some(callback) = callback else {
            return sys::RAC_ERROR_NULL_POINTER;
        };
        if audio_data.is_null() || audio_size == 0 {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        lock_state().stt_stream_count += 1;
        let partial = c"mlx-stt-partial";
        let finalt = c"mlx-stt-final";
        // SAFETY: callback is the caller-supplied function pointer; the two
        // string literals are 'static.
        unsafe {
            callback(partial.as_ptr(), sys::FALSE, callback_user_data);
            callback(finalt.as_ptr(), sys::TRUE, callback_user_data);
        }
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_stt_info(
    _handle: sys::rac_handle_t,
    out_info: *mut sys::rac_stt_info_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if out_info.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: out_info is a valid, exclusive rac_stt_info_t for the
        // duration of this call.
        let out = unsafe { &mut *out_info };
        *out = unsafe { std::mem::zeroed() };
        out.is_ready = sys::TRUE;
        // SAFETY: 'static C string literal; current_model is a borrowed
        // pointer per rac_stt_info's doc (no ownership transfer, unlike
        // text/detected_language on rac_stt_result).
        out.current_model = c"mlx.fake.stt".as_ptr();
        out.supports_streaming = sys::TRUE;
        lock_state().stt_info_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_tts_synthesize(
    _handle: sys::rac_handle_t,
    text: *const c_char,
    _options: *const sys::rac_tts_options_t,
    out_result: *mut sys::rac_tts_result_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if text.is_null() || out_result.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        const SAMPLE_COUNT: usize = 8;
        // SAFETY: freed by rac_tts_result_free (libc::free), matching calloc.
        let samples = unsafe { c_calloc_f32(SAMPLE_COUNT) };
        if samples.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        for i in 0..SAMPLE_COUNT {
            let value: f32 = if i % 2 == 0 { 0.25 } else { -0.25 };
            // SAFETY: samples was just calloc'd for SAMPLE_COUNT f32 slots.
            unsafe { *samples.add(i) = value };
        }
        // SAFETY: out_result is a valid, exclusive rac_tts_result_t for the
        // duration of this call.
        let out = unsafe { &mut *out_result };
        *out = unsafe { std::mem::zeroed() };
        out.audio_data = samples as *mut c_void;
        out.audio_size = SAMPLE_COUNT * std::mem::size_of::<f32>();
        out.audio_format = sys::RAC_AUDIO_FORMAT_PCM;
        out.sample_rate = 22050;
        out.duration_ms = 1;
        out.processing_time_ms = 13;
        let mut state = lock_state();
        state.tts_synthesize_count += 1;
        // SAFETY: text is a NUL-terminated string valid for this call.
        state.last_tts_text = unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned();
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_tts_synthesize_stream(
    _handle: sys::rac_handle_t,
    text: *const c_char,
    _options: *const sys::rac_tts_options_t,
    callback: sys::rac_tts_stream_callback_t,
    callback_user_data: *mut c_void,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        let Some(callback) = callback else {
            return sys::RAC_ERROR_NULL_POINTER;
        };
        if text.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        lock_state().tts_stream_count += 1;
        let samples: [f32; 2] = [0.1, -0.1];
        // SAFETY: callback is the caller-supplied function pointer; samples
        // stays alive for the duration of this call.
        unsafe {
            callback(
                samples.as_ptr() as *const c_void,
                std::mem::size_of_val(&samples),
                callback_user_data,
            )
        };
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_tts_stop(
    _handle: sys::rac_handle_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        lock_state().tts_stop_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_tts_info(
    _handle: sys::rac_handle_t,
    out_info: *mut sys::rac_tts_info_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    let result = std::panic::catch_unwind(|| {
        if out_info.is_null() {
            return sys::RAC_ERROR_NULL_POINTER;
        }
        // SAFETY: out_info is a valid, exclusive rac_tts_info_t for the
        // duration of this call.
        let out = unsafe { &mut *out_info };
        *out = unsafe { std::mem::zeroed() };
        out.is_ready = sys::TRUE;
        out.is_synthesizing = sys::FALSE;
        lock_state().tts_info_count += 1;
        sys::SUCCESS
    });
    result.unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_cancel(
    _handle: sys::rac_handle_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    std::panic::catch_unwind(|| sys::SUCCESS).unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_cleanup(
    _handle: sys::rac_handle_t,
    _user_data: *mut c_void,
) -> sys::rac_result_t {
    std::panic::catch_unwind(|| sys::SUCCESS).unwrap_or(sys::RAC_ERROR_INTERNAL)
}

pub unsafe extern "C" fn fake_destroy(handle: sys::rac_handle_t, _user_data: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        if !handle.is_null() {
            // SAFETY: handle was returned by fake_create's Box::into_raw and
            // is being destroyed exactly once, per the callback contract.
            unsafe { drop(Box::from_raw(handle as *mut FakeMlxSession)) };
        }
    });
}

/// Installs the fake callback table (test_wally_mlx_e2e.cpp's
/// install_fake_mlx_callbacks). The diarization slots stay `None`: the kit's
/// callback table grew them after this reference test was written, and this
/// port exercises the same five modalities the C++ file does.
pub fn install_fake_mlx_callbacks() -> bool {
    // SAFETY: every function field below is a real extern "C" fn matching
    // the type the struct declares (verified against src/sys/bindings.rs);
    // zeroing first leaves the unset diarization slots as valid `None`s.
    let mut callbacks: sys::rac_mlx_callbacks_t = unsafe { std::mem::zeroed() };
    callbacks.struct_size = std::mem::size_of::<sys::rac_mlx_callbacks_t>() as u32;
    callbacks.create = Some(fake_create);
    callbacks.initialize = Some(fake_initialize);
    callbacks.llm_generate = Some(fake_llm_generate);
    callbacks.llm_generate_stream = Some(fake_llm_generate_stream);
    callbacks.vlm_process = Some(fake_vlm_process);
    callbacks.vlm_process_stream = Some(fake_vlm_process_stream);
    callbacks.embed_batch = Some(fake_embed_batch);
    callbacks.embedding_info = Some(fake_embedding_info);
    callbacks.stt_transcribe = Some(fake_stt_transcribe);
    callbacks.stt_transcribe_stream = Some(fake_stt_transcribe_stream);
    callbacks.stt_info = Some(fake_stt_info);
    callbacks.tts_synthesize = Some(fake_tts_synthesize);
    callbacks.tts_synthesize_stream = Some(fake_tts_synthesize_stream);
    callbacks.tts_stop = Some(fake_tts_stop);
    callbacks.tts_info = Some(fake_tts_info);
    callbacks.cancel = Some(fake_cancel);
    callbacks.cleanup = Some(fake_cleanup);
    callbacks.destroy = Some(fake_destroy);
    // SAFETY: `callbacks` is fully initialized; rac_mlx_set_callbacks copies
    // it, so the local does not need to outlive this call.
    unsafe { sys::rac_mlx_set_callbacks(&callbacks) == sys::SUCCESS }
}

/// Registers the MLX backend, tolerating "already registered" from an
/// earlier test in this binary (test_wally_mlx_e2e.cpp's
/// register_mlx_backend_or_fail).
pub fn register_mlx_backend_or_fail() -> Result<(), String> {
    // SAFETY: FFI call with no arguments; always sound to call.
    let rc = unsafe { sys::rac_backend_mlx_register() };
    if rc == sys::SUCCESS || rc == sys::RAC_ERROR_MODULE_ALREADY_REGISTERED {
        Ok(())
    } else {
        Err(format!("rac_backend_mlx_register: {rc}"))
    }
}

/// Runs rac_backend_mlx_unregister() on drop, even if a test assertion
/// panics mid-body. RAII replacement for the C++ file's repeated
/// `rac_backend_mlx_unregister(); return result;` cleanup branches ahead of
/// every early return.
pub struct MlxBackendGuard;

impl Drop for MlxBackendGuard {
    fn drop(&mut self) {
        // SAFETY: FFI call with no arguments; always sound to call, even if
        // the backend was never successfully registered.
        unsafe {
            sys::rac_backend_mlx_unregister();
        }
    }
}

// ---------------------------------------------------------------------------
// Driver callbacks the tests pass into the MLX engine vtable's stream ops
// (append_llm_token_callback / append_token_callback / append_stt_callback /
// count_tts_chunk_callback in the C++ file). These are handed to the SDK too,
// so they get the same catch_unwind treatment.
// ---------------------------------------------------------------------------

pub unsafe extern "C" fn append_llm_token_callback(
    token: *const c_char,
    is_final: sys::rac_bool_t,
    _finish_reason: *const c_char,
    _tokens_in_delta: i32,
    user_data: *mut c_void,
) -> sys::rac_bool_t {
    let result = std::panic::catch_unwind(|| {
        if is_final == sys::TRUE {
            return sys::TRUE;
        }
        if !token.is_null() && !user_data.is_null() {
            // SAFETY: user_data points to the String the test passed as
            // callback_user_data and outlives this call.
            let out = unsafe { &mut *(user_data as *mut String) };
            // SAFETY: token is a NUL-terminated string valid for this call.
            out.push_str(&unsafe { CStr::from_ptr(token) }.to_string_lossy());
        }
        sys::TRUE
    });
    result.unwrap_or(sys::FALSE)
}

pub unsafe extern "C" fn append_token_callback(
    token: *const c_char,
    user_data: *mut c_void,
) -> sys::rac_bool_t {
    let result = std::panic::catch_unwind(|| {
        if !token.is_null() && !user_data.is_null() {
            // SAFETY: user_data points to the String the test passed as
            // callback_user_data and outlives this call.
            let out = unsafe { &mut *(user_data as *mut String) };
            // SAFETY: token is a NUL-terminated string valid for this call.
            out.push_str(&unsafe { CStr::from_ptr(token) }.to_string_lossy());
        }
        sys::TRUE
    });
    result.unwrap_or(sys::FALSE)
}

pub unsafe extern "C" fn append_stt_callback(
    text: *const c_char,
    is_final: sys::rac_bool_t,
    user_data: *mut c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if text.is_null() || user_data.is_null() {
            return;
        }
        // SAFETY: user_data points to the String the test passed as
        // callback_user_data and outlives this call.
        let out = unsafe { &mut *(user_data as *mut String) };
        if !out.is_empty() {
            out.push('|');
        }
        out.push_str(if is_final == sys::TRUE {
            "final:"
        } else {
            "partial:"
        });
        // SAFETY: text is a NUL-terminated string valid for this call.
        out.push_str(&unsafe { CStr::from_ptr(text) }.to_string_lossy());
    });
}

pub unsafe extern "C" fn count_tts_chunk_callback(
    _audio: *const c_void,
    audio_size: usize,
    user_data: *mut c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if !user_data.is_null() {
            // SAFETY: user_data points to the usize the test passed as
            // callback_user_data and outlives this call.
            let total = unsafe { &mut *(user_data as *mut usize) };
            *total += audio_size;
        }
    });
}
