// Port of tests/local_harness/fake_llm.cpp. Real CLI + SDK HTTP server,
// deterministic inference only. This binary is never installed or shipped
// with Wally.
use serde_json::{json, Value};
use std::env;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};
use wally::sys;

struct Session {
    path: CString,
    ready: bool,
    context: i32,
}

fn report() -> &'static Mutex<Value> {
    static REPORT: OnceLock<Mutex<Value>> = OnceLock::new();
    REPORT.get_or_init(|| {
        Mutex::new(json!({"created": 0, "initialized": 0, "destroyed": 0, "generated": 0}))
    })
}

fn report_lock() -> std::sync::MutexGuard<'static, Value> {
    report()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn borrow_c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: callers only pass pointers the SDK documents as valid,
    // NUL-terminated C strings for the duration of the call.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn create(
    model: *const c_char,
    config: *const c_char,
    out: *mut *mut c_void,
) -> sys::rac_result_t {
    let model_path = borrow_c_string(model);
    let mut context: i32 = 16384;
    let mut config_value: Option<Value> = None;
    if !config.is_null() {
        let text = borrow_c_string(config);
        if let Ok(mut parsed) = serde_json::from_str::<Value>(&text) {
            let requested = parsed
                .get("context_length")
                .and_then(Value::as_i64)
                .unwrap_or(16384);
            context = requested.max(16384) as i32;
            if let Some(object) = parsed.as_object_mut() {
                object.insert("context_length".to_string(), json!(context));
            }
            config_value = Some(parsed);
        }
    }
    // Test-only override: every path above clamps the reported context to
    // at least 16384, which means nothing here ever exercises the harness's
    // MINIMUM_CODING_HARNESS_CONTEXT rejection (src/harness/harness.rs). A
    // scenario that needs a smaller loaded context -- to prove the harness
    // actually rejects it -- sets this instead of trying to talk the real
    // SDK into allocating less memory than it has available.
    if let Some(forced) = env::var("WALLY_TEST_LOADED_CONTEXT")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        context = forced;
    }
    {
        let mut guard = report_lock();
        if let Some(value) = config_value {
            guard["config"] = value;
        }
        let created = guard["created"].as_i64().unwrap_or(0) + 1;
        guard["created"] = json!(created);
        guard["model_path"] = json!(model_path);
    }
    let session = Box::new(Session {
        path: CString::new(model_path).unwrap_or_default(),
        ready: false,
        context,
    });
    // SAFETY: `out` is a valid, writable out-parameter the SDK provided for
    // this call.
    unsafe { *out = Box::into_raw(session) as *mut c_void };
    sys::SUCCESS
}

unsafe extern "C" fn initialize(impl_: *mut c_void, path: *const c_char) -> sys::rac_result_t {
    if env::var_os("WALLY_TEST_FAIL_LOAD").is_some() {
        return sys::RAC_ERROR_MODEL_LOAD_FAILED;
    }
    // SAFETY: `impl_` is the Session this plugin allocated in `create`; the
    // SDK keeps it alive until `destroy`.
    let session = unsafe { &mut *(impl_ as *mut Session) };
    session.path = CString::new(borrow_c_string(path)).unwrap_or_default();
    session.ready = true;
    let mut guard = report_lock();
    let initialized = guard["initialized"].as_i64().unwrap_or(0) + 1;
    guard["initialized"] = json!(initialized);
    sys::SUCCESS
}

unsafe extern "C" fn generate(
    impl_: *mut c_void,
    prompt: *const c_char,
    _options: *const sys::rac_llm_options_t,
    out_result: *mut sys::rac_llm_result_t,
) -> sys::rac_result_t {
    // SAFETY: see `initialize`.
    let session = unsafe { &*(impl_ as *const Session) };
    if !session.ready {
        return sys::RAC_ERROR_NOT_INITIALIZED;
    }
    let prompt_text = borrow_c_string(prompt);
    {
        let mut guard = report_lock();
        let generated = guard["generated"].as_i64().unwrap_or(0) + 1;
        guard["generated"] = json!(generated);
        guard["last_prompt"] = json!(prompt_text);
    }
    const REPLY: &[u8] = b"local harness reply\0";
    // SAFETY: `rac_free()` ultimately calls libc `free()`, so the text
    // buffer must come from a C-compatible allocator (matching the C++
    // fixture's `malloc`); `out_result` is a valid, writable slot the SDK
    // provided for this call.
    unsafe {
        let buffer = libc::malloc(REPLY.len()) as *mut c_char;
        if buffer.is_null() {
            return sys::RAC_ERROR_OUT_OF_MEMORY;
        }
        std::ptr::copy_nonoverlapping(REPLY.as_ptr() as *const c_char, buffer, REPLY.len());
        *out_result = std::mem::zeroed();
        (*out_result).text = buffer;
        (*out_result).prompt_tokens = 3;
        (*out_result).completion_tokens = 3;
        (*out_result).total_tokens = 6;
    }
    sys::SUCCESS
}

unsafe extern "C" fn generate_stream(
    impl_: *mut c_void,
    prompt: *const c_char,
    _options: *const sys::rac_llm_options_t,
    callback: sys::rac_llm_stream_callback_fn,
    user: *mut c_void,
) -> sys::rac_result_t {
    // SAFETY: see `initialize`.
    let session = unsafe { &*(impl_ as *const Session) };
    if !session.ready {
        return sys::RAC_ERROR_NOT_INITIALIZED;
    }
    let prompt_text = borrow_c_string(prompt);
    {
        let mut guard = report_lock();
        let generated = guard["generated"].as_i64().unwrap_or(0) + 1;
        guard["generated"] = json!(generated);
        guard["last_prompt"] = json!(prompt_text);
    }
    let Some(callback) = callback else {
        return sys::SUCCESS;
    };
    // SAFETY: `callback` is the SDK-provided function pointer for this
    // stream; both C strings are 'static and stay alive past the call.
    unsafe {
        callback(
            c"local harness reply".as_ptr(),
            sys::FALSE,
            std::ptr::null(),
            3,
            user,
        );
        callback(c"".as_ptr(), sys::TRUE, c"stop".as_ptr(), 0, user);
    }
    sys::SUCCESS
}

unsafe extern "C" fn get_info(
    impl_: *mut c_void,
    out_info: *mut sys::rac_llm_info_t,
) -> sys::rac_result_t {
    // SAFETY: see `initialize`.
    let session = unsafe { &*(impl_ as *const Session) };
    // SAFETY: `out_info` is a valid, writable slot the SDK provided for this
    // call; `session.path` outlives it since `session` is not mutated while
    // this call runs.
    unsafe {
        *out_info = std::mem::zeroed();
        (*out_info).is_ready = if session.ready { sys::TRUE } else { sys::FALSE };
        (*out_info).current_model = session.path.as_ptr();
        (*out_info).context_length = session.context;
        (*out_info).supports_streaming = sys::TRUE;
    }
    sys::SUCCESS
}

unsafe extern "C" fn ok(_impl_: *mut c_void) -> sys::rac_result_t {
    sys::SUCCESS
}

unsafe extern "C" fn destroy(impl_: *mut c_void) {
    {
        let mut guard = report_lock();
        let destroyed = guard["destroyed"].as_i64().unwrap_or(0) + 1;
        guard["destroyed"] = json!(destroyed);
    }
    if !impl_.is_null() {
        // SAFETY: `impl_` was produced by `Box::into_raw` in `create`; the
        // SDK calls `destroy` exactly once and never uses `impl_` again
        // afterward.
        unsafe { drop(Box::from_raw(impl_ as *mut Session)) };
    }
}

const FORMATS: [u32; 1] = [sys::RAC_MODEL_FORMAT_ID_GGUF];

const OPS: sys::rac_llm_service_ops = sys::rac_llm_service_ops {
    initialize: Some(initialize),
    generate: Some(generate),
    generate_stream: Some(generate_stream),
    get_info: Some(get_info),
    cancel: Some(ok),
    cleanup: Some(ok),
    destroy: Some(destroy),
    load_lora: None,
    remove_lora: None,
    clear_lora: None,
    get_lora_info: None,
    inject_system_prompt: None,
    append_context: None,
    generate_from_context: None,
    clear_context: None,
    create: Some(create),
    get_stream_token_counts: None,
    generate_chat_stream: None,
};

const ENGINE: sys::rac_engine_vtable = sys::rac_engine_vtable {
    metadata: sys::rac_engine_metadata {
        abi_version: sys::RAC_PLUGIN_API_VERSION,
        name: c"llamacpp".as_ptr(),
        display_name: c"Hermetic harness test backend".as_ptr(),
        engine_version: std::ptr::null(),
        priority: 100_000,
        capability_flags: 0,
        runtimes: std::ptr::null(),
        runtimes_count: 0,
        formats: FORMATS.as_ptr(),
        formats_count: FORMATS.len(),
    },
    capability_check: None,
    on_unload: None,
    llm_ops: &OPS,
    stt_ops: std::ptr::null(),
    tts_ops: std::ptr::null(),
    vad_ops: std::ptr::null(),
    embedding_ops: std::ptr::null(),
    vlm_ops: std::ptr::null(),
    diffusion_ops: std::ptr::null(),
    diarization_ops: std::ptr::null(),
    segmentation_ops: std::ptr::null(),
    rerank_ops: std::ptr::null(),
    image_embedding_ops: std::ptr::null(),
    ocr_ops: std::ptr::null_mut(),
    reserved_slot_5: std::ptr::null(),
    reserved_slot_6: std::ptr::null(),
    reserved_slot_7: std::ptr::null(),
    reserved_slot_8: std::ptr::null(),
    reserved_slot_9: std::ptr::null(),
};

// The environment wally's harness commands read or write; the fixture proves
// production restores every one of them once the command returns.
const WATCHED_ENV: [&str; 12] = [
    "OPENCODE_CONFIG_CONTENT",
    "OPENCLAW_CONFIG_PATH",
    "OPENCLAW_STATE_DIR",
    "CUSTOM_BASE_URL",
    "HERMES_MODEL",
    "HERMES_INFERENCE_MODEL",
    "HERMES_INFERENCE_PROVIDER",
    "RUNANYWHERE_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
];

fn main() {
    // SAFETY: takes a value argument; always safe to call.
    unsafe { sys::rac_logger_set_min_level(sys::RAC_LOG_ERROR) };
    // SAFETY: `ENGINE` and `OPS` both have 'static storage; the SDK only
    // reads through the vtable for the life of the process.
    if unsafe { sys::rac_plugin_register(&ENGINE) } != sys::SUCCESS {
        std::process::exit(90);
    }
    if let Ok(token) = env::var("WALLY_TEST_SEED_CLOUD") {
        // Exercise the production platform store: Windows uses a DPAPI blob
        // named credentials.dat, while POSIX uses a protected JSON document.
        let expires_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(0)
            + 3600;
        let credentials = wally::account::Credentials {
            console_url: wally::account::default_console_url(),
            email: "harness@example.test".to_string(),
            access_token: token,
            refresh_token: String::new(),
            expires_at,
        };
        if let Err(error) = wally::account::save(&credentials) {
            eprintln!("fixture credential setup failed: {error}");
            std::process::exit(92);
        }
    }

    // Register before CLI parsing; pre-bootstrapping here would mask --home
    // regressions in the production launch path.
    let original: Vec<(&str, Option<String>)> = WATCHED_ENV
        .iter()
        .map(|name| (*name, env::var(name).ok()))
        .collect();

    let status = wally::run_main(env::args_os().collect());

    // SAFETY: takes no arguments; always safe to call.
    let stopped = unsafe { sys::rac_server_is_running() } == sys::FALSE;
    let mut environment_restored = true;
    for (name, value) in &original {
        let current = env::var(name).ok();
        if &current != value {
            environment_restored = false;
        }
    }
    {
        let mut guard = report_lock();
        guard["stopped"] = json!(stopped);
        guard["environment_restored"] = json!(environment_restored);
    }
    if let Ok(path) = env::var("WALLY_TEST_BACKEND_REPORT") {
        let dump = report_lock().clone();
        let _ = std::fs::write(
            path,
            serde_json::to_string_pretty(&dump).unwrap_or_default(),
        );
    }
    std::process::exit(status);
}
