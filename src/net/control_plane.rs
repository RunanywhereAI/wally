//! Control-plane network wiring for wally: auth, device, telemetry HTTP (port
//! of src/net/control_plane.cpp). Drives the canonical commons entry points
//! (rac_auth_* + rac_sdk_init_phase2_proto). Requires bootstrap() to have run.
//! Owner: the SDK bootstrap / device info / progress port.

use std::ffi::{CStr, CString};
use std::os::raw::c_void;
use std::sync::OnceLock;

use crate::io::output;
use crate::io::proto::{self, v1};
use crate::sys;

const K_ERROR_BODY_PREVIEW: usize = 500;

/// Largest prefix of `body` whose byte length is `<= K_ERROR_BODY_PREVIEW`
/// bytes and ends on a char boundary, with `\n`/`\r`/`\t` folded to a space,
/// plus a trailing "…" when `body` itself is longer than the preview limit.
/// The C++ original slices raw bytes (`std::string::substr`), which can cut a
/// multi-byte UTF-8 sequence in half; Rust's `String` must always be valid
/// UTF-8, so this keeps whole characters instead — the closest safe
/// equivalent for a user-facing error line.
fn single_line_preview(body: &str) -> String {
    let take_len = body
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= K_ERROR_BODY_PREVIEW)
        .last()
        .unwrap_or(0);
    let mut preview: String = body[..take_len]
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    if body.len() > K_ERROR_BODY_PREVIEW {
        preview.push('…');
    }
    preview
}

/// A NUL-terminated `*const c_char` (possibly NULL) owned by commons, copied
/// into an owned Rust `String` ("" when NULL) before the pointer's validity
/// window ends.
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

fn endpoint_authenticate() -> &'static str {
    let bytes = sys::RAC_ENDPOINT_AUTHENTICATE;
    std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap_or("/api/v1/auth/sdk/authenticate")
}

/// "macos" / "linux" / "windows" — the X-Platform header + auth payload value.
///
/// Cached (the C++ original re-reads `rac_desktop_platform_name()` on every
/// call, but the value is a compile-time constant baked into the SDK, so
/// caching once behaves identically and is what lets this return `&'static
/// str` instead of the C++ signature's raw `const char*`).
pub fn platform_name() -> &'static str {
    static PLATFORM: OnceLock<String> = OnceLock::new();
    PLATFORM
        .get_or_init(|| {
            // SAFETY: rac_desktop_platform_name takes no arguments and returns
            // either NULL or a NUL-terminated, process-lifetime string literal
            // baked into the SDK.
            let ptr = unsafe { sys::rac_desktop_platform_name() };
            // SAFETY: see above.
            unsafe { cstr_or_empty(ptr) }
        })
        .as_str()
}

/// Best-effort local hardware model (e.g. "Mac16,8"); empty when unknown.
pub fn device_model() -> &'static str {
    static MODEL: OnceLock<String> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            // SAFETY: rac_desktop_device_model takes no arguments and returns
            // either NULL or a NUL-terminated string valid for this call.
            let ptr = unsafe { sys::rac_desktop_device_model() };
            // SAFETY: see above.
            unsafe { cstr_or_empty(ptr) }
        })
        .as_str()
}

/// Best-effort OS version string (kernel release); empty when unknown.
pub fn os_version_string() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION
        .get_or_init(|| {
            // SAFETY: rac_desktop_os_version takes no arguments and returns
            // either NULL or a NUL-terminated string valid for this call.
            let ptr = unsafe { sys::rac_desktop_os_version() };
            // SAFETY: see above.
            unsafe { cstr_or_empty(ptr) }
        })
        .as_str()
}

/// One buffered control-plane HTTP exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResult {
    /// send-level result (network/TLS/timeout)
    pub transport: sys::rac_result_t,
    /// HTTP status (0 when transport failed)
    pub status: i32,
    /// response body (server error JSON on 4xx/5xx)
    pub body: String,
}

impl Default for HttpResult {
    fn default() -> Self {
        HttpResult {
            transport: sys::SUCCESS,
            status: 0,
            body: String::new(),
        }
    }
}

impl HttpResult {
    pub fn ok(&self) -> bool {
        self.transport == sys::SUCCESS && (200..300).contains(&self.status)
    }

    /// "HTTP 401: {...}" / "network error" — for user-facing error lines.
    pub fn describe(&self) -> String {
        if self.transport != sys::SUCCESS {
            let mut message = format!("network error: {}", output::describe_result(self.transport));
            if !self.body.is_empty() {
                message += &format!(" ({})", single_line_preview(&self.body));
            }
            return message;
        }
        let mut message = format!("HTTP {}", self.status);
        if !self.body.is_empty() {
            message += &format!(": {}", single_line_preview(&self.body));
        }
        message
    }
}

/// POST `endpoint` against the configured base URL with the canonical
/// control-plane headers; `bearer_auth` attaches the current JWT.
pub fn control_plane_post(endpoint: &str, json_body: &str, bearer_auth: bool) -> HttpResult {
    let mut result = HttpResult::default();

    // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
    // string or NULL, valid for this call.
    let base_url_ptr = unsafe { sys::rac_state_get_base_url() };
    // SAFETY: see above.
    let base_url = unsafe { cstr_or_empty(base_url_ptr) };
    if base_url.is_empty() {
        result.transport = sys::RAC_ERROR_INVALID_CONFIGURATION;
        result.body = "control-plane base URL is not configured".to_string();
        return result;
    }

    let mut url_buf = [0 as std::os::raw::c_char; 2048];
    let base_url_c = CString::new(base_url).unwrap_or_default();
    let endpoint_c = CString::new(endpoint).unwrap_or_default();
    // SAFETY: `base_url_c`/`endpoint_c` are valid NUL-terminated strings for
    // this call; `url_buf` is a correctly-sized out buffer.
    let written = unsafe {
        sys::rac_build_url(
            base_url_c.as_ptr(),
            endpoint_c.as_ptr(),
            url_buf.as_mut_ptr(),
            url_buf.len(),
        )
    };
    if written < 0 {
        result.transport = sys::RAC_ERROR_INVALID_CONFIGURATION;
        result.body = "failed to build control-plane URL".to_string();
        return result;
    }

    // Canonical control-plane header set — mirrors commons' phase-2 pattern:
    // defaults (Content-Type/Accept/X-SDK-*) + X-Platform + apikey [+ Bearer].
    let mut headers: Vec<sys::rac_http_header_kv_t> = Vec::new();
    let mut defaults_ptr: *const sys::rac_http_header_kv_t = std::ptr::null();
    let mut default_count: usize = 0;
    // SAFETY: out-params are valid stack locals.
    if unsafe { sys::rac_http_default_headers(&mut defaults_ptr, &mut default_count) }
        == sys::SUCCESS
        && !defaults_ptr.is_null()
    {
        // SAFETY: commons guarantees `defaults_ptr` describes `default_count`
        // valid, static-lifetime entries.
        let defaults = unsafe { std::slice::from_raw_parts(defaults_ptr, default_count) };
        headers.extend_from_slice(defaults);
    }
    let platform_name_header = c"X-Platform";
    let platform_value = CString::new(platform_name()).unwrap_or_default();
    headers.push(sys::rac_http_header_kv_t {
        name: platform_name_header.as_ptr(),
        value: platform_value.as_ptr(),
    });

    // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
    // string or NULL, valid for this call.
    let api_key_ptr = unsafe { sys::rac_state_get_api_key() };
    let apikey_header = c"apikey";
    // SAFETY: `api_key_ptr` is checked non-null immediately below; the
    // underlying commons state stays valid for this call.
    if !api_key_ptr.is_null() && !unsafe { CStr::from_ptr(api_key_ptr) }.to_bytes().is_empty() {
        headers.push(sys::rac_http_header_kv_t {
            name: apikey_header.as_ptr(),
            value: api_key_ptr,
        });
    }

    let mut bearer_value = CString::default();
    let auth_header = c"Authorization";
    if bearer_auth {
        // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
        // string or NULL, valid for this call.
        let token_ptr = unsafe { sys::rac_auth_get_access_token() };
        if !token_ptr.is_null() {
            // SAFETY: non-null, NUL-terminated per the function's contract.
            let token = unsafe { CStr::from_ptr(token_ptr) }.to_string_lossy();
            if !token.is_empty() {
                bearer_value = CString::new(format!("Bearer {token}")).unwrap_or_default();
            }
        }
    }
    if !bearer_value.as_bytes().is_empty() {
        headers.push(sys::rac_http_header_kv_t {
            name: auth_header.as_ptr(),
            value: bearer_value.as_ptr(),
        });
    }

    let mut client: *mut sys::rac_http_client_t = std::ptr::null_mut();
    // SAFETY: `&mut client` is a valid out-param.
    let rc = unsafe { sys::rac_http_client_create(&mut client) };
    if rc != sys::SUCCESS {
        result.transport = rc;
        return result;
    }

    // SAFETY: takes no arguments; always safe to call.
    let environment = unsafe { sys::rac_state_get_environment() };
    let json_bytes = json_body.as_bytes();
    let request = sys::rac_http_request_t {
        method: c"POST".as_ptr(),
        url: url_buf.as_ptr(),
        headers: if headers.is_empty() {
            std::ptr::null()
        } else {
            headers.as_ptr()
        },
        header_count: headers.len(),
        body_bytes: json_bytes.as_ptr(),
        body_len: json_bytes.len(),
        // Credential-bearing control-plane requests never replay across
        // redirects.
        // SAFETY: takes an environment enum; always safe to call.
        timeout_ms: unsafe { sys::rac_env_default_http_timeout_ms(environment) },
        follow_redirects: sys::FALSE,
        expected_checksum_hex: std::ptr::null(),
    };

    let mut response: sys::rac_http_response_t = unsafe { std::mem::zeroed() };
    // SAFETY: `client` is a freshly created, valid handle; `request` is built
    // above with every pointer alive for this call; `response` is a
    // correctly-sized out-param.
    let rc = unsafe { sys::rac_http_request_send(client, &request, &mut response) };
    // SAFETY: `client` was created by rac_http_client_create above and is not
    // used again after this.
    unsafe { sys::rac_http_client_destroy(client) };

    result.transport = rc;
    if rc == sys::SUCCESS {
        result.status = response.status;
        if !response.body_bytes.is_null() && response.body_len > 0 {
            // SAFETY: commons guarantees `body_bytes` holds `body_len` valid
            // bytes until `rac_http_response_free`.
            let body_slice =
                unsafe { std::slice::from_raw_parts(response.body_bytes, response.body_len) };
            result.body = String::from_utf8_lossy(body_slice).into_owned();
        }
    }
    // SAFETY: `response` was populated by rac_http_request_send above, valid
    // to free either way (send zero-initializes on early failure paths too).
    unsafe { sys::rac_http_response_free(&mut response) };
    result
}

/// Result of the real auth handshake (authenticate → device → assignments).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginSummary {
    pub organization_id: String,
    pub user_id: String,
    pub backend_device_id: String,
    pub persistent_device_id: String,
    pub token_expires_at: i64,
    pub has_completed_http_setup: bool,
    pub assignment_count: u32,
    pub warning: String,
}

/// Run the real control-plane handshake against the configured backend.
pub fn login() -> Result<LoginSummary, (sys::rac_result_t, String)> {
    // SAFETY: takes no arguments; always safe to call.
    let env = unsafe { sys::rac_state_get_environment() };
    // SAFETY: takes no arguments; returns a commons-owned NUL-terminated
    // string or NULL, valid for this call.
    let api_key_ptr = unsafe { sys::rac_state_get_api_key() };
    // SAFETY: `env` is a value type; `api_key_ptr` may be NULL, which is this
    // function's documented contract.
    let auth_expected = unsafe { sys::rac_env_auth_expected(env, api_key_ptr) };
    if !auth_expected {
        return Err((
            sys::RAC_ERROR_INVALID_CONFIGURATION,
            "keyless development has no JWT login; use --environment production with --base-url and --api-key"
                .to_string(),
        ));
    }

    // Step 1: API key → JWT. Idempotent within a process; a valid token
    // short-circuits (phase 2 below then takes its authenticated fast path).
    // SAFETY: takes no arguments; always safe to call.
    let is_authenticated = unsafe { sys::rac_auth_is_authenticated() };
    // SAFETY: takes no arguments; always safe to call.
    let needs_refresh = unsafe { sys::rac_auth_needs_refresh() };
    if !is_authenticated || needs_refresh {
        // SAFETY: takes no arguments; returns a commons-owned pointer or
        // NULL, valid for this call.
        let config_ptr = unsafe { sys::rac_sdk_get_config() };
        if config_ptr.is_null() {
            return Err((
                sys::RAC_ERROR_NOT_INITIALIZED,
                "SDK configuration unavailable (bootstrap did not run?)".to_string(),
            ));
        }
        // SAFETY: `config_ptr` was just checked non-null and points at a live
        // commons-owned config for the duration of this call.
        let request_json_ptr = unsafe { sys::rac_auth_build_authenticate_request(config_ptr) };
        if request_json_ptr.is_null() {
            return Err((
                sys::RAC_ERROR_INVALID_CONFIGURATION,
                "failed to build authenticate request".to_string(),
            ));
        }
        // SAFETY: `request_json_ptr` was just checked non-null and is
        // NUL-terminated per the function's documented contract.
        let request_json = unsafe { CStr::from_ptr(request_json_ptr) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `request_json_ptr` was heap-allocated by commons
        // (`rac_auth_build_authenticate_request`'s "caller must free"
        // contract); `rac_free` is the matching deallocator, and the pointer
        // is not used again after this.
        unsafe { sys::rac_free(request_json_ptr as *mut c_void) };

        let response = control_plane_post(endpoint_authenticate(), &request_json, false);
        if !response.ok() {
            let code = if response.transport != sys::SUCCESS {
                response.transport
            } else {
                sys::RAC_ERROR_HTTP_ERROR
            };
            return Err((
                code,
                format!("authentication failed: {}", response.describe()),
            ));
        }
        let body_c = CString::new(response.body).unwrap_or_default();
        // SAFETY: `body_c` is a valid NUL-terminated string for the duration
        // of this call.
        let auth_rc = unsafe { sys::rac_auth_handle_authenticate_response(body_c.as_ptr()) }
            as sys::rac_result_t;
        if auth_rc != sys::SUCCESS && auth_rc != sys::RAC_ERROR_SECURE_STORAGE_FAILED {
            return Err((
                sys::RAC_ERROR_INVALID_RESPONSE,
                format!(
                    "authentication response rejected: {}",
                    output::describe_result(auth_rc)
                ),
            ));
        }
    }

    // Step 2: canonical phase-2 orchestration — device registration +
    // model-assignment fetch (telemetry flush / local rescans stay off; the
    // CLI runs those flows through their own commands).
    let request = v1::SdkInitPhase2Request::default();
    let request_bytes = proto::serialize(&request);
    let mut out_buffer = proto::ProtoBuffer::new();
    // SAFETY: `request_bytes` is valid for the duration of this call (or its
    // pointer is null when empty, matching the ABI's documented "0-length
    // means the pointer may be null" contract); `out_buffer` is a freshly
    // initialized, correctly-sized out-param.
    let phase2_rc = unsafe {
        sys::rac_sdk_init_phase2_proto(
            if request_bytes.is_empty() {
                std::ptr::null()
            } else {
                request_bytes.as_ptr()
            },
            request_bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result: v1::SdkInitResult = match proto::parse_proto_buffer(out_buffer) {
        Ok(result) if phase2_rc == sys::SUCCESS => result,
        Ok(_) => {
            return Err((
                phase2_rc,
                format!(
                    "services init failed: {}",
                    output::describe_result(phase2_rc)
                ),
            ));
        }
        Err(parse_error) => {
            let message = if parse_error.is_empty() {
                output::describe_result(phase2_rc)
            } else {
                parse_error
            };
            let code = if phase2_rc != sys::SUCCESS {
                phase2_rc
            } else {
                sys::RAC_ERROR_INVALID_RESPONSE
            };
            return Err((code, format!("services init failed: {message}")));
        }
    };
    if let Some(error) = &result.error {
        return Err((
            sys::RAC_ERROR_INVALID_STATE,
            format!("services init failed: {}", error.message),
        ));
    }

    // SAFETY: each getter takes no arguments and returns a commons-owned
    // NUL-terminated string or NULL, valid for this call.
    let organization_id = unsafe { cstr_or_empty(sys::rac_auth_get_organization_id()) };
    // SAFETY: see above.
    let user_id = unsafe { cstr_or_empty(sys::rac_auth_get_user_id()) };
    // SAFETY: see above.
    let backend_device_id = unsafe { cstr_or_empty(sys::rac_auth_get_device_id()) };
    // SAFETY: see above.
    let persistent_device_id = unsafe { cstr_or_empty(sys::rac_state_get_device_id()) };
    // SAFETY: takes no arguments; always safe to call.
    let token_expires_at = unsafe { sys::rac_auth_get_token_expires_at() };

    Ok(LoginSummary {
        organization_id,
        user_id,
        backend_device_id,
        persistent_device_id,
        token_expires_at,
        has_completed_http_setup: result.has_completed_http_setup,
        assignment_count: result.linked_models_count,
        warning: result.warning,
    })
}
