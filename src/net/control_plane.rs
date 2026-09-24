//! Control-plane network wiring for wally: auth, device, telemetry HTTP (port
//! of src/net/control_plane.cpp). Drives the canonical commons entry points
//! (rac_auth_* + rac_sdk_init_phase2_proto). Requires bootstrap() to have run.
//! Owner: the SDK bootstrap / device info / progress port.

use crate::sys;

/// "macos" / "linux" / "windows" — the X-Platform header + auth payload value.
pub fn platform_name() -> &'static str {
    todo!("bootstrap port: platform_name")
}

/// Best-effort local hardware model (e.g. "Mac16,8"); empty when unknown.
pub fn device_model() -> &'static str {
    todo!("bootstrap port: device_model")
}

/// Best-effort OS version string (kernel release); empty when unknown.
pub fn os_version_string() -> &'static str {
    todo!("bootstrap port: os_version_string")
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
        todo!("bootstrap port: HttpResult::describe")
    }
}

/// POST `endpoint` against the configured base URL with the canonical
/// control-plane headers; `bearer_auth` attaches the current JWT.
pub fn control_plane_post(endpoint: &str, json_body: &str, bearer_auth: bool) -> HttpResult {
    let _ = json_body;
    todo!("bootstrap port: control_plane_post ({endpoint}, {bearer_auth})")
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
    todo!("bootstrap port: login")
}
