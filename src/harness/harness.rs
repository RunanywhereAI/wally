//! Endpoint resolution, cloud-session checks, tool install checks and the
//! launch (port of src/harness/harness.cpp).

use crate::account::{ConsoleClient, Credentials};

/// Where a coding tool is pointed: a hosted API or a local SDK server.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    /// Never logged.
    pub api_key: String,
    pub console_url: String,
    pub serving: bool,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("console_url", &self.console_url)
            .field("serving", &self.serving)
            .finish_non_exhaustive()
    }
}

pub fn model_id_is_safe(id: &str) -> bool {
    todo!("harness port: ModelIdIsSafe ({id})")
}

/// Result of `verify_cloud_session`: Ok(email) or the error, with `unverified`
/// true when the console could not be asked (rate limit, offline) rather than
/// refusing the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub message: String,
    pub unverified: bool,
}

/// Checks (and refreshes, if needed) the stored session against the console.
/// `credentials` may be updated in place by a refresh.
pub fn verify_cloud_session(
    console: &ConsoleClient,
    credentials: &mut Credentials,
) -> Result<String, SessionError> {
    let _ = (console, credentials);
    todo!("harness port: VerifyCloudSession")
}

/// Resolve `model` to an endpoint (hosted or local).
pub fn resolve(model: &str) -> Option<Endpoint> {
    todo!("harness port: Resolve ({model})")
}

pub fn release(endpoint: &Endpoint) {
    let _ = endpoint;
    todo!("harness port: Release")
}

pub fn ensure_installed(tool: &str) -> bool {
    todo!("harness port: EnsureInstalled ({tool})")
}

#[cfg(windows)]
pub fn quote_windows_arg(arg: &str) -> String {
    todo!("harness port: QuoteWindowsArg ({arg})")
}

#[cfg(windows)]
pub fn build_batch_command_line(script: &str, args: &[String]) -> Result<String, String> {
    todo!("harness port: BuildBatchCommandLine ({script}, {args:?})")
}

pub fn report_cloud_session_invalid(model: &str) {
    todo!("harness port: ReportCloudSessionInvalid ({model})")
}

pub fn report_not_signed_in() {
    todo!("harness port: ReportNotSignedIn")
}

pub fn refresh_and_recheck_model(credentials: &Credentials, model: &str) -> bool {
    let _ = credentials;
    todo!("harness port: RefreshAndRecheckModel ({model})")
}

/// Launch `tool` against `model`, forwarding `args`. Returns the exit code.
pub fn launch(tool: &str, model: &str, args: &[String]) -> i32 {
    todo!("harness port: Launch ({tool}, {model}, {args:?})")
}
