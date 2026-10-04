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

#[cfg(wally_has_server)]
fn serve_gpu_layers(value: Option<i64>) -> Result<i32, &'static str> {
    match value {
        None => Ok(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO),
        Some(-1) => Ok(-1),
        Some(0) => Ok(0),
        Some(value) if value == i64::from(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO) => {
            Ok(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO)
        }
        Some(_) => {
            Err("--gpu-layers accepts -1 (GPU) or 0 (CPU); omit it for automatic placement")
        }
    }
}

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

    // Decision checkpoints get their own endpoint: rac_server is LLM-only,
    // so a decision model reference serves POST /v1/decisions in-process.
    if !reference.is_empty() && is_decision_reference(reference) {
        return run_serve_decisions(options, reference, host, port);
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
    // Held until the server has stopped. An auto-pull before this point had
    // Ctrl-C only while it downloaded, so this action is the one that runs.
    let _interrupt = crate::util::interrupt::on_interrupt(serve_signal_handler);
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

/// A catalog id or alias naming an on-disk decision model (clef rows). Paths
/// to GGUF files are not covered: they skip the catalog and stay on the
/// rac_server path, which refuses them the way it always has.
#[cfg(wally_has_server)]
fn is_decision_reference(reference: &str) -> bool {
    matches!(
        crate::catalog::find(reference),
        Some(entry) if entry.category == crate::io::proto::v1::ModelCategory::Decision
    )
}

/// Same 1 MiB cap the CLI enforces on request bodies.
#[cfg(wally_has_server)]
const MAX_SERVE_BODY_BYTES: usize = 1024 * 1024;

/// Split an HTTP/1 head into method, path (query stripped) and content
/// length. Header names match case-insensitively; anything unreadable is a
/// 400 with the reason.
#[cfg(wally_has_server)]
fn parse_head(head: &[u8]) -> Result<(String, String, usize), String> {
    let text = std::str::from_utf8(head).map_err(|_| "unreadable request head".to_string())?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| "empty request".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "bad request line".to_string())?
        .to_string();
    let target = parts.next().ok_or_else(|| "bad request line".to_string())?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    let mut length: usize = 0;
    for line in lines {
        if let Some(value) = line
            .split_once(':')
            .filter(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value)
        {
            length = value
                .trim()
                .parse()
                .map_err(|_| "bad content-length".to_string())?;
        }
    }
    Ok((method, path, length))
}

#[cfg(wally_has_server)]
fn write_response(stream: &mut std::net::TcpStream, status: u16, reason: &str, body: &str) {
    use std::io::Write;
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

#[cfg(wally_has_server)]
fn json_error(message: &str) -> String {
    let mut json = crate::io::output::JsonWriter::new();
    json.begin_object().field_str("error", message).end_object();
    json.str().to_string()
}

/// One connection, served synchronously: the decision handle is not shared
/// across threads, so requests queue instead of racing inside the engine.
#[cfg(wally_has_server)]
fn handle_decisions_connection(
    stream: &mut std::net::TcpStream,
    loaded: &crate::commands::cmd_decisions::LoadedDecisions,
) {
    use std::io::Read;
    let mut raw: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                raw.extend_from_slice(&buf[..n]);
                if raw.len() > 65536 {
                    write_response(
                        stream,
                        431,
                        "Request Header Fields Too Large",
                        &json_error("headers too large"),
                    );
                    return;
                }
                if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            }
            Err(_) => return,
        }
    };
    let (method, path, length) = match parse_head(&raw[..head_end]) {
        Ok(parsed) => parsed,
        Err(message) => {
            write_response(stream, 400, "Bad Request", &json_error(&message));
            return;
        }
    };
    if path == "/health" && method == "GET" {
        let mut json = crate::io::output::JsonWriter::new();
        json.begin_object().field_str("status", "ok").end_object();
        write_response(stream, 200, "OK", json.str());
        return;
    }
    if method != "POST" || path != "/v1/decisions" {
        write_response(stream, 404, "Not Found", &json_error("not found"));
        return;
    }
    if length > MAX_SERVE_BODY_BYTES {
        write_response(
            stream,
            413,
            "Content Too Large",
            &json_error("request body exceeds 1 MiB"),
        );
        return;
    }
    let mut body = raw[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    if body.len() < length {
        write_response(
            stream,
            400,
            "Bad Request",
            &json_error("truncated request body"),
        );
        return;
    }
    body.truncate(length);
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            write_response(
                stream,
                400,
                "Bad Request",
                &json_error("invalid request JSON"),
            );
            return;
        }
    };
    let unknown = crate::commands::cmd_decisions::unknown_request_fields(&value);
    if !unknown.is_empty() {
        write_response(
            stream,
            400,
            "Bad Request",
            &json_error(&format!(
                "request has fields the decisions contract does not define: {}",
                unknown.join(", ")
            )),
        );
        return;
    }
    let request = match crate::account::decisions_contract::DecisionsRequest::from_json(&value) {
        Ok(request) => request,
        Err(error) => {
            write_response(
                stream,
                400,
                "Bad Request",
                &json_error(&format!(
                    "request does not match the decisions contract: {error}"
                )),
            );
            return;
        }
    };
    if let Err(error) = crate::commands::cmd_decisions::validate(&request) {
        write_response(stream, 400, "Bad Request", &json_error(&error));
        return;
    }
    // The served model answers every request; a body naming another model is
    // scored by this server's model, the way `serve` owns its one model.
    match crate::commands::cmd_decisions::score_loaded(loaded, &request) {
        Ok(response) => {
            write_response(
                stream,
                200,
                "OK",
                &crate::io::json::dump(&response.to_json()),
            );
        }
        Err(message) => {
            write_response(stream, 500, "Internal Server Error", &json_error(&message));
        }
    }
}

/// Serve a local decision checkpoint over HTTP: one model loaded once, then
/// POST /v1/decisions per request plus GET /health. Mirrors the CLI's
/// validation so the same body behaves the same in both.
#[cfg(wally_has_server)]
fn run_serve_decisions(options: &GlobalOptions, reference: &str, host: &str, port: u16) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    let model = match ensure_model_ready(options, reference) {
        Ok(model) => model,
        Err(code) => return code,
    };
    let loaded = match crate::commands::cmd_decisions::load_decisions_component(&model) {
        Ok(loaded) => loaded,
        Err(message) => {
            error_line(&message);
            return 1;
        }
    };

    let listener = match std::net::TcpListener::bind((host, port)) {
        Ok(listener) => listener,
        Err(error) => {
            error_line(&format!("could not bind {host}:{port}: {error}"));
            return 1;
        }
    };
    if listener.set_nonblocking(true).is_err() {
        error_line("could not set the listener non-blocking");
        return 1;
    }

    status_line(&format!(
        "serving {} (decision model, single model)",
        model.model_id
    ));
    status_line(&format!("Decisions API: http://{host}:{port}/v1/decisions"));
    status_line(&format!("health:        http://{host}:{port}/health"));
    status_line("Ctrl-C to stop");

    SERVE_STOP.store(false, Ordering::SeqCst);
    let _interrupt = crate::util::interrupt::on_interrupt(serve_signal_handler);
    loop {
        if SERVE_STOP.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                handle_decisions_connection(&mut stream, &loaded);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(error) => {
                error_line(&format!("accept failed: {error}"));
                break;
            }
        }
    }
    0
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
            "GPU placement: -1 all, 0 CPU (default auto)",
        );
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
            let gpu_layers = match serve_gpu_layers(p.get_i64("--gpu-layers")) {
                Ok(value) => value,
                Err(message) => {
                    error_line(message);
                    return 2;
                }
            };
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

    #[test]
    fn head_parses_method_path_and_length() {
        let (method, path, length) =
            parse_head(b"POST /v1/decisions?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 12\r\n\r\n")
                .expect("head parses");
        assert_eq!(method, "POST");
        assert_eq!(path, "/v1/decisions");
        assert_eq!(length, 12);
    }

    #[test]
    fn head_rejects_garbage() {
        assert!(parse_head(b"").is_err());
        assert!(parse_head(b"GET\r\n\r\n").is_err());
        assert!(parse_head(b"POST /x HTTP/1.1\r\nContent-Length: lots\r\n\r\n").is_err());
    }

    #[test]
    fn decision_ids_route_to_the_decisions_server() {
        // Catalog rows are platform-gated (Mlx on macOS only, LlamaCpp where
        // linked), so the routing test asserts per-build truth, not macOS truth.
        assert_eq!(
            is_decision_reference("clef-flash-mlx"),
            cfg!(target_os = "macos")
        );
        assert_eq!(
            is_decision_reference("clef-flash-9b"),
            cfg!(wally_has_llamacpp)
        );
        assert!(!is_decision_reference("qwen3-4b-instruct-2507"));
        assert!(!is_decision_reference("eve"));
        assert!(!is_decision_reference("no-such-model"));
    }

    #[test]
    fn serve_gpu_layers_uses_auto_unless_explicitly_overridden() {
        assert_eq!(
            serve_gpu_layers(None),
            Ok(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO)
        );
        assert_eq!(serve_gpu_layers(Some(0)), Ok(0));
        assert_eq!(serve_gpu_layers(Some(-1)), Ok(-1));
        assert_eq!(
            serve_gpu_layers(Some(i64::from(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO))),
            Ok(sys::RAC_LLM_LLAMACPP_GPU_LAYERS_AUTO)
        );
        assert!(serve_gpu_layers(Some(20)).is_err());
    }
}
