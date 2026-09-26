//! The console's CLI endpoints: device-flow sign-in, identity, usage, hosted
//! models and request cancellation (port of src/account/console.cpp). Requests
//! and responses go through the generated binding in console_contract.rs (P0
//! contract-first rule in AGENTS.md).
//!
//! The real transport is ureq with native-tls (the OS trust store, as curl and
//! WinHTTP used), env proxies with loopback bypass. Tests inject a `Transport`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::console_contract as contract;
use super::{
    UsageRequestRow, UsageRequestsPage, UsageRequestsQuery, UsageRequestsTotals,
    USAGE_REQUESTS_CURSOR_MAX_CHARS,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub body: String,
    /// Never logged.
    pub bearer_token: String,
    pub timeout_ms: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: i32,
    pub body: String,
    /// Lower-cased header names.
    pub headers: BTreeMap<String, String>,
}

impl HttpResponse {
    /// Seconds from a digits-only `Retry-After` header (the API never sends an
    /// HTTP-date), or -1 when the header is absent or malformed. A day is the
    /// ceiling: anything larger is a misconfiguration, and honoring it would
    /// hang a terminal for hours.
    pub fn retry_after_seconds(&self) -> i32 {
        let Some(value) = self.headers.get("retry-after") else {
            return -1;
        };
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return -1;
        }
        // Parse into a 32-bit width (matches C++'s `int` via std::from_chars):
        // a digit string that overflows i32 (e.g. an accidental millisecond
        // epoch) must report "no valid Retry-After", not clamp to the ceiling.
        match value.parse::<i32>() {
            Ok(seconds) => std::cmp::min(seconds, 86400),
            Err(_) => -1,
        }
    }
}

/// Sends one request. `Err` is a transport failure message (C++ returned false
/// and filled the error string).
pub type Transport = Arc<dyn Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    Cancelled,
    NotFound,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    pub plan: String,
    pub tokens_this_month: i64,
    pub monthly_token_limit: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageDay {
    pub date: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageEvent {
    pub request_id: String,
    pub model: String,
    pub harness: String,
    pub started_at: String,
    pub error_code: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
    pub ttft_ms: i64,
    pub status_code: i32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageModel {
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cost_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credits {
    pub balance_micros: i64,
    pub granted_micros: i64,
    pub spent_micros: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageWindow {
    pub window: String,
    pub seconds: i64,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub credit: Credits,
    pub totals: UsageTotals,
    pub windows: Vec<UsageWindow>,
    pub timeline: Vec<UsageDay>,
    pub models: Vec<UsageModel>,
    pub events: Vec<UsageEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageQuery {
    pub days: i32,
    pub model: String,
    pub limit: i32,
}

impl Default for UsageQuery {
    fn default() -> Self {
        UsageQuery {
            days: 30,
            model: String::new(),
            limit: 20,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub request_code: String,
    /// Proves the process collecting the grant started it. Never logged.
    pub poll_secret: String,
    pub verification_url: String,
    pub expires_in: i32,
    pub interval: i32,
}

impl Default for Authorization {
    fn default() -> Self {
        Authorization {
            request_code: String::new(),
            poll_secret: String::new(),
            verification_url: String::new(),
            expires_in: 0,
            interval: 2,
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Grant {
    pub access_token: String,
    pub refresh_token: String,
    pub email: String,
    pub plan: String,
    pub expires_in: i64,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Grant")
            .field("email", &self.email)
            .field("plan", &self.plan)
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollResult {
    Pending,
    Approved,
    Denied,
    Expired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityResult {
    Ok,
    Unauthorized,
    Failed,
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: i64,
    pub max_output_tokens: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogPrice {
    pub id: String,
    pub input_per_mtok: i64,
    pub output_per_mtok: i64,
}

pub const LOGIN_MAX_WAIT_SECONDS: i32 = 10;

/// The delay before the next device-flow poll: never sooner than the server
/// asked, capped (see the C++ for the exact rule).
///
/// The authorization's own cadence is the floor and the console's request
/// raises it. A negative or absent delay means the console asked for nothing.
pub fn next_poll_delay_seconds(interval: i32, retry_after: i32) -> i32 {
    let floor = std::cmp::max(interval, 1);
    let asked = std::cmp::max(retry_after, 0);
    std::cmp::max(floor, asked)
}

/// Outcome of `ConsoleClient::poll`: the result, and the server's Retry-After
/// when it rate-limited the poll (C++'s `int* retry_after` out-param).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollOutcome {
    pub result: PollResult,
    pub grant: Option<Grant>,
    pub error: String,
    pub retry_after: i32,
}

/// C++ `Refresh(..., bool* unavailable)`: on failure, whether the console was
/// unreachable/rate-limited (retry later) rather than refusing the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshError {
    pub message: String,
    pub unavailable: bool,
}

#[derive(Clone, Default)]
pub struct ConsoleClient {
    /// None → the real network transport.
    transport: Option<Transport>,
}

// Both transports (curl and WinHTTP in the C++; ureq here) share the same
// defaults: 10s to connect, 30s in all. A request's own `timeout_ms` bounds
// the whole call instead; the connect phase never gets more than its usual
// share of it.
const CONNECT_TIMEOUT_MS: i32 = 10_000;
const TOTAL_TIMEOUT_MS: i32 = 30_000;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

// Proxy discovery (WPAD/PAC auto-detection on Windows, see
// windows_autodetected_system_proxy) runs before a request's own timeout
// starts and is bounded to 3s of its own. Once discovery has spent any of
// that against the request, never leave less than this much of the budget
// for the request itself -- a slow-but-successful discovery should still
// give the request a chance to run instead of failing it outright.
const MIN_REMAINING_TOTAL_TIMEOUT_MS: i32 = 1_000;

fn total_timeout_ms(request: &HttpRequest) -> i32 {
    if request.timeout_ms > 0 {
        request.timeout_ms
    } else {
        TOTAL_TIMEOUT_MS
    }
}

fn connect_timeout_ms(total_timeout_ms: i32) -> i32 {
    std::cmp::min(CONNECT_TIMEOUT_MS, total_timeout_ms)
}

/// What is left of `total_timeout_ms` after proxy discovery already spent
/// `discovery_elapsed` finding out whether to use one. Without this,
/// discovery time is on top of the request's own timeout instead of counted
/// against it, so the first request of a process (a cache miss in
/// `cached_autodetected_system_proxy`) could take up to 3s longer than
/// configured. Never negative, never below `MIN_REMAINING_TOTAL_TIMEOUT_MS`,
/// and never above `total_timeout_ms` itself -- discovery can only spend
/// budget, never hand back more than the caller asked for, so the floor is
/// only allowed to pull the remainder back up when discovery actually ate
/// into it.
fn remaining_after_discovery_ms(total_timeout_ms: i32, discovery_elapsed: Duration) -> i32 {
    let elapsed_ms = i32::try_from(discovery_elapsed.as_millis()).unwrap_or(i32::MAX);
    std::cmp::min(
        total_timeout_ms,
        std::cmp::max(
            total_timeout_ms.saturating_sub(elapsed_ms),
            MIN_REMAINING_TOTAL_TIMEOUT_MS,
        ),
    )
}

fn url_is_loopback(url: &str) -> bool {
    match url.parse::<ureq::http::Uri>() {
        Ok(uri) => matches!(
            uri.host(),
            Some("localhost") | Some("127.0.0.1") | Some("::1")
        ),
        Err(_) => false,
    }
}

/// libcurl's real per-scheme proxy env var precedence (see the curl manual's
/// ENVIRONMENT section): the scheme-specific variable first, then `ALL_PROXY`
/// as the fallback if it is unset. `http_proxy` is checked in lowercase only
/// -- curl never reads the uppercase `HTTP_PROXY`, because on a CGI-hosted
/// server an attacker's `Proxy:` request header is exposed to the target
/// process as the env var `HTTP_PROXY` (CVE-2016-5385, "httpoxy"); `https_proxy`
/// and `all_proxy` have no such attacker-controlled uppercase alias and so are
/// read in both cases, lowercase first. An explicit empty value disables the
/// proxy for that request outright rather than falling through to `ALL_PROXY`.
/// `NO_PROXY`/`no_proxy` is deliberately never consulted here: the console
/// only calls this for a URL that `resolve_proxy_url` has already confirmed
/// is not loopback, mirroring how the C++ pins `CURLOPT_NOPROXY` to just the
/// loopback hosts, which replaces (not merges with) whatever `NO_PROXY` says.
fn proxy_env_value(scheme: &str, get_env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let scheme_specific = match scheme {
        "https" => get_env("https_proxy").or_else(|| get_env("HTTPS_PROXY")),
        "http" => get_env("http_proxy"),
        _ => None,
    };
    let value =
        scheme_specific.or_else(|| get_env("all_proxy").or_else(|| get_env("ALL_PROXY")))?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// The proxy URL (if any) libcurl's `CURLOPT_NOPROXY "localhost,127.0.0.1,::1"`
/// plus its normal env-var proxy resolution would pick for `url`: loopback is
/// always direct regardless of any proxy env var, and everything else follows
/// `proxy_env_value`. Pure and closure-driven so it is testable without
/// mutating real process env (which `cargo test`'s parallel threads share).
fn resolve_proxy_url(url: &str, get_env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if url_is_loopback(url) {
        return None;
    }
    let scheme = url
        .parse::<ureq::http::Uri>()
        .ok()?
        .scheme_str()?
        .to_string();
    proxy_env_value(&scheme, get_env)
}

/// Windows' `ProxyServer` registry value (the same field
/// `WinHttpGetIEProxyConfigForCurrentUser` surfaces as `lpszProxy`) is either
/// a single `host:port` applied to every protocol, or a
/// `protocol=host:port;protocol=host:port` list -- see
/// <https://learn.microsoft.com/en-us/windows/win32/api/winhttp/ns-winhttp-winhttp_current_user_ie_proxy_config>.
/// Pick the entry for `scheme`, falling back to a bare (unprefixed) value.
///
/// Its only production caller is Windows-only (`windows_static_system_proxy`);
/// it stays unconditionally compiled so its parsing rules are covered by
/// hermetic tests on every platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn static_proxy_for_scheme(proxy_server: &str, scheme: &str) -> Option<String> {
    if !proxy_server.contains('=') {
        let trimmed = proxy_server.trim();
        return if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    proxy_server.split(';').find_map(|entry| {
        let (protocol, value) = entry.split_once('=')?;
        if !protocol.trim().eq_ignore_ascii_case(scheme) {
            return None;
        }
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

#[cfg(windows)]
fn windows_static_system_proxy(scheme: &str) -> Option<String> {
    // The static (non-PAC) half of what the C++ gets from
    // WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: the same "Use a proxy server"
    // settings under Internet Options that WinHttpGetIEProxyConfigForCurrentUser
    // reads. The WPAD/PAC half lives in `windows_autodetected_system_proxy`,
    // which `console_proxy_url` tries first; this is only the fallback for a
    // network with a manually configured proxy and no auto-detection.
    use std::ffi::CStr;
    use windows_sys::Win32::System::Registry::{
        RegGetValueA, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
    };

    let subkey = c"Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings";

    let mut enabled: u32 = 0;
    let mut enabled_size = std::mem::size_of::<u32>() as u32;
    let enabled_name = c"ProxyEnable";
    // SAFETY: `subkey`/`enabled_name` are valid NUL-terminated C strings; the
    // output pointer is a single correctly-sized `u32` and `enabled_size`
    // matches its capacity.
    let rc = unsafe {
        RegGetValueA(
            HKEY_CURRENT_USER,
            subkey.as_ptr() as *const u8,
            enabled_name.as_ptr() as *const u8,
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut enabled as *mut u32 as *mut core::ffi::c_void,
            &mut enabled_size,
        )
    };
    if rc != 0 || enabled == 0 {
        return None;
    }

    let mut proxy_server = [0u8; 1024];
    let mut proxy_server_size = proxy_server.len() as u32;
    let proxy_server_name = c"ProxyServer";
    // SAFETY: `subkey`/`proxy_server_name` are valid NUL-terminated C strings;
    // `proxy_server`/`proxy_server_size` are a correctly-sized out buffer.
    let rc = unsafe {
        RegGetValueA(
            HKEY_CURRENT_USER,
            subkey.as_ptr() as *const u8,
            proxy_server_name.as_ptr() as *const u8,
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            proxy_server.as_mut_ptr() as *mut core::ffi::c_void,
            &mut proxy_server_size,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: RegGetValueA NUL-terminates a REG_SZ result on success.
    let text = unsafe { CStr::from_ptr(proxy_server.as_ptr() as *const i8) }
        .to_string_lossy()
        .into_owned();
    static_proxy_for_scheme(&text, scheme)
}

#[cfg(not(windows))]
fn windows_static_system_proxy(_scheme: &str) -> Option<String> {
    None
}

/// A NUL-terminated wide (UTF-16) string a WinHTTP out-param points at,
/// decoded losslessly. Mirrors the `CStr::from_ptr` read of the narrow
/// registry string above, just for a wide one; an unpaired surrogate in a PAC
/// URL or resolved proxy host is not expected, so lossy replacement (matching
/// the console's own credential code at the same OS boundary) is fine.
///
/// # Safety
/// `ptr` must be non-null and point at a NUL-terminated UTF-16 buffer that
/// stays valid for the read.
#[cfg(windows)]
unsafe fn wide_pwstr_to_string(ptr: windows_sys::core::PWSTR) -> String {
    let mut len = 0usize;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

/// What WinHTTP's automatic proxy discovery (WPAD / a PAC script) said for a
/// URL. `Direct` is a real answer -- the PAC script chose no proxy -- and must
/// win over the static setting, as it does under the C++'s
/// `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`; only `NotConfigured` falls back.
#[derive(Debug, Clone, PartialEq, Eq)]
// Only the Windows discovery answers `Direct`/`Proxy`; elsewhere it is always
// `NotConfigured`, and the tests construct the rest.
#[cfg_attr(not(windows), allow(dead_code))]
enum AutoProxy {
    /// Auto-detect and PAC are off, or discovery failed.
    NotConfigured,
    Direct,
    Proxy(String),
}

/// `windows_autodetected_system_proxy`, asked once per origin per process.
/// Discovery can take up to its 3 s timeout, and a sign-in polls the console
/// every few seconds; every console request goes to the same origin.
fn cached_autodetected_system_proxy(url: &str) -> AutoProxy {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, AutoProxy>>,
    > = std::sync::OnceLock::new();
    let origin = url
        .parse::<ureq::http::Uri>()
        .ok()
        .and_then(|uri| Some(format!("{}://{}", uri.scheme_str()?, uri.authority()?)))
        .unwrap_or_else(|| url.to_string());
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&origin)
    {
        return hit.clone();
    }
    let fresh = windows_autodetected_system_proxy(url);
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(origin, fresh.clone());
    fresh
}

/// The WPAD/PAC half of what the C++ got for free from
/// `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY`: read whether Internet Options has
/// "Automatically detect settings" and/or a proxy auto-config script URL
/// turned on, and if either is, ask WinHTTP to resolve a proxy for `url`
/// through WPAD discovery and/or that script -- the same
/// `WinHttpGetProxyForUrl` call the C++'s automatic-proxy access type made
/// internally. Bounded by a short timeout: an unreachable WPAD server or a
/// hung PAC script must not stall every console request.
#[cfg(windows)]
fn windows_autodetected_system_proxy(url: &str) -> AutoProxy {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::Networking::WinHttp::{
        WinHttpCloseHandle, WinHttpGetIEProxyConfigForCurrentUser, WinHttpGetProxyForUrl,
        WinHttpOpen, WinHttpSetTimeouts, WINHTTP_ACCESS_TYPE_NO_PROXY,
        WINHTTP_AUTOPROXY_AUTO_DETECT, WINHTTP_AUTOPROXY_CONFIG_URL, WINHTTP_AUTOPROXY_OPTIONS,
        WINHTTP_AUTO_DETECT_TYPE_DHCP, WINHTTP_AUTO_DETECT_TYPE_DNS_A,
        WINHTTP_CURRENT_USER_IE_PROXY_CONFIG, WINHTTP_PROXY_INFO,
    };

    let mut ie_config = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: `ie_config` is a valid, zeroed, correctly-sized out param.
    let got_config = unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut ie_config) } != 0;

    let auto_detect = got_config && ie_config.fAutoDetect != 0;
    let auto_config_url = if got_config && !ie_config.lpszAutoConfigUrl.is_null() {
        // SAFETY: WinHttpGetIEProxyConfigForCurrentUser NUL-terminates a
        // returned auto-config URL.
        Some(unsafe { wide_pwstr_to_string(ie_config.lpszAutoConfigUrl) })
    } else {
        None
    };
    // `lpszAutoConfigUrl`/`lpszProxy`/`lpszProxyBypass` are GlobalAlloc'd by
    // WinHTTP; the caller frees them (MSDN, WinHttpGetIEProxyConfigForCurrentUser).
    for ptr in [
        ie_config.lpszAutoConfigUrl,
        ie_config.lpszProxy,
        ie_config.lpszProxyBypass,
    ] {
        if !ptr.is_null() {
            // SAFETY: `ptr` came from the GlobalAlloc'd fields above.
            unsafe {
                GlobalFree(ptr as *mut core::ffi::c_void);
            }
        }
    }

    if !auto_detect && auto_config_url.is_none() {
        return AutoProxy::NotConfigured;
    }

    let wide_url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let wide_auto_config_url: Option<Vec<u16>> = auto_config_url
        .as_deref()
        .map(|s| s.encode_utf16().chain(std::iter::once(0)).collect());

    let mut options = WINHTTP_AUTOPROXY_OPTIONS::default();
    if auto_detect {
        options.dwFlags |= WINHTTP_AUTOPROXY_AUTO_DETECT;
        options.dwAutoDetectFlags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    }
    if let Some(wide) = &wide_auto_config_url {
        options.dwFlags |= WINHTTP_AUTOPROXY_CONFIG_URL;
        options.lpszAutoConfigUrl = wide.as_ptr();
    }

    let agent: Vec<u16> = "wally".encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `agent` is a NUL-terminated wide string; the proxy/bypass args
    // are null because this handle is only used for autoproxy discovery
    // below, never to send a real request through a configured proxy.
    let session = unsafe {
        WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_NO_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if session.is_null() {
        return AutoProxy::NotConfigured;
    }

    const DISCOVERY_TIMEOUT_MS: i32 = 3000;
    // SAFETY: `session` is the handle WinHttpOpen just returned.
    unsafe {
        WinHttpSetTimeouts(
            session,
            DISCOVERY_TIMEOUT_MS,
            DISCOVERY_TIMEOUT_MS,
            DISCOVERY_TIMEOUT_MS,
            DISCOVERY_TIMEOUT_MS,
        );
    }

    let mut proxy_info = WINHTTP_PROXY_INFO::default();
    // SAFETY: `session` is valid; `wide_url` is NUL-terminated; `options`/
    // `proxy_info` are correctly-sized in/out params.
    let resolved =
        unsafe { WinHttpGetProxyForUrl(session, wide_url.as_ptr(), &mut options, &mut proxy_info) }
            != 0;

    // SAFETY: `session` is the handle WinHttpOpen returned above.
    unsafe {
        WinHttpCloseHandle(session);
    }

    if !resolved {
        return AutoProxy::NotConfigured;
    }

    let proxy = if !proxy_info.lpszProxy.is_null() {
        // SAFETY: WinHttpGetProxyForUrl NUL-terminates a returned proxy list.
        Some(unsafe { wide_pwstr_to_string(proxy_info.lpszProxy) })
    } else {
        None
    };
    for ptr in [proxy_info.lpszProxy, proxy_info.lpszProxyBypass] {
        if !ptr.is_null() {
            // SAFETY: `ptr` came from the GlobalAlloc'd fields above.
            unsafe {
                GlobalFree(ptr as *mut core::ffi::c_void);
            }
        }
    }

    // WinHttpGetProxyForUrl can return several fallback proxies separated by
    // semicolons; ureq::Proxy only takes one, so use the first, same as the
    // bare (unprefixed) case in `static_proxy_for_scheme`. No proxy in a
    // successful answer means the PAC script said DIRECT.
    let first = proxy
        .as_deref()
        .and_then(|list| list.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    if first.is_empty() {
        AutoProxy::Direct
    } else {
        AutoProxy::Proxy(first.to_string())
    }
}

#[cfg(not(windows))]
fn windows_autodetected_system_proxy(_url: &str) -> AutoProxy {
    AutoProxy::NotConfigured
}

/// The proxy (if any) to use for a console request, matching the C++'s
/// per-platform behaviour exactly. On Windows the C++ uses WinHTTP with
/// `WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY` (loopback gets
/// `WINHTTP_ACCESS_TYPE_NO_PROXY` instead) -- it never reads any proxy
/// environment variable, so Windows here is loopback -> direct, otherwise
/// WPAD/PAC auto-detection, then the static system (Internet Options) proxy
/// setting, else direct. Everywhere else the C++ goes through libcurl, so
/// non-Windows keeps `resolve_proxy_url`'s environment-variable rules
/// unchanged. `is_windows` is a parameter rather than `cfg!(windows)` so both
/// platforms' rules are covered by hermetic tests on every host; production
/// always passes `cfg!(windows)`.
fn console_proxy_url(
    url: &str,
    is_windows: bool,
    get_env: &dyn Fn(&str) -> Option<String>,
    autodetected_system_proxy: &dyn Fn(&str) -> AutoProxy,
    static_system_proxy: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    if !is_windows {
        return resolve_proxy_url(url, get_env);
    }
    if url_is_loopback(url) {
        return None;
    }
    match autodetected_system_proxy(url) {
        AutoProxy::Proxy(proxy) => return Some(proxy),
        AutoProxy::Direct => return None,
        AutoProxy::NotConfigured => {}
    }
    let scheme = url
        .parse::<ureq::http::Uri>()
        .ok()?
        .scheme_str()?
        .to_string();
    static_system_proxy(&scheme)
}

fn resolve_console_proxy(url: &str) -> Option<ureq::Proxy> {
    let value = console_proxy_url(
        url,
        cfg!(windows),
        &|name| std::env::var(name).ok(),
        &cached_autodetected_system_proxy,
        &windows_static_system_proxy,
    )?;
    ureq::Proxy::new(&value).ok()
}

/// TLS for the console: the platform's own stack and trust store, as the C++
/// had through libcurl and WinHTTP. ureq defaults to rustls with a bundled
/// root list; with only the `native-tls` feature built, that default panics on
/// the first https:// request.
fn console_tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build()
}

fn map_transport_error(error: ureq::Error) -> String {
    match error {
        // curl's write callback refused the body past kMaximumResponseBytes;
        // ureq's body-read limit is the equivalent guard.
        ureq::Error::BodyExceedsLimit(_) => {
            "Wally Cloud sent an unexpectedly large response".to_string()
        }
        _ => "could not reach Wally Cloud - check your internet connection".to_string(),
    }
}

/// The real network transport: ureq with native-tls (the OS trust store, as
/// curl and WinHTTP used), env proxies with a loopback bypass, redirects never
/// followed, TLS verification on (the connector's default).
fn real_transport(request: &HttpRequest) -> Result<HttpResponse, String> {
    if !super::browser_url_is_safe(&request.url) {
        return Err("refusing an unsafe console request URL".to_string());
    }
    if !request.bearer_token.is_empty() && !super::session_token_is_safe(&request.bearer_token) {
        return Err("refusing an invalid console bearer token".to_string());
    }

    let total = total_timeout_ms(request);
    // CURLOPT_NOPROXY "localhost,127.0.0.1,::1": a request to the loopback
    // (dev consoles, tests) never goes through an env-configured proxy, and
    // that list replaces NO_PROXY rather than adding to it -- see
    // resolve_proxy_url and proxy_env_value. Timed because on Windows a cache
    // miss runs WPAD/PAC discovery here, before the request below starts --
    // see remaining_after_discovery_ms.
    let discovery_start = Instant::now();
    let proxy = resolve_console_proxy(&request.url);
    let total = remaining_after_discovery_ms(total, discovery_start.elapsed());
    let connect = connect_timeout_ms(total);

    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .tls_config(console_tls_config())
        .proxy(proxy)
        .user_agent("wally-cloud-auth/1")
        .timeout_connect(Some(Duration::from_millis(connect.max(0) as u64)))
        .timeout_global(Some(Duration::from_millis(total.max(0) as u64)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut builder = ureq::http::Request::builder()
        .method(request.method.as_str())
        .uri(&request.url)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    if !request.bearer_token.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {}", request.bearer_token));
    }
    let http_request = builder
        .body(request.body.clone())
        .map_err(|error| format!("could not build the console request: {error}"))?;

    let mut response = agent.run(http_request).map_err(map_transport_error)?;

    let status = response.status().as_u16() as i32;
    let mut headers = BTreeMap::new();
    for (name, value) in response.headers() {
        if let Ok(text) = value.to_str() {
            // HeaderName is always lower-cased by the http crate already.
            headers.insert(name.as_str().to_string(), text.to_string());
        }
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(map_transport_error)?;

    if !(100..=599).contains(&status) {
        return Err("could not reach Wally Cloud - check your internet connection".to_string());
    }

    Ok(HttpResponse {
        status,
        body,
        headers,
    })
}

/// How much of a console's refusal is shown. The contract allows 2048
/// characters; a terminal line needs the sentence, not the essay.
const REFUSAL_MAX_CHARS: usize = 240;

/// The console's own `ApiError.message` from a refusal body, when the body
/// has one, it is printable text, and it does not carry the caller's own
/// access token back at them (a hostile or buggy console must not get the
/// terminal to echo the session it was just handed). `access_token` empty
/// (no session was in play yet, e.g. `/auth/cli/start`) skips that filter.
fn console_refusal_message(response: &HttpResponse, access_token: &str) -> Option<String> {
    parse_object(response)
        .ok()
        .and_then(|object| contract::ApiError::from_json(&object).ok())
        .map(|error| error.message)
        .filter(|message| display_text_is_safe(message, 2048))
        .filter(|message| access_token.is_empty() || !message.contains(access_token))
}

/// `message` cut to one terminal line, attributed to `operation`.
fn format_refusal(operation: &str, message: &str) -> String {
    if message.len() > REFUSAL_MAX_CHARS {
        format!(
            "Wally Cloud refused the {operation}: {}...",
            &message[..REFUSAL_MAX_CHARS]
        )
    } else {
        format!("Wally Cloud refused the {operation}: {message}")
    }
}

/// Every console failure in this file is phrased here, so this is the one
/// place that decides what a person reads when the cloud says no. It is
/// written for them, not for us: a status line and an internal endpoint tells
/// somebody trying to start a coding agent nothing they can act on.
fn http_error(
    operation: &str,
    origin: &str,
    response: &HttpResponse,
    access_token: &str,
) -> String {
    let status = response.status;
    let mut message = if status == 429 {
        // Not a broken request - overload. Say how long the server itself
        // asked for, so "try again" is a fact rather than a guess.
        let wait = response.retry_after_seconds();
        if wait >= 0 {
            format!("Wally Cloud is busy - try again in {wait}s")
        } else {
            "Wally Cloud is busy - try again in a moment".to_string()
        }
    } else if status == 401 {
        "your cloud session is no longer valid - run `wally account login`".to_string()
    } else if status == 403 {
        // Unlike a 401, a 403 is the console refusing this request outright --
        // a plan without access, a route this session may never call -- and
        // the session itself is still good, so this must never send someone
        // back to `wally account login` (the contract's ApiError names this
        // `forbidden` and carries a message a person can act on, e.g. "model
        // not entitled"; show it when the body has one).
        match console_refusal_message(response, access_token) {
            Some(message) => format_refusal(operation, &message),
            None => format!("Wally Cloud refused the {operation}"),
        }
    } else if status == 404 {
        "Wally Cloud has no such endpoint".to_string()
    } else if status >= 500 {
        // The status stays on this one: the person cannot act on a 5xx either
        // way, and it is the only thing support can work from.
        format!("Wally Cloud is temporarily unavailable - try again shortly (HTTP {status})")
    } else {
        // Nothing specific to say, so name the operation that failed and keep
        // the status, which is the only part support can act on.
        format!("Wally Cloud could not complete the {operation} (HTTP {status})")
    };
    // Name the endpoint only when the console denies the route exists, i.e.
    // the wrong console was probably asked and the origin is the actionable
    // part. Otherwise it is internal detail a person cannot act on.
    if status == 404 {
        message.push_str(&format!(" ({origin})"));
    }
    message
}

/// A 400 or 422 is the console refusing what was asked, and its `ApiError`
/// message says what to change ("the window may span at most 31 days"). That
/// sentence is shown, cut to a line, when it is printable text that does not
/// carry the session token; anything else falls back to `http_error`, which
/// never echoes a body.
fn refusal_error(
    operation: &str,
    origin: &str,
    response: &HttpResponse,
    access_token: &str,
) -> String {
    match console_refusal_message(response, access_token) {
        Some(message) => format_refusal(operation, &message),
        None => http_error(operation, origin, response, access_token),
    }
}

/// Lifts every `provider` the generated `UsageProvider` does not know out of a
/// usage-export body, leaving `null` in its place, and returns them by row.
///
/// The generated reader fails a whole page on an unknown enum value, which is
/// right for a value the CLI acts on and wrong for this one: `provider` is a
/// label the export only reports, and a console that starts routing to a new
/// provider must not make every page of history unreadable. Only this field is
/// relaxed, and only here; every other closed value still fails the page.
fn take_unknown_providers(body: &mut serde_json::Value) -> Vec<Option<String>> {
    let Some(rows) = body.get_mut("requests").and_then(|r| r.as_array_mut()) else {
        return Vec::new();
    };
    rows.iter_mut()
        .map(|row| {
            let provider = row.get_mut("provider")?;
            let raw = provider.as_str()?;
            if contract::UsageProvider::parse(raw).is_ok() {
                return None;
            }
            let raw = raw.to_string();
            *provider = serde_json::Value::Null;
            Some(raw)
        })
        .collect()
}

fn parse_object(response: &HttpResponse) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value = serde_json::from_str(&response.body)
        // Do not include the response body: an upstream error can echo a token.
        .map_err(|_| "console returned malformed JSON".to_string())?;
    if !parsed.is_object() {
        return Err("console returned a JSON value instead of an object".to_string());
    }
    Ok(parsed)
}

const CONTRACT_MISMATCH: &str = "console returned a response that did not match the contract";

fn display_text_is_safe(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// A console string that is safe to print in a terminal. Anything else is
/// dropped rather than rendered: this text lands straight in the user's shell.
fn display_safe(value: &str, maximum: usize) -> String {
    if display_text_is_safe(value, maximum) {
        value.to_string()
    } else {
        String::new()
    }
}

/// A label the console chose, made printable rather than dropped: every
/// character outside printable ASCII becomes `?`, and a label past `maximum`
/// keeps its first `maximum - 3` characters and ends in `...`. For text that is
/// only ever shown (an unknown provider), where some of the label is worth more
/// than none of it. Empty in, empty out.
fn display_lossy(value: &str, maximum: usize) -> String {
    let printable: String = value
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '?' })
        .collect();
    if printable.len() <= maximum {
        return printable;
    }
    let keep = maximum.saturating_sub(3);
    format!("{}...", &printable[..keep])
}

fn request_code_is_safe(value: &str) -> bool {
    // The control plane mints these with Python's `token_urlsafe`, whose
    // alphabet is base64url: letters, digits, `-` and `_`. Omitting `_`
    // rejected roughly half of all real codes as malformed.
    (4..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The plan is a closed enum in the contract, so its only text is a known-safe
/// literal; render it back for the domain Grant, which carries a plain string.
fn plan_text(plan: Option<contract::CliPlan>) -> String {
    plan.map(|p| p.as_str().to_string()).unwrap_or_default()
}

/// Map the wire grant fields onto the domain Grant, still sanitizing every
/// string that reaches the terminal or the credential file. The contract
/// typing removes the field-name guesswork; the safety checks stay.
fn map_grant(
    access_token: Option<String>,
    refresh_token: Option<String>,
    email: Option<String>,
    plan: Option<contract::CliPlan>,
    expires_in: i64,
) -> Result<Grant, String> {
    let grant = Grant {
        access_token: access_token.unwrap_or_default(),
        refresh_token: refresh_token.unwrap_or_default(),
        email: email.unwrap_or_default(),
        plan: plan_text(plan),
        expires_in: std::cmp::max(0, expires_in),
    };
    if (!grant.access_token.is_empty() && !super::session_token_is_safe(&grant.access_token))
        || (!grant.refresh_token.is_empty() && !super::session_token_is_safe(&grant.refresh_token))
        || (!grant.email.is_empty() && !display_text_is_safe(&grant.email, 320))
    {
        return Err("console returned an invalid cloud session".to_string());
    }
    Ok(grant)
}

/// Percent-encode a query value. Model names carry dots and slashes, and a
/// filter is user input either way.
fn query_escape(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        let unreserved =
            b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~';
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

fn console_origin(input: &str) -> Result<String, String> {
    super::normalize_console_url(input)
}

fn copy_usage_totals(from: &contract::UsageTotals) -> UsageTotals {
    UsageTotals {
        requests: from.requests,
        prompt_tokens: from.prompt_tokens,
        completion_tokens: from.completion_tokens,
        cached_tokens: from.cached_tokens,
        cost_micros: from.cost_micros,
    }
}

impl ConsoleClient {
    pub fn new(transport: Option<Transport>) -> Self {
        ConsoleClient { transport }
    }

    /// C++ `Send()`: run the request through the injected transport, or the
    /// real network transport when none was injected. An empty error from a
    /// failing transport gets a default message.
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let result = match &self.transport {
            Some(transport) => transport(&request),
            None => real_transport(&request),
        };
        result.map_err(|error| {
            if error.is_empty() {
                "could not reach Wally Cloud - check your internet connection".to_string()
            } else {
                error
            }
        })
    }

    /// `on_retry` runs before each retry of a refused/unreachable start.
    pub fn begin_authorization(
        &self,
        console_url: &str,
        hostname: &str,
        on_retry: Option<&dyn Fn()>,
    ) -> Result<Authorization, String> {
        let origin = console_origin(console_url)?;

        // The client value is the contract enum, whose only member serializes
        // to "rcli" -- InferenceInfra's CliClient StrEnum recognizes exactly
        // that, and sending anything else 422s /auth/cli/start.
        let body = contract::CliStartRequest {
            client: contract::CliClient::KRcli,
            hostname: hostname.to_string(),
        };
        let start = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/start"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };

        // A 429 here means the console is BUSY, not that this request is
        // wrong. Do the waiting here, for as long as the server asked (up to
        // kLoginMaxWaitSeconds), and only then fail with the same message a
        // plain rate limit would produce (InferenceInfra#444, #90).
        const RATE_LIMIT_RETRIES: i32 = 3;
        let mut response;
        let mut attempt = 0;
        loop {
            response = self.send(start.clone())?;
            if response.status != 429 || attempt >= RATE_LIMIT_RETRIES {
                break;
            }
            let asked = response.retry_after_seconds();
            // No header is the only case this guesses at, and it guesses small.
            let wait = if asked >= 0 {
                std::cmp::max(asked, 1)
            } else {
                1
            };
            if wait > LOGIN_MAX_WAIT_SECONDS {
                // Retrying before this elapses would only be refused again.
                // Say what the server asked for and let the person decide.
                return Err(http_error("authorization", &origin, &response, ""));
            }
            if let Some(callback) = on_retry {
                callback();
            }
            std::thread::sleep(Duration::from_secs(wait as u64));
            attempt += 1;
        }
        if response.status != 200 {
            return Err(http_error("authorization", &origin, &response, ""));
        }

        let object = parse_object(&response)?;
        let parsed = contract::CliStartResponse::from_json(&object)
            .map_err(|_| CONTRACT_MISMATCH.to_string())?;

        let request_code = parsed.request_code;
        let poll_secret = parsed.poll_secret;
        if !request_code_is_safe(&request_code) || !super::session_token_is_safe(&poll_secret) {
            return Err("console returned an invalid authorization request".to_string());
        }
        Ok(Authorization {
            request_code,
            poll_secret,
            verification_url: parsed.verification_url,
            expires_in: parsed.expires_in.clamp(30, 1800) as i32,
            interval: parsed.interval.clamp(1, 30) as i32,
        })
    }

    pub fn poll(&self, console_url: &str, authorization: &Authorization) -> PollOutcome {
        let mut outcome = PollOutcome {
            result: PollResult::Failed,
            grant: None,
            error: String::new(),
            retry_after: 0,
        };
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        let body = contract::CliPollRequest {
            request_code: authorization.request_code.clone(),
            poll_secret: authorization.poll_secret.clone(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/poll"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => {
                // A dropped connection has not denied anything either: keep
                // waiting, bounded by the request's expiry like a 5xx below.
                // Failing here ended real sign-ins on one network blip while
                // the person was still approving in the browser.
                outcome.error = error;
                outcome.result = PollResult::Pending;
                return outcome;
            }
        };
        if response.status != 200 {
            outcome.error = http_error("poll", &origin, &response, "");
            // A busy or briefly unavailable console has not denied anything,
            // and the person may still be approving in the browser. Treat it
            // as "still waiting" so the poll loop keeps going at its normal
            // cadence instead of failing the whole login on one refusal
            // (InferenceInfra#444). The loop is bounded by the authorization's
            // own expiry, so this cannot spin.
            if response.status == 429 || response.status >= 500 {
                outcome.retry_after = std::cmp::max(response.retry_after_seconds(), 0);
                outcome.result = PollResult::Pending;
            }
            return outcome;
        }

        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        let parsed = match contract::PollResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                outcome.error = CONTRACT_MISMATCH.to_string();
                return outcome;
            }
        };
        match parsed.status {
            contract::PollStatus::KPending => {
                outcome.result = PollResult::Pending;
                return outcome;
            }
            contract::PollStatus::KDenied => {
                outcome.result = PollResult::Denied;
                return outcome;
            }
            contract::PollStatus::KExpired => {
                outcome.result = PollResult::Expired;
                return outcome;
            }
            contract::PollStatus::KApproved => {}
        }

        let grant = match map_grant(
            parsed.access_token,
            parsed.refresh_token,
            parsed.email,
            parsed.plan,
            parsed.expires_in.unwrap_or(0),
        ) {
            Ok(grant) => grant,
            Err(error) => {
                outcome.error = error;
                return outcome;
            }
        };
        if grant.access_token.is_empty() || grant.refresh_token.is_empty() {
            outcome.error = "console approved the request without a complete session".to_string();
            return outcome;
        }
        outcome.result = PollResult::Approved;
        outcome.grant = Some(grant);
        outcome
    }

    pub fn refresh(&self, console_url: &str, refresh_token: &str) -> Result<Grant, RefreshError> {
        let unavailable_err = |message: String, unavailable: bool| RefreshError {
            message,
            unavailable,
        };
        if !super::session_token_is_safe(refresh_token) {
            return Err(unavailable_err(
                "no refresh token is available".to_string(),
                false,
            ));
        }
        let origin =
            console_origin(console_url).map_err(|message| unavailable_err(message, false))?;
        let body = contract::CliRefreshRequest {
            refresh_token: refresh_token.to_string(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/refresh"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: String::new(),
            timeout_ms: 0,
        };
        // No answer at all (DNS, a refused connection, a timeout) is the
        // console not being reached, which says nothing about the session:
        // logging in again cannot fix a network that is down.
        let response = self
            .send(request)
            .map_err(|message| unavailable_err(message, true))?;
        if response.status != 200 {
            let message = http_error("refresh", &origin, &response, "");
            // Same distinction as WhoAmI: a busy console has not told us this
            // session is bad, only that it could not answer (InferenceInfra#444).
            let unavailable = response.status == 429 || response.status >= 500;
            return Err(unavailable_err(message, unavailable));
        }
        let object = parse_object(&response).map_err(|message| unavailable_err(message, false))?;
        let parsed = contract::GrantResponse::from_json(&object)
            .map_err(|_| unavailable_err(CONTRACT_MISMATCH.to_string(), false))?;
        let grant = map_grant(
            Some(parsed.access_token),
            Some(parsed.refresh_token),
            Some(parsed.email),
            Some(parsed.plan),
            parsed.expires_in,
        )
        .map_err(|message| unavailable_err(message, false))?;
        if grant.access_token.is_empty() {
            return Err(unavailable_err(
                "console refreshed the session without an access token".to_string(),
                false,
            ));
        }
        Ok(grant)
    }

    pub fn who_am_i(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Identity, String) {
        let mut identity = Identity::default();
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                identity,
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/me"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                identity,
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            let message = http_error("identity request", &origin, &response, access_token);
            // A 429 or a 5xx says "not right now", not "this session is bad".
            // Treating them as a bad session locked a signed-in person out of
            // their own harness while a load test was running (InferenceInfra#444).
            if response.status == 429 || response.status >= 500 {
                return (IdentityResult::Unavailable, identity, message);
            }
            return (IdentityResult::Failed, identity, message);
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, identity, error),
        };
        let parsed = match contract::IdentityResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    identity,
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        identity.email = parsed.email;
        if !display_text_is_safe(&identity.email, 320) {
            return (
                IdentityResult::Failed,
                identity,
                "console returned an invalid account identity".to_string(),
            );
        }
        identity.plan = plan_text(Some(parsed.plan));
        identity.tokens_this_month = parsed.tokens_this_month;
        identity.monthly_token_limit = parsed.monthly_token_limit;
        if identity.tokens_this_month < 0 || identity.monthly_token_limit < 0 {
            return (
                IdentityResult::Failed,
                identity,
                "console returned invalid account usage".to_string(),
            );
        }
        (IdentityResult::Ok, identity, String::new())
    }

    pub fn revoke(
        &self,
        console_url: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> Result<(), String> {
        if access_token.is_empty() && refresh_token.is_empty() {
            return Ok(());
        }
        if (!access_token.is_empty() && !super::session_token_is_safe(access_token))
            || (!refresh_token.is_empty() && !super::session_token_is_safe(refresh_token))
        {
            return Err("cloud session contains an invalid token encoding".to_string());
        }
        let origin = console_origin(console_url)?;
        let body = contract::CliRefreshRequest {
            refresh_token: refresh_token.to_string(),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/auth/cli/revoke"),
            body: crate::io::json::dump(&body.to_json()),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = self.send(request)?;
        if response.status != 200 && response.status != 204 {
            return Err(http_error("revoke", &origin, &response, access_token));
        }
        Ok(())
    }

    pub fn fetch_usage(
        &self,
        console_url: &str,
        access_token: &str,
        query: &UsageQuery,
    ) -> (IdentityResult, Usage, String) {
        let mut usage = Usage::default();
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                usage,
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        // One read for the whole report: a terminal draws it in a single
        // pass, and three round-trips would only give it three chances to
        // print parts that disagree about when they were taken.
        let days = query.days.clamp(1, 365);
        let limit = query.limit.clamp(1, 200);
        let mut url = format!("{origin}/v1/cli/usage?days={days}&limit={limit}");
        if !query.model.is_empty() {
            url.push_str(&format!("&model={}", query_escape(&query.model)));
        }
        let request = HttpRequest {
            method: "GET".to_string(),
            url,
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                usage,
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                usage,
                http_error("usage request", &origin, &response, access_token),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, usage, error),
        };
        let parsed = match contract::CliUsageResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => return (IdentityResult::Failed, usage, CONTRACT_MISMATCH.to_string()),
        };

        // Map the typed response onto the domain Usage, sanitizing every
        // string the server chose before it reaches the terminal. The numbers
        // are already typed; the strings still pass through display_safe
        // because a hostile console must not be able to write escape
        // sequences to the user's screen.
        usage.credit.balance_micros = parsed.credit.balance_micros;
        usage.credit.granted_micros = parsed.credit.granted_micros;
        usage.credit.spent_micros = parsed.credit.spent_micros;
        usage.totals = copy_usage_totals(&parsed.totals);

        // Absent on every console deployed before windowed totals shipped.
        // The window label is a closed enum, so its text is a known-safe literal.
        for entry in &parsed.windows {
            usage.windows.push(UsageWindow {
                window: entry.window.as_str().to_string(),
                seconds: entry.seconds,
                totals: copy_usage_totals(&entry.totals),
            });
        }
        for point in &parsed.timeline {
            usage.timeline.push(UsageDay {
                date: display_safe(&point.date, 32),
                requests: point.requests,
                prompt_tokens: point.prompt_tokens,
                completion_tokens: point.completion_tokens,
                cost_micros: point.cost_micros,
            });
        }
        for entry in &parsed.models {
            usage.models.push(UsageModel {
                model: display_safe(&entry.model, 128),
                requests: entry.requests,
                prompt_tokens: entry.prompt_tokens,
                completion_tokens: entry.completion_tokens,
                cached_tokens: entry.cached_tokens,
                cost_micros: entry.cost_micros,
            });
        }
        for entry in &parsed.recent {
            usage.events.push(UsageEvent {
                request_id: display_safe(&entry.request_id, 128),
                model: display_safe(&entry.model, 128),
                harness: entry
                    .harness
                    .map(|h| display_safe(h.as_str(), 64))
                    .unwrap_or_default(),
                started_at: display_safe(&entry.ts_start, 64),
                error_code: display_safe(entry.error_code.as_deref().unwrap_or(""), 64),
                prompt_tokens: entry.prompt_tokens,
                completion_tokens: entry.completion_tokens,
                cached_tokens: entry.cached_tokens,
                cost_micros: entry.cost_micros,
                ttft_ms: entry.ttft_ms.unwrap_or(0),
                status_code: entry.status_code as i32,
            });
        }
        (IdentityResult::Ok, usage, String::new())
    }

    /// One page of settled requests (`GET /v1/cli/usage/requests`,
    /// InferenceInfra #809). The query is checked against the contract before
    /// anything is sent; `since`/`until` go out as given.
    pub fn fetch_usage_requests(
        &self,
        console_url: &str,
        access_token: &str,
        query: &UsageRequestsQuery,
    ) -> (IdentityResult, UsageRequestsPage, String) {
        let failed = |error: String| (IdentityResult::Failed, UsageRequestsPage::default(), error);
        if !super::session_token_is_safe(access_token) {
            return failed("no access token is available".to_string());
        }
        if let Err(error) = query.validate() {
            return failed(error);
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return failed(error),
        };
        let mut url = format!(
            "{origin}/v1/cli/usage/requests?since={}&until={}&limit={}",
            query_escape(&query.since),
            query_escape(&query.until),
            query.limit
        );
        if let Some(model) = &query.model {
            url.push_str(&format!("&model={}", query_escape(model)));
        }
        if let Some(status_code) = query.status_code {
            url.push_str(&format!("&status_code={status_code}"));
        }
        if let Some(id) = &query.response_request_id {
            url.push_str(&format!("&response_request_id={}", query_escape(id)));
        }
        if let Some(cursor) = &query.cursor {
            url.push_str(&format!("&cursor={}", query_escape(cursor)));
        }
        let request = HttpRequest {
            method: "GET".to_string(),
            url,
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return failed(error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                UsageRequestsPage::default(),
                "console session expired".to_string(),
            );
        }
        if response.status == 400 || response.status == 422 {
            return failed(refusal_error(
                "usage export",
                &origin,
                &response,
                access_token,
            ));
        }
        if response.status != 200 {
            return failed(http_error("usage export", &origin, &response, access_token));
        }
        let mut object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return failed(error),
        };
        // The contract requires `next_cursor` and makes `null` the last page.
        // The generated reader defaults a missing field to `None`, which would
        // end the export early and look complete, so absence is checked on
        // the raw object: a page that omits it broke the contract.
        if object.get("next_cursor").is_none() {
            return failed(CONTRACT_MISMATCH.to_string());
        }
        let providers = take_unknown_providers(&mut object);
        let parsed = match contract::UsageRequestPage::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => return failed(CONTRACT_MISMATCH.to_string()),
        };

        // A cursor is 1..=2048 characters in the contract and otherwise any
        // string. It is only ever sent back, through `query_escape`, and never
        // printed, so it is carried as given. One outside those bounds broke
        // the contract; read as "no more pages" it would end the export early
        // and look complete, so it fails the page.
        let next_cursor = match parsed.next_cursor {
            None => None,
            Some(cursor)
                if (1..=USAGE_REQUESTS_CURSOR_MAX_CHARS).contains(&cursor.chars().count()) =>
            {
                Some(cursor)
            }
            Some(_) => return failed(CONTRACT_MISMATCH.to_string()),
        };

        // Map the typed page onto the domain struct, sanitizing every string
        // the server chose: this text lands in the user's terminal.
        let optional = |value: &Option<String>, maximum: usize| {
            value
                .as_deref()
                .map(|v| display_safe(v, maximum))
                .filter(|v| !v.is_empty())
        };
        let requests = parsed
            .requests
            .iter()
            .enumerate()
            .map(|(index, record)| UsageRequestRow {
                request_id: display_safe(&record.request_id, 128),
                response_request_id: optional(&record.response_request_id, 128),
                // A UUID string ("8-4-4-4-12", 36 characters); the contract's
                // own bound for this field.
                api_key_id: optional(&record.api_key_id, 36),
                model: display_safe(&record.model, 128),
                provider: match record.provider {
                    Some(provider) => Some(provider.as_str().to_string()),
                    None => providers
                        .get(index)
                        .cloned()
                        .flatten()
                        .map(|raw| display_lossy(&raw, 64))
                        .filter(|label| !label.is_empty()),
                },
                status_code: record.status_code,
                error_code: optional(&record.error_code, 80),
                finish_reason: optional(&record.finish_reason, 40),
                stream: record.stream,
                ts_start: display_safe(&record.ts_start, 64),
                ts_end: optional(&record.ts_end, 64),
                recorded_at: display_safe(&record.recorded_at, 64),
                prompt_tokens: record.prompt_tokens,
                cached_tokens: record.cached_tokens,
                noncached_prompt_tokens: record.noncached_prompt_tokens,
                completion_tokens: record.completion_tokens,
                reasoning_tokens: record.reasoning_tokens,
                max_tokens_requested: record.max_tokens_requested,
                max_tokens_granted: record.max_tokens_granted,
                ttft_ms: record.ttft_ms,
                tpot_ms: record.tpot_ms,
                cost_micros: record.cost_micros,
                pricing_version: display_safe(&record.pricing_version, 64),
            })
            .collect();
        let totals = &parsed.totals;
        let page = UsageRequestsPage {
            as_of: display_safe(&parsed.as_of, 64),
            totals: UsageRequestsTotals {
                requests: totals.requests,
                prompt_tokens: totals.prompt_tokens,
                cached_tokens: totals.cached_tokens,
                noncached_prompt_tokens: totals.noncached_prompt_tokens,
                completion_tokens: totals.completion_tokens,
                reasoning_tokens: totals.reasoning_tokens,
                cost_micros: totals.cost_micros,
            },
            requests,
            next_cursor,
        };
        (IdentityResult::Ok, page, String::new())
    }

    pub fn fetch_models(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<ModelInfo>, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                Vec::new(),
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/models"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                Vec::new(),
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                Vec::new(),
                http_error("models request", &origin, &response, access_token),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let parsed = match contract::ModelList::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    Vec::new(),
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        let models = parsed
            .data
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                context_window: model.max_input_tokens.unwrap_or(0),
                max_output_tokens: model.max_output_tokens.unwrap_or(0),
            })
            .collect();
        (IdentityResult::Ok, models, String::new())
    }

    pub fn fetch_catalog(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<CatalogPrice>, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                IdentityResult::Failed,
                Vec::new(),
                "no access token is available".to_string(),
            );
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let request = HttpRequest {
            method: "GET".to_string(),
            url: format!("{origin}/v1/models/catalog"),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms: 0,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        if response.status == 401 {
            return (
                IdentityResult::Unauthorized,
                Vec::new(),
                "console session expired".to_string(),
            );
        }
        if response.status != 200 {
            return (
                IdentityResult::Failed,
                Vec::new(),
                http_error("model catalog request", &origin, &response, access_token),
            );
        }
        let object = match parse_object(&response) {
            Ok(object) => object,
            Err(error) => return (IdentityResult::Failed, Vec::new(), error),
        };
        let parsed = match contract::ModelCatalogResponse::from_json(&object) {
            Ok(parsed) => parsed,
            Err(_) => {
                return (
                    IdentityResult::Failed,
                    Vec::new(),
                    CONTRACT_MISMATCH.to_string(),
                )
            }
        };
        let prices = parsed
            .models
            .into_iter()
            .map(|model| CatalogPrice {
                id: model.id,
                input_per_mtok: model.input_per_mtok,
                output_per_mtok: model.output_per_mtok,
            })
            .collect();
        (IdentityResult::Ok, prices, String::new())
    }

    /// Returns the outcome and, when it failed, why.
    pub fn cancel_request(
        &self,
        console_url: &str,
        access_token: &str,
        request_id: &str,
        timeout_ms: i32,
    ) -> (CancelOutcome, String) {
        if !super::session_token_is_safe(access_token) {
            return (
                CancelOutcome::Failed,
                "no access token is available".to_string(),
            );
        }
        // The id is the server's own (its x-request-id); it is still
        // user-facing input to a path, so it is escaped rather than trusted.
        if request_id.is_empty() {
            return (CancelOutcome::Failed, "no request id to cancel".to_string());
        }
        let origin = match console_origin(console_url) {
            Ok(origin) => origin,
            Err(error) => return (CancelOutcome::Failed, error),
        };
        let request = HttpRequest {
            method: "POST".to_string(),
            url: format!("{origin}/v1/requests/{}/cancel", query_escape(request_id)),
            body: String::new(),
            bearer_token: access_token.to_string(),
            timeout_ms,
        };
        let response = match self.send(request) {
            Ok(response) => response,
            Err(error) => return (CancelOutcome::Failed, error),
        };
        if response.status == 202 {
            // The body is informational (the id echoed, status "cancelling");
            // a server that answered 202 has already done the work, so a body
            // this binding cannot read is ignored, not treated as a failure.
            if let Ok(object) = parse_object(&response) {
                let _ = contract::CancelRequestResponse::from_json(&object);
            }
            return (CancelOutcome::Cancelled, String::new());
        }
        if response.status == 404 {
            return (CancelOutcome::NotFound, String::new());
        }
        (
            CancelOutcome::Failed,
            http_error("cancel request", &origin, &response, access_token),
        )
    }
}

// The C++ free-function forms, over the real transport.
pub fn begin_authorization(console_url: &str, hostname: &str) -> Result<Authorization, String> {
    ConsoleClient::default().begin_authorization(console_url, hostname, None)
}

pub fn poll(console_url: &str, authorization: &Authorization) -> PollOutcome {
    ConsoleClient::default().poll(console_url, authorization)
}

pub fn refresh(console_url: &str, refresh_token: &str) -> Result<Grant, String> {
    ConsoleClient::default()
        .refresh(console_url, refresh_token)
        .map_err(|e| e.message)
}

pub fn who_am_i(console_url: &str, token: &str) -> Result<Identity, String> {
    match ConsoleClient::default().who_am_i(console_url, token) {
        (IdentityResult::Ok, identity, _) => Ok(identity),
        (_, _, error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    #[test]
    fn console_tls_uses_the_platform_stack_and_trust_store() {
        let tls = super::console_tls_config();
        assert_eq!(tls.provider(), ureq::tls::TlsProvider::NativeTls);
        assert!(matches!(
            tls.root_certs(),
            ureq::tls::RootCerts::PlatformVerifier
        ));
    }

    // remaining_after_discovery_ms: the request timeout budget left after
    // proxy discovery (WPAD/PAC on Windows) already spent part of it. Pure
    // and platform-independent, so it is covered here even though the
    // Windows-only discovery it protects cannot run on this host.

    #[test]
    fn remaining_after_discovery_ms_is_unchanged_when_discovery_was_instant() {
        let remaining = super::remaining_after_discovery_ms(30_000, Duration::ZERO);
        assert_eq!(remaining, 30_000);
    }

    #[test]
    fn remaining_after_discovery_ms_subtracts_what_discovery_spent() {
        // Discovery's own 3s bound consumed against the request's 30s
        // default -- without this the request would still get the full 30s
        // on top, per cubic review comment #66.
        let remaining = super::remaining_after_discovery_ms(30_000, Duration::from_millis(3_000));
        assert_eq!(remaining, 27_000);
    }

    #[test]
    fn remaining_after_discovery_ms_never_drops_below_the_floor() {
        // A short request timeout plus a slow discovery must not leave the
        // request with zero or negative time to run.
        let remaining = super::remaining_after_discovery_ms(2_000, Duration::from_millis(5_000));
        assert_eq!(remaining, super::MIN_REMAINING_TOTAL_TIMEOUT_MS);
    }

    #[test]
    fn remaining_after_discovery_ms_never_exceeds_the_original_total() {
        // A caller-configured timeout below the floor, with discovery taking
        // no time at all (macOS/Linux never run discovery; a Windows cache
        // hit is instant), must come back unchanged -- the floor exists to
        // protect against discovery eating into the budget, not to inflate
        // a budget discovery never touched.
        let remaining = super::remaining_after_discovery_ms(500, Duration::ZERO);
        assert_eq!(remaining, 500);
    }

    // resolve_proxy_url / proxy_env_value: libcurl proxy-selection parity.
    // Closure-driven env lookup, never touching real process env, so these
    // stay hermetic and safe under cargo test's parallel threads.

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn lookup(env: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
        move |name: &str| env.get(name).cloned()
    }

    #[test]
    fn loopback_urls_never_use_a_proxy_even_when_a_matching_env_var_is_set() {
        let env = env_of(&[("http_proxy", "http://proxy.example:8080")]);
        let resolved = super::resolve_proxy_url("http://127.0.0.1:9999/x", &lookup(&env));
        assert_eq!(resolved, None);
    }

    #[test]
    fn https_scheme_reads_the_lowercase_https_proxy_first() {
        let env = env_of(&[("https_proxy", "http://proxy.example:8080")]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080"));
    }

    #[test]
    fn https_scheme_falls_back_to_uppercase_https_proxy_when_lowercase_is_unset() {
        let env = env_of(&[("HTTPS_PROXY", "http://proxy.example:8080")]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080"));
    }

    #[test]
    fn http_scheme_only_reads_the_lowercase_http_proxy_and_ignores_the_uppercase_form() {
        // libcurl deliberately never reads uppercase HTTP_PROXY (httpoxy,
        // CVE-2016-5385); ureq::Proxy::try_from_env() does read it, which is
        // exactly the gap this function closes.
        let env = env_of(&[("HTTP_PROXY", "http://proxy.example:8080")]);
        let resolved = super::resolve_proxy_url("http://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved, None);
    }

    #[test]
    fn scheme_specific_proxy_takes_priority_over_all_proxy() {
        let env = env_of(&[
            ("https_proxy", "http://scheme-specific:8080"),
            ("all_proxy", "http://fallback:9090"),
        ]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://scheme-specific:8080"));
    }

    #[test]
    fn all_proxy_is_used_when_no_scheme_specific_var_is_set() {
        let env = env_of(&[("all_proxy", "http://fallback:9090")]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://fallback:9090"));
    }

    #[test]
    fn uppercase_all_proxy_is_used_when_lowercase_is_unset() {
        let env = env_of(&[("ALL_PROXY", "http://fallback:9090")]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://fallback:9090"));
    }

    #[test]
    fn an_explicit_empty_scheme_specific_value_disables_the_proxy_without_falling_back_to_all_proxy(
    ) {
        let env = env_of(&[("https_proxy", ""), ("all_proxy", "http://fallback:9090")]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved, None);
    }

    #[test]
    fn no_proxy_env_var_is_never_consulted() {
        // The C++ pins CURLOPT_NOPROXY to the loopback hosts only, which
        // replaces (not merges with) whatever NO_PROXY says -- so a
        // non-loopback host still gets the configured proxy even if it
        // appears in NO_PROXY.
        let env = env_of(&[
            ("https_proxy", "http://proxy.example:8080"),
            ("NO_PROXY", "*"),
        ]);
        let resolved = super::resolve_proxy_url("https://console.example/v1/me", &lookup(&env));
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080"));
    }

    // static_proxy_for_scheme: Windows' ProxyServer registry value format.

    #[test]
    fn static_proxy_for_scheme_returns_the_bare_value_for_every_scheme_when_there_is_no_per_protocol_list(
    ) {
        assert_eq!(
            super::static_proxy_for_scheme("proxy.example:8080", "https").as_deref(),
            Some("proxy.example:8080")
        );
        assert_eq!(
            super::static_proxy_for_scheme("proxy.example:8080", "http").as_deref(),
            Some("proxy.example:8080")
        );
    }

    #[test]
    fn static_proxy_for_scheme_picks_the_matching_protocol_entry_from_a_semicolon_list() {
        let value = "http=proxy1:8080;https=proxy2:8443;ftp=proxy3:21";
        assert_eq!(
            super::static_proxy_for_scheme(value, "https").as_deref(),
            Some("proxy2:8443")
        );
        assert_eq!(
            super::static_proxy_for_scheme(value, "http").as_deref(),
            Some("proxy1:8080")
        );
    }

    #[test]
    fn static_proxy_for_scheme_returns_none_when_the_protocol_is_not_listed() {
        let value = "http=proxy1:8080;ftp=proxy3:21";
        assert_eq!(super::static_proxy_for_scheme(value, "https"), None);
    }

    #[test]
    fn static_proxy_for_scheme_treats_an_empty_value_as_no_proxy() {
        assert_eq!(super::static_proxy_for_scheme("", "https"), None);
        assert_eq!(
            super::static_proxy_for_scheme("http=proxy1:8080;https=", "https"),
            None
        );
    }

    // console_proxy_url: the per-platform dispatch. `is_windows` is a plain
    // parameter (not cfg!(windows)) precisely so both rule sets are exercised
    // here regardless of which platform runs the test suite.

    fn no_static_proxy(_scheme: &str) -> Option<String> {
        None
    }

    fn no_autodetected_proxy(_url: &str) -> super::AutoProxy {
        super::AutoProxy::NotConfigured
    }

    #[test]
    fn non_windows_ignores_the_static_system_proxy_and_follows_the_env_rules() {
        let env = env_of(&[("https_proxy", "http://proxy.example:8080")]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            false,
            &lookup(&env),
            &no_autodetected_proxy,
            &|scheme| Some(format!("static-{scheme}:9")),
        );
        assert_eq!(resolved.as_deref(), Some("http://proxy.example:8080"));
    }

    #[test]
    fn non_windows_with_no_env_proxy_configured_ignores_the_static_system_proxy_too() {
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            false,
            &lookup(&env),
            &no_autodetected_proxy,
            &|scheme| Some(format!("static-{scheme}:9")),
        );
        assert_eq!(resolved, None);
    }

    #[test]
    fn windows_ignores_proxy_env_vars_entirely_and_uses_the_static_system_proxy() {
        let env = env_of(&[
            ("https_proxy", "http://from-env:8080"),
            ("all_proxy", "http://from-env-all:9090"),
        ]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            true,
            &lookup(&env),
            &no_autodetected_proxy,
            &|scheme| Some(format!("static-{scheme}:9")),
        );
        assert_eq!(resolved.as_deref(), Some("static-https:9"));
    }

    #[test]
    fn windows_with_no_static_system_proxy_configured_goes_direct_even_with_env_vars_set() {
        let env = env_of(&[("https_proxy", "http://from-env:8080")]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            true,
            &lookup(&env),
            &no_autodetected_proxy,
            &no_static_proxy,
        );
        assert_eq!(resolved, None);
    }

    #[test]
    fn windows_loopback_urls_go_direct_even_when_a_static_system_proxy_is_configured() {
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "http://127.0.0.1:9999/x",
            true,
            &lookup(&env),
            &no_autodetected_proxy,
            &|scheme| Some(format!("static-{scheme}:9")),
        );
        assert_eq!(resolved, None);
    }

    #[test]
    fn windows_uses_the_scheme_specific_static_system_proxy_entry() {
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "http://console.example/v1/me",
            true,
            &lookup(&env),
            &no_autodetected_proxy,
            &|scheme| super::static_proxy_for_scheme("http=proxy1:8080;https=proxy2:8443", scheme),
        );
        assert_eq!(resolved.as_deref(), Some("proxy1:8080"));
    }

    #[test]
    fn windows_prefers_the_autodetected_pac_wpad_proxy_over_the_static_one() {
        // A PAC/WPAD-only managed network: auto-detect is on, there is no
        // static ProxyServer entry, and no proxy env var is set either --
        // exactly the case cubic review comment #9 flagged as falling
        // through to direct. The fake autoproxy resolver stands in for
        // WinHttpGetIEProxyConfigForCurrentUser + WinHttpGetProxyForUrl.
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            true,
            &lookup(&env),
            &|url| {
                assert_eq!(url, "https://console.example/v1/me");
                super::AutoProxy::Proxy("pac-resolved-proxy:8080".to_string())
            },
            &no_static_proxy,
        );
        assert_eq!(resolved.as_deref(), Some("pac-resolved-proxy:8080"));
    }

    #[test]
    fn windows_goes_direct_when_the_pac_script_says_direct_even_with_a_static_proxy() {
        // WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY honours a PAC DIRECT answer; the
        // static Internet Options proxy is only the fallback when discovery is
        // off or fails.
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "https://console.example/v1/me",
            true,
            &lookup(&env),
            &|_url| super::AutoProxy::Direct,
            &|scheme| Some(format!("static-{scheme}:9")),
        );
        assert_eq!(resolved, None);
    }

    #[test]
    fn windows_loopback_urls_go_direct_without_even_trying_pac_wpad_autodetection() {
        let env = env_of(&[]);
        let resolved = super::console_proxy_url(
            "http://127.0.0.1:9999/x",
            true,
            &lookup(&env),
            &|_url| panic!("autodetection must not run for a loopback URL"),
            &no_static_proxy,
        );
        assert_eq!(resolved, None);
    }
}
