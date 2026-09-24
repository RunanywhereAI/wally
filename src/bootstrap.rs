//! One-call SDK bring-up for every wally command (port of src/bootstrap.cpp).
//!
//! Mirrors the canonical bootstrap proven by the commons real-inference tests
//! with real desktop I/O:
//!
//!   desktop adapter → rac_model_paths_set_base_dir → rac_init →
//!   desktop HTTP transport → backend registration → catalog + discovery
//!
//! Commands call bootstrap() exactly once; it is idempotent within a process.
//! Every callback handed
//! to the SDK must be `'static`, thread-safe and panic-free across the ABI.

use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::Mutex;

use prost::Message;

use crate::catalog;
use crate::device_info;
use crate::io::output;
use crate::io::proto::{self, v1};
use crate::sys;
use crate::util;

/// Global flags shared by all subcommands (parsed in app.rs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalOptions {
    pub json: bool,
    pub verbose: bool,
    pub quiet: bool,
    pub no_progress: bool,
    /// --no-color flag; the actual ANSI decision is made in app::run() before
    /// parsing, see cli_formatter.
    pub no_color: bool,
    /// --home flag
    pub home_override: String,
    /// Control-plane connection, read from RUNANYWHERE_ENVIRONMENT /
    /// RUNANYWHERE_BASE_URL / RUNANYWHERE_API_KEY by resolve_connection().
    /// development: keyless OSS → staging backend (baked URL or base URL).
    /// production: API key + https URL.
    pub environment: String,
    pub base_url: String,
    pub api_key: String,
}

/// Validated control-plane connection resolved from GlobalOptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub environment: sys::rac_environment_t,
    pub base_url: String,
    pub api_key: String,
}

impl Default for Connection {
    fn default() -> Self {
        Connection {
            environment: sys::RAC_ENV_DEVELOPMENT,
            base_url: String::new(),
            api_key: String::new(),
        }
    }
}

/// Resolve + validate the connection client-side (before any network call).
/// On failure returns RAC_ERROR_INVALID_CONFIGURATION with an actionable message.
pub fn resolve_connection(
    options: &GlobalOptions,
) -> Result<Connection, (sys::rac_result_t, String)> {
    // Prefer explicit --environment; otherwise CLI11 / getenv via
    // RUNANYWHERE_ENVIRONMENT (the single canonical name on the cross-fit line).
    let mut environment_name = options.environment.clone();
    if environment_name.is_empty() {
        environment_name = first_env_value(&["RUNANYWHERE_ENVIRONMENT"]);
    }
    let mut base_url = options.base_url.clone();
    if base_url.is_empty() {
        base_url = first_env_value(&["RUNANYWHERE_BASE_URL"]);
    }
    let mut api_key = options.api_key.clone();
    if api_key.is_empty() {
        api_key = first_env_value(&["RUNANYWHERE_API_KEY"]);
    }

    let environment = match parse_environment_name(&environment_name) {
        Some(environment) => environment,
        None => {
            return Err((
                sys::RAC_ERROR_INVALID_CONFIGURATION,
                format!(
                    "invalid --environment '{environment_name}' (expected development or production)"
                ),
            ));
        }
    };

    let connection = Connection {
        environment,
        base_url,
        api_key,
    };

    // Development (keyless OSS): optional --base-url (else baked staging backend
    // URL). API key is optional and usually omitted.
    // Production: API key + https base URL required (validators enforce).
    let key_c = (!connection.api_key.is_empty())
        .then(|| CString::new(connection.api_key.clone()).unwrap_or_default());
    // SAFETY: `key_c`'s pointer, when present, is a valid NUL-terminated
    // string for this call; NULL is this function's documented "no key"
    // contract.
    let key_rc = unsafe {
        sys::rac_validate_api_key(
            key_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            connection.environment,
        )
    };
    if key_rc != sys::RAC_VALIDATION_OK {
        // SAFETY: takes a value argument; returns a 'static message string.
        let message = unsafe { cstr_or_empty(sys::rac_validation_error_message(key_rc)) };
        return Err((
            sys::RAC_ERROR_INVALID_CONFIGURATION,
            format!("{message} (--api-key / RUNANYWHERE_API_KEY)"),
        ));
    }

    let url_c = (!connection.base_url.is_empty())
        .then(|| CString::new(connection.base_url.clone()).unwrap_or_default());
    // SAFETY: see above.
    let url_rc = unsafe {
        sys::rac_validate_base_url(
            url_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            connection.environment,
        )
    };
    if url_rc != sys::RAC_VALIDATION_OK {
        // SAFETY: takes a value argument; returns a 'static message string.
        let message = unsafe { cstr_or_empty(sys::rac_validation_error_message(url_rc)) };
        return Err((
            sys::RAC_ERROR_INVALID_CONFIGURATION,
            format!("{message} (--base-url / RUNANYWHERE_BASE_URL)"),
        ));
    }

    Ok(connection)
}

/// Resolved environment after bootstrap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bootstrapped {
    /// RunAnywhere home (storage base dir)
    pub home: String,
    /// commons-derived models directory
    pub models_dir: String,
}

// -----------------------------------------------------------------------------
// Process-lifetime state (mirrors bootstrap.cpp's anonymous-namespace globals)
// -----------------------------------------------------------------------------

/// `rac_init` requires the adapter pointer to stay valid until `rac_shutdown`,
/// so this lives at static storage exactly like C++'s `rac_platform_adapter_t
/// g_adapter{};`.
struct AdapterCell(std::cell::UnsafeCell<sys::rac_platform_adapter_t>);

// SAFETY: every write to the cell happens inside the one-time init block
// below, itself serialized by `G_BOOTSTRAPPED`'s mutex; after that the SDK
// only reads through the pointer handed to `rac_init` (the adapter's own
// callback slots are populated by the desktop kit, which documents them as
// safe to invoke from any thread).
unsafe impl Sync for AdapterCell {}

const fn zeroed_adapter() -> sys::rac_platform_adapter_t {
    // SAFETY: `rac_platform_adapter_t` is a `repr(C)` struct of integers and
    // function pointers; an all-zero bit pattern is a valid value, matching
    // C++'s value-initialized `rac_platform_adapter_t g_adapter{};`.
    unsafe { std::mem::MaybeUninit::zeroed().assume_init() }
}

static G_ADAPTER: AdapterCell = AdapterCell(std::cell::UnsafeCell::new(zeroed_adapter()));

/// Guards the one-time init block; also doubles as C++'s `bool g_bootstrapped`.
static G_BOOTSTRAPPED: Mutex<bool> = Mutex::new(false);

/// Owns the telemetry manager for the process lifetime so the terminal flush
/// in `shutdown()` can deliver through the HTTP callback before teardown.
static G_TELEMETRY_MANAGER: AtomicPtr<sys::rac_telemetry_manager_t> =
    AtomicPtr::new(std::ptr::null_mut());

// -----------------------------------------------------------------------------
// Helpers (private, mirror bootstrap.cpp's anonymous-namespace functions)
// -----------------------------------------------------------------------------

fn log_level_for(options: &GlobalOptions) -> sys::rac_log_level_t {
    if options.verbose {
        return sys::RAC_LOG_DEBUG;
    }
    // Quiet by default: SDK internals only surface at ERROR.
    // wally prints its own user-facing status/progress lines on stderr.
    sys::RAC_LOG_ERROR
}

// ggml/llama.cpp prints its backend and Metal init straight to stderr, outside
// rac_logger's gate, so --quiet never reached it. Route each line back through
// the logger at ggml's own level: the default ERROR floor and --quiet then
// drop it like any other SDK log, and --verbose still shows it. Only compiled
// where llama.cpp is linked.
//
// SAFETY: matches `rac_llamacpp_log_callback_fn` exactly; ggml may call this
// from any backend/logging thread, so the whole body is wrapped in
// `catch_unwind`.
#[cfg(wally_has_llamacpp)]
unsafe extern "C" fn route_ggml_log(
    level: sys::rac_log_level_t,
    message: *const std::os::raw::c_char,
    _user_data: *mut c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if message.is_null() {
            return;
        }
        // SAFETY: takes no arguments; always safe to call.
        let min_level = unsafe { sys::rac_logger_get_min_level() };
        if level < min_level {
            return;
        }
        // SAFETY: `category`/`format` are 'static NUL-terminated strings;
        // `message` was checked non-null above and is NUL-terminated per the
        // ggml log callback's documented contract for the duration of this
        // call.
        unsafe {
            sys::rac_logger_logf(
                level,
                c"LLM.LlamaCpp.GGML".as_ptr(),
                std::ptr::null(),
                c"%s".as_ptr(),
                message,
            )
        };
    });
}

fn first_env_value(keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = util::getenv(key) {
            return value;
        }
    }
    String::new()
}

fn normalize_locale(locale: &str) -> String {
    let mut locale = locale.to_string();
    if let Some(encoding) = locale.find('.') {
        locale.truncate(encoding);
    }
    if let Some(modifier) = locale.find('@') {
        locale.truncate(modifier);
    }
    if locale.is_empty() || locale == "C" || locale == "POSIX" {
        return String::new();
    }
    locale.replace('_', "-")
}

fn detect_locale() -> String {
    normalize_locale(&first_env_value(&["LC_ALL", "LC_MESSAGES", "LANG"]))
}

fn strip_timezone_prefix(path: &str) -> String {
    const PREFIXES: [&str; 3] = [
        "/usr/share/zoneinfo/",
        "/var/db/timezone/zoneinfo/",
        "/usr/share/lib/zoneinfo/",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = path.strip_prefix(prefix) {
            return rest.to_string();
        }
    }
    const MARKER: &str = "zoneinfo/";
    if let Some(pos) = path.find(MARKER) {
        return path[pos + MARKER.len()..].to_string();
    }
    String::new()
}

/// The `/etc/localtime` symlink target, stripped of its zoneinfo prefix. The
/// C++ original reads the link with a raw `readlink(2)` call into a stack
/// buffer; `std::fs::read_link` is the safe std equivalent of that exact
/// syscall (same target string, no unsafe/libc needed).
#[cfg(not(windows))]
fn timezone_from_localtime_link() -> String {
    match std::fs::read_link("/etc/localtime") {
        Ok(target) => strip_timezone_prefix(&target.to_string_lossy()),
        Err(_) => String::new(),
    }
}

#[cfg(windows)]
fn timezone_from_localtime_link() -> String {
    String::new()
}

fn detect_timezone() -> String {
    let tz = first_env_value(&["TZ"]);
    if !tz.is_empty() {
        return tz.strip_prefix(':').unwrap_or(&tz).to_string();
    }
    timezone_from_localtime_link()
}

fn desktop_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(windows) {
        "windows"
    } else {
        "desktop"
    }
}

fn parse_environment_name(name: &str) -> Option<sys::rac_environment_t> {
    if name.is_empty() || name == "dev" || name == "development" {
        return Some(sys::RAC_ENV_DEVELOPMENT);
    }
    if name == "prod" || name == "production" {
        return Some(sys::RAC_ENV_PRODUCTION);
    }
    None
}

// SdkInitEnvironment is gone: SdkInitPhase1Request.environment now takes
// model_types.proto's SDKEnvironment directly (the single environment
// vocabulary across the whole IDL).
fn proto_environment_from_rac(env: sys::rac_environment_t) -> v1::SdkEnvironment {
    // SAFETY: takes a value argument; always safe to call.
    let normalized = unsafe { sys::rac_env_normalize(env) };
    if normalized == sys::RAC_ENV_PRODUCTION {
        v1::SdkEnvironment::Production
    } else {
        v1::SdkEnvironment::Development
    }
}

/// A NUL-terminated `*const c_char` (possibly NULL) copied into an owned Rust
/// `String` ("" when NULL) before the pointer's validity window ends.
///
/// # Safety
/// `ptr` must be either NULL or a valid, NUL-terminated string pointer for
/// the duration of this call.
unsafe fn cstr_or_empty(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: caller guarantees `ptr` is non-null and NUL-terminated.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

fn initialize_sdk_metadata(connection: &Connection) {
    let mut device_id_buf =
        [0 as std::os::raw::c_char; sys::RAC_DEVICE_ID_BUFFER_MIN_SIZE as usize];
    // SAFETY: `device_id_buf` is a correctly-sized out buffer per the
    // documented `>= RAC_DEVICE_ID_BUFFER_MIN_SIZE` contract.
    let device_rc = unsafe {
        sys::rac_device_get_or_create_persistent_id(device_id_buf.as_mut_ptr(), device_id_buf.len())
    };
    let device_id = if device_rc == sys::SUCCESS {
        // SAFETY: populated and NUL-terminated by the call above on success.
        unsafe { CStr::from_ptr(device_id_buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        output::status_line(&format!(
            "warning: device identity unavailable: {}",
            output::describe_result(device_rc)
        ));
        String::new()
    };

    let locale = detect_locale();
    let timezone = detect_timezone();

    // Mirror rac_sdk_init_phase1_proto's step order: runtime state first (the
    // auth / device-registration / telemetry paths read env + credentials from
    // rac_state), then the copied SDK configuration + client info.
    // Development fills the baked staging backend URL when base_url is empty.
    let mut effective_base_url = connection.base_url.clone();
    if connection.environment == sys::RAC_ENV_DEVELOPMENT && effective_base_url.is_empty() {
        // SAFETY: takes no arguments; returns a 'static baked string or NULL.
        let baked = unsafe { sys::rac_dev_config_get_staging_base_url() };
        // SAFETY: `baked` may be NULL, which is this function's documented
        // contract.
        if unsafe { sys::rac_dev_config_is_usable_http_url(baked) } {
            // SAFETY: just confirmed non-null/usable above.
            effective_base_url = unsafe { cstr_or_empty(baked) };
        }
    }

    let api_key_c = CString::new(connection.api_key.clone()).unwrap_or_default();
    let base_url_c = CString::new(effective_base_url.clone()).unwrap_or_default();
    let device_id_c = CString::new(device_id.clone()).unwrap_or_default();
    // SAFETY: every pointer is a valid NUL-terminated string for the duration
    // of this call; commons copies what it needs internally.
    let state_rc = unsafe {
        sys::rac_state_initialize(
            connection.environment,
            api_key_c.as_ptr(),
            base_url_c.as_ptr(),
            device_id_c.as_ptr(),
        )
    };
    if state_rc != sys::SUCCESS {
        output::status_line(&format!(
            "warning: SDK state init failed: {}",
            output::describe_result(state_rc)
        ));
    }

    let platform_c = CString::new(desktop_platform()).unwrap_or_default();
    let sdk_version_c = CString::new(crate::WALLY_VERSION).unwrap_or_default();
    let locale_c = (!locale.is_empty()).then(|| CString::new(locale.clone()).unwrap_or_default());
    let timezone_c =
        (!timezone.is_empty()).then(|| CString::new(timezone.clone()).unwrap_or_default());

    let sdk_config = sys::rac_sdk_config_t {
        environment: connection.environment,
        api_key: api_key_c.as_ptr(),
        base_url: base_url_c.as_ptr(),
        device_id: device_id_c.as_ptr(),
        platform: platform_c.as_ptr(),
        sdk_version: sdk_version_c.as_ptr(),
        client_info: sys::rac_client_info_t {
            sdk_binding: c"cli".as_ptr(),
            app_identifier: c"ai.runanywhere.wally".as_ptr(),
            app_name: c"RunAnywhere CLI".as_ptr(),
            app_version: sdk_version_c.as_ptr(),
            app_build: std::ptr::null(),
            locale: locale_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            timezone: timezone_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        },
    };
    // SAFETY: every field pointer above is backed by a CString or 'static C
    // string literal that outlives this call; rac_sdk_init copies what it
    // needs internally per its documented contract.
    let config_rc = unsafe { sys::rac_sdk_init(&sdk_config) };
    if config_rc != sys::RAC_VALIDATION_OK {
        // SAFETY: takes a value argument; returns a 'static message string.
        let message = unsafe { cstr_or_empty(sys::rac_validation_error_message(config_rc)) };
        output::status_line(&format!("warning: SDK metadata init failed: {message}"));
    }
}

/// Best-effort append of the exact outgoing telemetry JSON to
/// `$WALLY_TELEMETRY_DUMP`, for inspecting a malformed offset. Mirrors the
/// C++ debug hook byte-for-byte; failures (missing env var, unwritable path)
/// are silently ignored, exactly like the original's `if (FILE *fp = ...)`.
fn dump_telemetry_body(json_bytes: &[u8]) {
    let Some(dump_path) = util::getenv("WALLY_TELEMETRY_DUMP") else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&dump_path)
    {
        use std::io::Write as _;
        let _ = file.write_all(json_bytes);
        let _ = file.write_all(b"\n");
    }
}

fn telemetry_http_complete(
    manager: *mut sys::rac_telemetry_manager_t,
    ok: bool,
    body: *const std::os::raw::c_char,
    error: *const std::os::raw::c_char,
) {
    if manager.is_null() {
        return;
    }
    // SAFETY: `manager` was just checked non-null; `body`/`error` are either
    // NULL or valid NUL-terminated strings for the duration of this call.
    unsafe {
        sys::rac_telemetry_manager_http_complete(
            manager,
            if ok { sys::TRUE } else { sys::FALSE },
            body,
            error,
        )
    };
}

// Delivers a queued telemetry batch over the desktop HTTP transport. Wired via
// rac_telemetry_manager_set_http_callback (user_data = the manager) so the
// outcome is reported back through rac_telemetry_manager_http_complete. Mirrors
// the control-plane POST performed by commons' auth path.
//
// SAFETY: matches `rac_telemetry_http_callback_t` exactly; the SDK may call
// this from any thread while flushing a batch, so the whole body is wrapped in
// `catch_unwind`. `user_data` is the same `*mut rac_telemetry_manager_t`
// handle bootstrap() created and keeps alive (in `G_TELEMETRY_MANAGER`) until
// `shutdown()` destroys it.
unsafe extern "C" fn wally_telemetry_http_callback(
    user_data: *mut c_void,
    endpoint: *const std::os::raw::c_char,
    json_body: *const std::os::raw::c_char,
    json_length: usize,
    requires_auth: sys::rac_bool_t,
) {
    let _ = std::panic::catch_unwind(|| {
        let manager = user_data as *mut sys::rac_telemetry_manager_t;

        // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
        // string or NULL, valid for this call.
        let base_url = unsafe { cstr_or_empty(sys::rac_state_get_base_url()) };
        // SAFETY: takes no arguments; always safe to call.
        let transport_registered = unsafe { sys::rac_http_transport_is_registered() };
        if base_url.is_empty() || transport_registered != sys::TRUE {
            telemetry_http_complete(
                manager,
                false,
                std::ptr::null(),
                c"telemetry transport unavailable".as_ptr(),
            );
            return;
        }

        let mut url_buf = [0 as std::os::raw::c_char; 2048];
        let base_url_c = CString::new(base_url).unwrap_or_default();
        // SAFETY: `base_url_c` is a valid NUL-terminated string; `endpoint` is
        // the caller-supplied NUL-terminated endpoint per this callback's
        // documented contract; `url_buf` is a correctly-sized out buffer.
        let written = unsafe {
            sys::rac_build_url(
                base_url_c.as_ptr(),
                endpoint,
                url_buf.as_mut_ptr(),
                url_buf.len(),
            )
        };
        if written < 0 {
            telemetry_http_complete(
                manager,
                false,
                std::ptr::null(),
                c"telemetry URL build failed".as_ptr(),
            );
            return;
        }

        let mut headers: Vec<sys::rac_http_header_kv_t> = Vec::new();
        let mut defaults_ptr: *const sys::rac_http_header_kv_t = std::ptr::null();
        let mut default_count: usize = 0;
        // SAFETY: out-params are valid stack locals.
        if unsafe { sys::rac_http_default_headers(&mut defaults_ptr, &mut default_count) }
            == sys::SUCCESS
            && !defaults_ptr.is_null()
        {
            // SAFETY: commons guarantees `defaults_ptr` describes
            // `default_count` valid, static-lifetime entries.
            let defaults = unsafe { std::slice::from_raw_parts(defaults_ptr, default_count) };
            headers.extend_from_slice(defaults);
        }
        let mut auth_value = CString::default();
        if requires_auth == sys::TRUE {
            // SAFETY: takes no arguments; returns a commons-owned
            // NUL-terminated string or NULL, valid for this call.
            let token_ptr = unsafe { sys::rac_auth_get_access_token() };
            if !token_ptr.is_null() {
                // SAFETY: non-null, NUL-terminated per the function's
                // contract.
                let token = unsafe { CStr::from_ptr(token_ptr) }.to_string_lossy();
                if !token.is_empty() {
                    auth_value = CString::new(format!("Bearer {token}")).unwrap_or_default();
                }
            }
        }
        if !auth_value.as_bytes().is_empty() {
            headers.push(sys::rac_http_header_kv_t {
                name: c"Authorization".as_ptr(),
                value: auth_value.as_ptr(),
            });
        }

        let mut client: *mut sys::rac_http_client_t = std::ptr::null_mut();
        // SAFETY: `&mut client` is a valid out-param.
        let rc = unsafe { sys::rac_http_client_create(&mut client) };
        if rc != sys::SUCCESS {
            telemetry_http_complete(
                manager,
                false,
                std::ptr::null(),
                c"telemetry client create failed".as_ptr(),
            );
            return;
        }

        // SAFETY: takes no arguments; always safe to call.
        let environment = unsafe { sys::rac_state_get_environment() };
        let request = sys::rac_http_request_t {
            method: c"POST".as_ptr(),
            url: url_buf.as_ptr(),
            headers: if headers.is_empty() {
                std::ptr::null()
            } else {
                headers.as_ptr()
            },
            header_count: headers.len(),
            body_bytes: json_body as *const u8,
            body_len: json_length,
            // SAFETY: takes an environment enum; always safe to call.
            timeout_ms: unsafe { sys::rac_env_default_http_timeout_ms(environment) },
            follow_redirects: sys::FALSE,
            expected_checksum_hex: std::ptr::null(),
        };

        let mut response: sys::rac_http_response_t = unsafe { std::mem::zeroed() };
        // SAFETY: `client` is a freshly created, valid handle; `request` is
        // built above with every pointer alive for this call (including
        // `json_body`, which the callback contract guarantees is valid for
        // `json_length` bytes for the duration of this call); `response` is a
        // correctly-sized out-param.
        let rc = unsafe { sys::rac_http_request_send(client, &request, &mut response) };
        // SAFETY: `client` was created by rac_http_client_create above and is
        // not used again after this.
        unsafe { sys::rac_http_client_destroy(client) };

        let ok = rc == sys::SUCCESS && (200..300).contains(&response.status);
        let mut body = String::new();
        if !response.body_bytes.is_null() && response.body_len > 0 {
            // SAFETY: commons guarantees `body_bytes` holds `body_len` valid
            // bytes until `rac_http_response_free`.
            let body_slice =
                unsafe { std::slice::from_raw_parts(response.body_bytes, response.body_len) };
            body = String::from_utf8_lossy(body_slice).into_owned();
        }
        if !ok {
            // Surface the exact backend rejection (status + response body) so
            // schema mismatches (e.g. strict extra_forbidden 422s) are
            // diagnosable from wally.
            // SAFETY: `endpoint` is the caller-supplied NUL-terminated
            // endpoint per this callback's documented contract, or NULL.
            let endpoint_str = unsafe {
                if endpoint.is_null() {
                    "?".to_string()
                } else {
                    cstr_or_empty(endpoint)
                }
            };
            output::status_line(&format!(
                "telemetry POST {endpoint_str} -> rc={} http={} body={}",
                output::describe_result(rc),
                response.status,
                if body.is_empty() { "(empty)" } else { &body }
            ));
            // DEBUG: dump the exact request JSON so a malformed offset can be
            // inspected.
            if !json_body.is_null() {
                // SAFETY: the callback contract guarantees `json_body` holds
                // `json_length` valid bytes for the duration of this call.
                let json_bytes =
                    unsafe { std::slice::from_raw_parts(json_body as *const u8, json_length) };
                dump_telemetry_body(json_bytes);
            }
        }
        let body_c = (!body.is_empty()).then(|| CString::new(body.clone()).unwrap_or_default());
        telemetry_http_complete(
            manager,
            ok,
            body_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            if ok {
                std::ptr::null()
            } else {
                c"telemetry POST failed".as_ptr()
            },
        );
        // SAFETY: `response` was populated by rac_http_request_send above,
        // valid to free either way (send zero-initializes on early failure
        // paths too).
        unsafe { sys::rac_http_response_free(&mut response) };
    });
}

// Runs the canonical two-phase SDK init so telemetry can flush.
// Development (keyless OSS): Phase 1 fills baked staging backend URL when
// needed; Phase 2 skips JWT/register; telemetry POSTs anonymously → PUBLIC org.
// Production: Phase 2 authenticates + registers with the API key.
fn initialize_telemetry_auth(connection: &Connection) {
    let keyless_dev = connection.environment == sys::RAC_ENV_DEVELOPMENT;
    let mut effective_base_url = connection.base_url.clone();
    if keyless_dev && effective_base_url.is_empty() {
        // SAFETY: takes no arguments; returns a 'static baked string or NULL.
        let baked = unsafe { sys::rac_dev_config_get_staging_base_url() };
        // SAFETY: `baked` may be NULL, which is this function's documented
        // contract.
        if unsafe { sys::rac_dev_config_is_usable_http_url(baked) } {
            // SAFETY: just confirmed non-null/usable above.
            effective_base_url = unsafe { cstr_or_empty(baked) };
        }
    }

    // No remote telemetry without a base URL (baked or explicit).
    if effective_base_url.is_empty() {
        return;
    }
    // Authenticated environments still need an API key.
    if !keyless_dev && connection.api_key.is_empty() {
        return;
    }

    // Enable the auth manager. NULL secure storage: tokens are not persisted
    // across runs (fine for a CLI session); authentication still runs per run
    // when Phase 2 expects a key.
    // SAFETY: NULL secure storage is this function's documented no-op
    // contract.
    unsafe { sys::rac_auth_init(std::ptr::null()) };

    let mut device_id_buf =
        [0 as std::os::raw::c_char; sys::RAC_DEVICE_ID_BUFFER_MIN_SIZE as usize];
    // SAFETY: `device_id_buf` is a correctly-sized out buffer per the
    // documented `>= RAC_DEVICE_ID_BUFFER_MIN_SIZE` contract.
    let device_rc = unsafe {
        sys::rac_device_get_or_create_persistent_id(device_id_buf.as_mut_ptr(), device_id_buf.len())
    };
    let device_id = if device_rc == sys::SUCCESS {
        // SAFETY: populated and NUL-terminated by the call above on success.
        unsafe { CStr::from_ptr(device_id_buf.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    };

    // Create + register the telemetry sink BEFORE Phase 2 so its flush has a
    // sink and events emitted during subsequent commands are tracked.
    // Delivery runs through wally_telemetry_http_callback over the desktop
    // HTTP transport; the terminal batch flushes in shutdown() during
    // teardown.
    let device_id_c = CString::new(device_id).unwrap_or_default();
    let platform_c = CString::new(desktop_platform()).unwrap_or_default();
    let sdk_version_c = CString::new(crate::WALLY_VERSION).unwrap_or_default();
    // SAFETY: every argument is a valid NUL-terminated string for the
    // duration of this call; commons copies what it needs internally.
    let manager = unsafe {
        sys::rac_telemetry_manager_create(
            connection.environment,
            device_id_c.as_ptr(),
            platform_c.as_ptr(),
            sdk_version_c.as_ptr(),
        )
    };
    if !manager.is_null() {
        // SAFETY: `manager` is non-null; `wally_telemetry_http_callback`
        // matches `rac_telemetry_http_callback_t` exactly and is a plain
        // `extern "C" fn` wrapped in `catch_unwind`, valid for as long as the
        // SDK may call it, including after this function returns.
        unsafe {
            sys::rac_telemetry_manager_set_http_callback(
                manager,
                Some(wally_telemetry_http_callback),
                manager as *mut c_void,
            )
        };
        // SAFETY: `manager` is non-null and stays valid (in
        // `G_TELEMETRY_MANAGER`) until `shutdown()` destroys it.
        unsafe { sys::rac_events_set_telemetry_sink(manager as *mut c_void) };
    }
    G_TELEMETRY_MANAGER.store(manager, Ordering::SeqCst);

    let mut phase1 = v1::SdkInitPhase1Request {
        environment: proto_environment_from_rac(connection.environment) as i32,
        api_key: connection.api_key.clone(),
        base_url: effective_base_url,
        platform: desktop_platform().to_string(),
        sdk_version: crate::WALLY_VERSION.to_string(),
        ..Default::default()
    };
    // SAFETY: `device_id_buf` was populated above (or left zeroed on
    // failure); re-reading it here (rather than the moved `device_id_c`) is
    // simplest since it is still in scope.
    if device_rc == sys::SUCCESS {
        phase1.device_id = unsafe { CStr::from_ptr(device_id_buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
    }

    // proto3 encoding does not fail (unlike C++'s SerializeToString, which
    // the original checks defensively), so there is no Rust equivalent of
    // the "telemetry phase 1 serialize failed" branch.
    let phase1_bytes = proto::serialize(&phase1);
    let mut phase1_out = proto::ProtoBuffer::new();
    // SAFETY: `phase1_bytes` is valid for the duration of this call (or its
    // pointer may be null when empty, matching the ABI's documented
    // "0-length means the pointer may be null" contract); `phase1_out` is a
    // freshly initialized, correctly-sized out-param.
    let rc = unsafe {
        sys::rac_sdk_init_phase1_proto(
            if phase1_bytes.is_empty() {
                std::ptr::null()
            } else {
                phase1_bytes.as_ptr()
            },
            phase1_bytes.len(),
            phase1_out.as_mut_ptr(),
        )
    };
    drop(phase1_out);
    if rc != sys::SUCCESS {
        output::status_line(&format!(
            "warning: telemetry phase 1 failed: {}",
            output::describe_result(rc)
        ));
        return;
    }

    // flush_telemetry/discover_downloaded_models/rescan_local_models were
    // deleted from SdkInitPhase2Request outright: telemetry flushing and
    // registry/local-file reconciliation are now unconditional commons
    // behavior on every Phase 2 call, not per-call hints.
    let phase2 = v1::SdkInitPhase2Request::default();
    let phase2_bytes = proto::serialize(&phase2);
    let mut phase2_out = proto::ProtoBuffer::new();
    // SAFETY: see the phase 1 call above.
    let rc = unsafe {
        sys::rac_sdk_init_phase2_proto(
            if phase2_bytes.is_empty() {
                std::ptr::null()
            } else {
                phase2_bytes.as_ptr()
            },
            phase2_bytes.len(),
            phase2_out.as_mut_ptr(),
        )
    };
    let parsed = (phase2_out.status() == sys::SUCCESS)
        .then(|| v1::SdkInitResult::decode(phase2_out.bytes()).ok())
        .flatten();
    drop(phase2_out);

    if rc != sys::SUCCESS {
        output::status_line(&format!(
            "warning: telemetry phase 2 failed: {}",
            output::describe_result(rc)
        ));
        return;
    }

    if let Some(result) = parsed {
        // http_configured/device_registered were deleted outright from
        // SdkInitResult; has_completed_http_setup is the cross-phase latched
        // bit that survives (the per-call http_configured signal has no
        // replacement).
        let mut note = format!(
            "telemetry ready | has_completed_http_setup={}",
            if result.has_completed_http_setup {
                "yes"
            } else {
                "no"
            }
        );
        if !result.warning.is_empty() {
            note += &format!(" | {}", result.warning);
        }
        output::status_line(&note);
    }
}

/// Initialize the SDK for CLI use. Logs go to stderr at WARNING by default
/// (DEBUG with --verbose, ERROR with --quiet). Returns the first failing step's
/// error code.
pub fn bootstrap(options: &GlobalOptions) -> Result<Bootstrapped, sys::rac_result_t> {
    let home = crate::config::cli_paths::resolve_home(&options.home_override);
    if home.is_empty() {
        output::error_line("cannot resolve RunAnywhere home ($HOME unset?)");
        return Err(sys::RAC_ERROR_NOT_INITIALIZED);
    }

    let connection = match resolve_connection(options) {
        Ok(connection) => connection,
        Err((_code, message)) => {
            output::error_line(&message);
            return Err(sys::RAC_ERROR_INVALID_CONFIGURATION);
        }
    };

    {
        let mut bootstrapped = G_BOOTSTRAPPED.lock().unwrap_or_else(|e| e.into_inner());
        if !*bootstrapped {
            // rac_model_paths_set_base_dir happily creates <home>/Models on
            // first use, which is right for the real default — but an
            // explicit --home that doesn't exist is far more often a typo
            // than a fresh directory someone wants populated from nothing,
            // and the only symptom otherwise is a catalog that looks empty
            // with no explanation at all.
            if !options.home_override.is_empty() && !std::path::Path::new(&home).exists() {
                output::status_line(&format!(
                    "warning: --home '{home}' does not exist yet; it will be created empty \
                     (pass the right path, or `wally models pull` into this one)"
                ));
            }

            // SAFETY: `G_ADAPTER` has static storage for the process
            // lifetime, matching C++'s static `g_adapter`; this whole block
            // runs under `G_BOOTSTRAPPED`'s lock, so nothing else writes to
            // it concurrently.
            let adapter_ptr = G_ADAPTER.0.get();
            // SAFETY: `adapter_ptr` is a valid, correctly-sized out pointer.
            let rc = unsafe { sys::rac_desktop_adapter_init(std::ptr::null(), adapter_ptr) };
            if rc != sys::SUCCESS {
                output::error_line(&format!(
                    "desktop adapter init failed: {}",
                    output::describe_result(rc)
                ));
                return Err(rc);
            }

            let home_c = CString::new(home.clone()).unwrap_or_default();
            // SAFETY: `home_c` is a valid NUL-terminated string for this
            // call.
            let rc = unsafe { sys::rac_model_paths_set_base_dir(home_c.as_ptr()) };
            if rc != sys::SUCCESS {
                output::error_line(&format!(
                    "model paths init failed: {}",
                    output::describe_result(rc)
                ));
                return Err(rc);
            }

            // Configure the logger BEFORE rac_init so init-time logs obey the
            // CLI level too. Two distinct knobs: stderr_always off makes the
            // adapter the single sink (commons' own stderr mirror would
            // double every line); the logger min level is a separate gate
            // from rac_config_t.log_level.
            let log_level = log_level_for(options);
            // SAFETY: takes a value argument; always safe to call.
            unsafe { sys::rac_logger_set_stderr_always(sys::FALSE) };
            // SAFETY: takes a value argument; always safe to call.
            unsafe { sys::rac_logger_set_min_level(log_level) };
            #[cfg(wally_has_llamacpp)]
            {
                // SAFETY: `route_ggml_log` matches `rac_llamacpp_log_callback_fn`
                // exactly and is a plain `extern "C" fn` wrapped in
                // `catch_unwind`, valid for the process lifetime.
                unsafe {
                    sys::rac_llamacpp_set_log_callback(Some(route_ggml_log), std::ptr::null_mut())
                };
            }

            let config = sys::rac_config_t {
                platform_adapter: adapter_ptr as *const sys::rac_platform_adapter_t,
                log_level,
                log_tag: c"wally".as_ptr(),
                reserved: std::ptr::null_mut(),
            };
            // SAFETY: `config.platform_adapter` points at `G_ADAPTER`, which
            // has static storage and stays valid until `rac_shutdown` per the
            // documented retained-pointer contract; `log_tag` is a 'static C
            // string.
            let rc = unsafe { sys::rac_init(&config) };
            if rc != sys::SUCCESS {
                output::error_line(&format!("rac_init failed: {}", output::describe_result(rc)));
                return Err(rc);
            }

            // SAFETY: takes no arguments; always safe to call.
            let rc = unsafe { sys::rac_desktop_http_transport_register() };
            if rc != sys::SUCCESS {
                output::error_line(&format!(
                    "HTTP transport registration failed: {}",
                    output::describe_result(rc)
                ));
                return Err(rc);
            }

            initialize_sdk_metadata(&connection);

            // Prefer the richer desktop device-info callbacks from
            // device_info.rs (battery/RAM/CPU/fingerprint). control_plane.rs
            // still owns login() / control_plane_post() for the explicit
            // auth/telemetry commands.
            // device_info::install_device_callbacks() is a safe fn; it wraps
            // its own unsafe FFI internally.
            if device_info::install_device_callbacks() != sys::SUCCESS {
                output::status_line("warning: device info callbacks failed to register");
            }

            initialize_telemetry_auth(&connection);

            #[cfg(wally_has_llamacpp)]
            // SAFETY: takes no arguments; always safe to call.
            if unsafe { sys::rac_backend_llamacpp_register() } != sys::SUCCESS {
                output::status_line("warning: llamacpp backend failed to register");
            }
            #[cfg(wally_has_onnx)]
            // SAFETY: takes no arguments; always safe to call.
            if unsafe { sys::rac_backend_onnx_register() } != sys::SUCCESS {
                output::status_line("warning: onnx backend failed to register");
            }
            #[cfg(wally_has_sherpa)]
            // SAFETY: takes no arguments; always safe to call.
            if unsafe { sys::rac_backend_sherpa_register() } != sys::SUCCESS {
                output::status_line("warning: sherpa backend failed to register");
            }
            #[cfg(wally_has_mlx)]
            {
                // A C++-only host (wally-cxx) links the MLX plugin from the
                // kit but provides no MLX runtime callbacks, so availability
                // is false by design. That is the normal state for that
                // build, not a warning — note it only under --verbose. A
                // host that DOES provide the runtime and still fails to
                // register is a real problem and always warns.
                // SAFETY: takes no arguments; always safe to call.
                if unsafe { sys::rac_mlx_is_available() } != sys::TRUE {
                    if options.verbose {
                        output::status_line(
                            "mlx runtime callbacks not provided; skipping MLX backend",
                        );
                    }
                // SAFETY: takes no arguments; always safe to call.
                } else if unsafe { sys::rac_backend_mlx_register() } != sys::SUCCESS {
                    output::status_line("warning: mlx backend failed to register");
                }
            }
            #[cfg(wally_has_neurt)]
            {
                // The neurt engine (Apple-only: ANE LLM + CoreML diffusion)
                // has no dedicated rac_backend_neurt_register() fn; register
                // its plugin entry directly. This call also keeps the static
                // rac_backend_neurt archive linked (references
                // rac_plugin_entry_neurt), mirroring how the other backends
                // stay alive.
                // SAFETY: returns a 'static plugin vtable pointer owned by
                // the linked neurt archive.
                let vtable = unsafe { sys::rac_plugin_entry_neurt() };
                // SAFETY: `vtable` is a valid plugin vtable pointer per the
                // above.
                if unsafe { sys::rac_plugin_register(vtable) } != sys::SUCCESS {
                    output::status_line(
                        "warning: neurt (Apple Neural Engine) backend failed to register",
                    );
                }
            }
            #[cfg(wally_has_qhexrt)]
            // SAFETY: takes no arguments; always safe to call.
            if unsafe { sys::rac_backend_qhexrt_register() } != sys::SUCCESS {
                output::status_line(
                    "warning: qhexrt (Qualcomm Hexagon NPU) backend failed to register",
                );
            }

            // Built-in catalog — same per-launch registration pattern as the
            // example apps (the registry is in-memory). Ad-hoc URL/HF pulls
            // from previous runs come back via the commons model-folder
            // manifest restore inside the registry refresh/discover paths.
            let _ = catalog::register_all();

            *bootstrapped = true;
        }
    }

    let mut models_dir = String::new();
    let mut models_buf = [0 as std::os::raw::c_char; 1024];
    // SAFETY: `models_buf` is a correctly-sized out buffer.
    if unsafe {
        sys::rac_model_paths_get_models_directory(models_buf.as_mut_ptr(), models_buf.len())
    } == sys::SUCCESS
    {
        // SAFETY: NUL-terminated by the kit on success.
        models_dir = unsafe { CStr::from_ptr(models_buf.as_ptr()) }
            .to_string_lossy()
            .into_owned();
    }

    Ok(Bootstrapped { home, models_dir })
}

/// rac_shutdown() wrapper; safe to call when bootstrap never ran.
pub fn shutdown() {
    let mut bootstrapped = G_BOOTSTRAPPED.lock().unwrap_or_else(|e| e.into_inner());
    if *bootstrapped {
        // rac_shutdown() flushes the terminal telemetry batch through the
        // registered sink (our HTTP callback) before clearing lifetime state.
        // SAFETY: takes no arguments; always safe to call.
        unsafe { sys::rac_shutdown() };
        // SAFETY: NULL detaches the sink; the header documents this waits
        // for in-flight use to finish.
        unsafe { sys::rac_events_set_telemetry_sink(std::ptr::null_mut()) };
        let manager = G_TELEMETRY_MANAGER.swap(std::ptr::null_mut(), Ordering::SeqCst);
        if !manager.is_null() {
            // SAFETY: `manager` was created by rac_telemetry_manager_create
            // in initialize_telemetry_auth and just detached from the sink
            // above; not used again after this.
            unsafe { sys::rac_telemetry_manager_destroy(manager) };
        }
        *bootstrapped = false;
    }
}

/// The process telemetry manager created by bootstrap() (null if telemetry was
/// not initialized). Exposed for the live telemetry integration test.
pub fn active_telemetry_manager() -> *mut sys::rac_telemetry_manager_t {
    G_TELEMETRY_MANAGER.load(Ordering::SeqCst)
}
