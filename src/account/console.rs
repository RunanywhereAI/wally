//! The console's CLI endpoints: device-flow sign-in, identity, usage, hosted
//! models and request cancellation (port of src/account/console.cpp). Requests
//! and responses go through the generated binding in console_contract.rs (P0
//! contract-first rule in AGENTS.md). Owner: the account port.
//!
//! The real transport is ureq with native-tls (the OS trust store, as curl and
//! WinHTTP used), env proxies with loopback bypass. Tests inject a `Transport`.

use std::collections::BTreeMap;
use std::sync::Arc;

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
    /// Seconds from a `Retry-After` header (delta or HTTP-date), else 0.
    pub fn retry_after_seconds(&self) -> i32 {
        todo!("account port: HttpResponse::retry_after_seconds")
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
pub fn next_poll_delay_seconds(interval: i32, retry_after: i32) -> i32 {
    todo!("account port: NextPollDelaySeconds ({interval}, {retry_after})")
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

impl ConsoleClient {
    pub fn new(transport: Option<Transport>) -> Self {
        ConsoleClient { transport }
    }

    /// `on_retry` runs before each retry of a refused/unreachable start.
    pub fn begin_authorization(
        &self,
        console_url: &str,
        hostname: &str,
        on_retry: Option<&dyn Fn()>,
    ) -> Result<Authorization, String> {
        let _ = (&self.transport, on_retry.is_some());
        todo!("account port: BeginAuthorization ({console_url}, {hostname})")
    }

    pub fn poll(&self, console_url: &str, authorization: &Authorization) -> PollOutcome {
        let _ = authorization;
        todo!("account port: Poll ({console_url})")
    }

    pub fn refresh(&self, console_url: &str, refresh_token: &str) -> Result<Grant, RefreshError> {
        let _ = refresh_token;
        todo!("account port: Refresh ({console_url})")
    }

    pub fn who_am_i(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Identity, String) {
        let _ = access_token;
        todo!("account port: WhoAmI ({console_url})")
    }

    pub fn revoke(
        &self,
        console_url: &str,
        access_token: &str,
        refresh_token: &str,
    ) -> Result<(), String> {
        let _ = (access_token, refresh_token);
        todo!("account port: Revoke ({console_url})")
    }

    pub fn fetch_usage(
        &self,
        console_url: &str,
        access_token: &str,
        query: &UsageQuery,
    ) -> (IdentityResult, Usage, String) {
        let _ = access_token;
        todo!("account port: FetchUsage ({console_url}, {query:?})")
    }

    pub fn fetch_models(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<ModelInfo>, String) {
        let _ = access_token;
        todo!("account port: FetchModels ({console_url})")
    }

    pub fn fetch_catalog(
        &self,
        console_url: &str,
        access_token: &str,
    ) -> (IdentityResult, Vec<CatalogPrice>, String) {
        let _ = access_token;
        todo!("account port: FetchCatalog ({console_url})")
    }

    /// Returns the outcome and, when it failed, why.
    pub fn cancel_request(
        &self,
        console_url: &str,
        access_token: &str,
        request_id: &str,
        timeout_ms: i32,
    ) -> (CancelOutcome, String) {
        let _ = access_token;
        todo!("account port: CancelRequest ({console_url}, {request_id}, {timeout_ms})")
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
