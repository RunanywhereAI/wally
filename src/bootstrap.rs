//! One-call SDK bring-up for every wally command (port of src/bootstrap.cpp).
//!
//! Mirrors the canonical bootstrap proven by the commons real-inference tests
//! with real desktop I/O:
//!
//!   desktop adapter → rac_model_paths_set_base_dir → rac_init →
//!   desktop HTTP transport → backend registration → catalog + discovery
//!
//! Commands call bootstrap() exactly once; it is idempotent within a process.
//! Owner: the SDK bootstrap / device info / progress port. Every callback handed
//! to the SDK must be `'static`, thread-safe and panic-free across the ABI; see
//! /tmp/wally-rust-migration/explore-bootstrap.md for the retained-callback and
//! init/shutdown-order contract.

use crate::sys;

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
    todo!("bootstrap port: resolve_connection ({options:?})")
}

/// Resolved environment after bootstrap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bootstrapped {
    /// RunAnywhere home (storage base dir)
    pub home: String,
    /// commons-derived models directory
    pub models_dir: String,
}

/// Initialize the SDK for CLI use. Logs go to stderr at WARNING by default
/// (DEBUG with --verbose, ERROR with --quiet). Returns the first failing step's
/// error code.
pub fn bootstrap(options: &GlobalOptions) -> Result<Bootstrapped, sys::rac_result_t> {
    todo!("bootstrap port: bootstrap ({options:?})")
}

/// rac_shutdown() wrapper; safe to call when bootstrap never ran.
pub fn shutdown() {
    todo!("bootstrap port: shutdown")
}

/// The process telemetry manager created by bootstrap() (null if telemetry was
/// not initialized). Exposed for the live telemetry integration test.
pub fn active_telemetry_manager() -> *mut sys::rac_telemetry_manager_t {
    todo!("bootstrap port: active_telemetry_manager")
}
