//! `wally serve [model]` -- OpenAI-compatible local HTTP server. Port of
//! src/commands/cmd_serve.cpp.
//!
//! Wraps the existing commons rac_server (include/rac/server/rac_server.h --
//! same engine behind tools/runanywhere-server.cpp). Scope inherited from
//! rac_server: LLM only, ONE model per server process. Model refs resolve
//! through the same catalog/registry path as every other command, with
//! auto-pull when missing.
//!
//! `RAC_SERVER_CONFIG_DEFAULT` is a header-only `static const` in
//! rac_server.h: bindgen declares it as an `extern static` (src/sys/bindings.rs),
//! but it is not in any kit archive, so referencing it compiles under
//! `cargo check` and then fails at link. `default_server_config()` below
//! builds the same values as a plain Rust literal instead, with each field
//! commented against the header default it mirrors.

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, ValueType};
use crate::io::output::{describe_result, error_line, status_line};
use crate::sys;

#[cfg(wally_has_server)]
use crate::cli_formatter::{examples_footer, Example};
#[cfg(wally_has_server)]
use crate::commands::model_setup::ensure_model_ready;
#[cfg(wally_has_server)]
use std::ffi::CString;
#[cfg(wally_has_server)]
use std::sync::atomic::{AtomicBool, Ordering};

const DEFAULT_SERVE_MODEL: &str = "qwen3-4b-instruct-2507";

#[cfg(wally_has_server)]
// Async-signal-safe shutdown: the ctrlc handler only sets a flag; the main
// thread polls and performs the actual stop. Calling rac_server_stop() from
// signal context (the tools/runanywhere-server.cpp pattern) deadlocks -- the
// handler would interrupt rac_server_wait() on the same thread and re-enter
// its mutex.
static SERVE_STOP: AtomicBool = AtomicBool::new(false);

/// Literal mirror of `RAC_SERVER_CONFIG_DEFAULT` from rac_server.h. See the
/// module doc comment for why this cannot reference the bindgen extern
/// static directly.
///
/// `host` and `cors_origins` point at `'static` C string literals rather
/// than null: the header default is the string `"127.0.0.1"` / `"*"`, not
/// `RAC_NULL`, and `run_serve` always overrides `host` from the CLI but
/// never touches `cors_origins`, so a null default there would reach
/// `rac_server_start` unlike the header's C++ callers.
#[cfg(wally_has_server)]
fn default_server_config() -> sys::rac_server_config_t {
    sys::rac_server_config_t {
        host: c"127.0.0.1".as_ptr(), // header default: "127.0.0.1" (overridden below from CLI)
        port: 8080,                  // header default: 8080
        model_path: std::ptr::null(), // header default: RAC_NULL (required, set below)
        model_id: std::ptr::null(),  // header default: RAC_NULL (set below)
        context_size: 8192,          // header default: 8192
        threads: 4,                  // header default: 4
        gpu_layers: i32::MIN,        // header default: RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO (INT32_MIN)
        enable_cors: sys::RAC_TRUE as sys::rac_bool_t, // header default: RAC_TRUE
        cors_origins: c"*".as_ptr(), // header default: "*" (never overridden by this CLI)
        request_timeout_seconds: 300, // header default: 300 (5 minutes)
        max_concurrent_requests: 4,  // header default: 4
        verbose: sys::RAC_FALSE as sys::rac_bool_t, // header default: RAC_FALSE
    }
}

#[cfg(wally_has_server)]
fn serve_signal_handler() {
    SERVE_STOP.store(true, Ordering::SeqCst);
}

#[cfg(wally_has_server)]
#[allow(clippy::too_many_arguments)]
fn run_serve(
    options: &GlobalOptions,
    reference: &str,
    host: &str,
    port: u16,
    context_size: i32,
    threads: i32,
    gpu_layers: i32,
    cors: bool,
) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    let model = match ensure_model_ready(
        options,
        if reference.is_empty() {
            DEFAULT_SERVE_MODEL
        } else {
            reference
        },
    ) {
        Ok(model) => model,
        Err(code) => return code,
    };

    let host_c = match CString::new(host) {
        Ok(value) => value,
        Err(_) => {
            error_line("--host contains an embedded NUL byte");
            return 2;
        }
    };
    let model_path_c = match CString::new(model.primary_path.as_str()) {
        Ok(value) => value,
        Err(_) => {
            error_line("resolved model path contains an embedded NUL byte");
            return 1;
        }
    };
    let model_id_c = match CString::new(model.model_id.as_str()) {
        Ok(value) => value,
        Err(_) => {
            error_line("model id contains an embedded NUL byte");
            return 1;
        }
    };

    let mut config = default_server_config();
    config.host = host_c.as_ptr();
    config.port = port;
    config.model_path = model_path_c.as_ptr();
    config.model_id = model_id_c.as_ptr();
    config.context_size = context_size;
    config.threads = threads;
    config.gpu_layers = gpu_layers;
    config.enable_cors = if cors {
        sys::RAC_TRUE as sys::rac_bool_t
    } else {
        sys::RAC_FALSE as sys::rac_bool_t
    };
    config.verbose = if options.verbose {
        sys::RAC_TRUE as sys::rac_bool_t
    } else {
        sys::RAC_FALSE as sys::rac_bool_t
    };

    // SAFETY: config is a fully-populated, live rac_server_config_t; every
    // pointer field (host_c, model_path_c, model_id_c) outlives this call.
    let rc = unsafe { sys::rac_server_start(&config) };
    if rc < 0 {
        error_line(&format!("server start failed: {}", describe_result(rc)));
        return 1;
    }

    status_line(&format!(
        "serving {} (LLM-only, single model)",
        model.model_id
    ));
    status_line(&format!(
        "OpenAI API: http://{host}:{port}/v1/chat/completions"
    ));
    status_line(&format!("health:     http://{host}:{port}/health"));
    status_line("Ctrl-C to stop");

    SERVE_STOP.store(false, Ordering::SeqCst);
    // Errors are ignored, matching C++'s std::signal() return value (SIG_ERR)
    // never being checked either: if a handler is already installed
    // elsewhere in the process, Ctrl-C simply won't stop the server here (it
    // still stops on SIGTERM via ctrlc's own combined handling on Unix, and
    // the process can still be killed).
    let _ = ctrlc::set_handler(serve_signal_handler);
    // ctrlc's "termination" feature (needed for SIGTERM) also installs the
    // same handler for SIGHUP on unix, but C++ only installs SIGINT/SIGTERM
    // and leaves SIGHUP at its default disposition (terminate). Restore that
    // default so `kill -HUP <pid>` still kills the process outright here too,
    // instead of running the graceful-shutdown path C++ never takes on
    // SIGHUP.
    #[cfg(unix)]
    // SAFETY: SIGHUP and SIG_DFL are both valid libc constants; signal() with
    // a valid signal number and disposition is always safe to call.
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
    }
    // SAFETY: rac_server_is_running() takes no arguments and only reads
    // internal server state.
    while !SERVE_STOP.load(Ordering::SeqCst)
        && unsafe { sys::rac_server_is_running() } == sys::RAC_TRUE as sys::rac_bool_t
    {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    // SAFETY: no arguments; stops the server started above.
    unsafe {
        sys::rac_server_stop();
    }
    // SAFETY: no arguments; blocks until the server thread has fully exited.
    let exit_code = unsafe { sys::rac_server_wait() };

    // C++'s `if (RAC_SUCCEEDED(rac_server_get_status(&status)) &&
    // options.verbose)` short-circuits left-to-right, so the call itself
    // always happens -- only the subsequent status_line print is gated on
    // --verbose, its result simply discarded otherwise. Call it
    // unconditionally here too so both builds make the same SDK calls.
    let mut status = std::mem::MaybeUninit::<sys::rac_server_status_t>::zeroed();
    // SAFETY: status is a valid, zeroed-out rac_server_status_t the call
    // fills in; read only after a successful (non-negative) result.
    let status_rc = unsafe { sys::rac_server_get_status(status.as_mut_ptr()) };
    if status_rc >= 0 && options.verbose {
        // SAFETY: status_rc >= 0 means rac_server_get_status fully
        // initialized every field before returning.
        let status = unsafe { status.assume_init() };
        status_line(&format!(
            "requests: {}, tokens: {}, uptime: {}s",
            status.total_requests, status.total_tokens_generated, status.uptime_seconds
        ));
    }
    exit_code
}

pub fn register_serve(app: &mut App) {
    let cmd = app.add_subcommand("serve", "Serve a model over an OpenAI-compatible API");
    #[cfg(wally_has_server)]
    {
        cmd.footer(&examples_footer(&[
            Example::new("wally serve qwen3-4b-instruct-2507", ""),
            Example::new("wally serve granite-4.2-8b --port 8000", ""),
        ]));
        cmd.add_option(
            "model",
            ValueType::Text,
            &format!("Model to serve (default {DEFAULT_SERVE_MODEL})"),
        );
        cmd.add_option(
            "--host,-H",
            ValueType::Text,
            "Address to bind (default 127.0.0.1)",
        )
        .default_val("127.0.0.1");
        cmd.add_option(
            "--port,-p",
            ValueType::UInt,
            "Port to listen on (default 8080)",
        )
        // cmd_serve.cpp binds this to a uint16_t: 70000/4294967296 are
        // conversion errors even though they'd fit a plain uint32_t.
        .int_bounds(0, i128::from(u16::MAX))
        .default_val("8080");
        cmd.add_option(
            "--context-length,--context,-c",
            ValueType::Int,
            "Context window in tokens (default 8192)",
        )
        .default_val("8192");
        cmd.add_option(
            "--threads,-t",
            ValueType::Int,
            "Inference threads (default 4)",
        )
        .default_val("4");
        cmd.add_option(
            "--gpu-layers,--ngl",
            ValueType::Int,
            "Layers to offload to the GPU",
        )
        .default_val("0");
        cmd.add_flag(
            "--cors",
            "Allow cross-origin browser requests (off by default)",
        );
        cmd.callback(move |p, options| {
            let reference = p.get_str("model").unwrap_or_default();
            let host = p
                .get_str("--host")
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let port = p.get_u64("--port").unwrap_or(8080) as u16;
            let context = p.get_i64("--context-length").unwrap_or(8192) as i32;
            let threads = p.get_i64("--threads").unwrap_or(4) as i32;
            let gpu_layers = p.get_i64("--gpu-layers").unwrap_or(0) as i32;
            let cors = p.flag("--cors");
            run_serve(
                options, &reference, &host, port, context, threads, gpu_layers, cors,
            )
        });
    }
    #[cfg(not(wally_has_server))]
    {
        cmd.callback(move |_p, _options| {
            error_line("this wally build does not include the server (RAC_BUILD_SERVER=OFF)");
            1
        });
    }
}

#[cfg(all(test, wally_has_server))]
mod tests {
    use super::*;
    use std::ffi::CStr;

    // Pins default_server_config() to rac_server.h's RAC_SERVER_CONFIG_DEFAULT
    // field by field. host and cors_origins previously defaulted to null,
    // which read as "unset" rather than the header's actual "127.0.0.1" /
    // "*" string literals -- host is always overridden by the CLI before
    // rac_server_start(), but cors_origins never is, so that mismatch would
    // have reached the SDK call.
    #[test]
    fn default_server_config_matches_header_default() {
        let config = default_server_config();
        // SAFETY: default_server_config() sets host/cors_origins to static
        // NUL-terminated string literals.
        unsafe {
            assert_eq!(CStr::from_ptr(config.host).to_str().unwrap(), "127.0.0.1");
            assert_eq!(CStr::from_ptr(config.cors_origins).to_str().unwrap(), "*");
        }
        assert_eq!(config.port, 8080);
        assert!(config.model_path.is_null());
        assert!(config.model_id.is_null());
        assert_eq!(config.context_size, 8192);
        assert_eq!(config.threads, 4);
        assert_eq!(config.gpu_layers, i32::MIN); // RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO
        assert_eq!(config.enable_cors, sys::RAC_TRUE as sys::rac_bool_t);
        assert_eq!(config.request_timeout_seconds, 300);
        assert_eq!(config.max_concurrent_requests, 4);
        assert_eq!(config.verbose, sys::RAC_FALSE as sys::rac_bool_t);
    }
}
