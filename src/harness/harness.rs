//! Endpoint resolution, cloud-session checks, tool install checks and the
//! launch (port of src/harness/harness.cpp).

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::account::{self, ConsoleClient, Credentials};
use crate::bootstrap::{self, GlobalOptions};
use crate::catalog;
use crate::cli_formatter::{cli_color, color_output_enabled};
use crate::commands;
use crate::io::output as out;
use crate::sys;
use crate::util::term;

use super::catalog_models::catalog_models_for;
use super::local_models::{local_context_size, local_models, local_output_size};
use super::opencode::build_open_code_config;

/// Where a coding tool is pointed: a hosted API or a local SDK server.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    /// Never logged.
    pub api_key: String,
    pub console_url: String,
    pub serving: bool,
    /// The context window the endpoint was actually loaded with: the local
    /// server's real allocation when `serving`, 0 for a hosted endpoint.
    pub context_window: i64,
    /// The output budget derived from `context_window`, 0 for a hosted
    /// endpoint.
    pub max_output: i64,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("console_url", &self.console_url)
            .field("serving", &self.serving)
            .field("context_window", &self.context_window)
            .field("max_output", &self.max_output)
            .finish_non_exhaustive()
    }
}

/// Whether `id` is safe to carry into a live editor/agent session: forwarded
/// into HTTP request bodies, environment variables, and config files built by
/// plain string concatenation with no escaping.
pub fn model_id_is_safe(id: &str) -> bool {
    if id.is_empty() || id.len() > 512 {
        return false;
    }
    for &byte in id.as_bytes() {
        if byte < 0x20 || byte == 0x7f {
            return false;
        }
        // `/` and `\` never appear in a real id — LocalModels() yields a bare
        // directory name — and `< > " ' &` are exactly what an unescaped XML
        // attribute or a shell word cannot survive.
        if matches!(byte, b'<' | b'>' | b'"' | b'\'' | b'&' | b'/' | b'\\') {
            return false;
        }
    }
    true
}

/// Result of `verify_cloud_session`: Ok(email) or the error, with `unverified`
/// true when the console could not be asked (rate limit, offline) rather than
/// refusing the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionError {
    pub message: String,
    pub unverified: bool,
}

fn epoch_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The refresh half of the same dance `wally account usage` uses: exchange the
/// refresh token for a new access token and persist it, so later commands in
/// the same session do not pay for the refresh again.
fn refresh_session(
    console: &ConsoleClient,
    credentials: &mut Credentials,
) -> Result<(), SessionError> {
    if credentials.refresh_token.is_empty() {
        return Err(SessionError {
            message: "the cloud session cannot be refreshed; run `wally account login`".to_string(),
            unverified: false,
        });
    }
    match console.refresh(&credentials.console_url, &credentials.refresh_token) {
        Ok(grant) => {
            credentials.access_token = grant.access_token;
            if !grant.refresh_token.is_empty() {
                credentials.refresh_token = grant.refresh_token;
            }
            credentials.expires_at = epoch_seconds()
                + if grant.expires_in > 0 {
                    grant.expires_in
                } else {
                    3600
                };
            account::save(credentials).map_err(|message| SessionError {
                message,
                unverified: false,
            })
        }
        Err(err) => Err(SessionError {
            message: err.message,
            unverified: err.unavailable,
        }),
    }
}

/// Checks (and refreshes, if needed) the stored session against the console.
/// `credentials` may be updated in place by a refresh.
pub fn verify_cloud_session(
    console: &ConsoleClient,
    credentials: &mut Credentials,
) -> Result<String, SessionError> {
    // An EXPIRED token refreshes first, and that refresh is itself a console
    // call that can be rate limited. This is the path a real user hits,
    // because tokens expire hourly: the refresh 429'd and the launch was
    // refused before the identity check below was ever reached
    // (InferenceInfra#444).
    if credentials.access_token_expired(epoch_seconds(), 60) {
        refresh_session(console, credentials)?;
    }
    let (mut result, mut identity, mut error) =
        console.who_am_i(&credentials.console_url, &credentials.access_token);
    if result == account::IdentityResult::Unauthorized {
        refresh_session(console, credentials)?;
        let (r2, i2, e2) = console.who_am_i(&credentials.console_url, &credentials.access_token);
        result = r2;
        identity = i2;
        error = e2;
    }
    if result != account::IdentityResult::Ok {
        // Separate "the console says this session is bad" from "the console
        // could not be asked". Only the first should stop a launch.
        return Err(SessionError {
            message: error,
            unverified: result == account::IdentityResult::Unavailable,
        });
    }
    Ok(identity.email)
}

/// Resolve `model` to an endpoint (hosted or local).
/// A model good enough to point a coding harness at, and nothing above the
/// window a machine can actually serve it at.
const MINIMUM_CODING_HARNESS_CONTEXT: i64 = 16384;

/// A port nothing is listening on, found by letting the OS pick one and
/// giving it straight back. There is a race between this returning and the
/// server binding, but the alternative is a fixed port that collides with a
/// second wally. `TcpListener::bind` does the same "AF_INET, bind to
/// 127.0.0.1:0, read back the assigned port" dance the C++ does by hand with
/// raw sockets (and, on Windows, WSAStartup) — the standard library already
/// carries that platform difference, so this needs none of it.
fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .unwrap_or(0)
}

/// Prompts on stderr and reads one line from stdin; true only for a
/// non-empty line starting with 'y' or 'Y'. Mirrors the C++
/// `std::getline(std::cin, answer) && !answer.empty() && ...` exactly: an
/// immediate EOF (`read_line` returns 0) and a bare newline (trims to empty)
/// both answer "no".
fn confirm_model_pull(model: &str) -> bool {
    eprint!("{model} is not installed. Download it now? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    let read = std::io::stdin().read_line(&mut answer).unwrap_or(0);
    if read == 0 {
        return false;
    }
    let trimmed = answer.trim_end_matches(['\n', '\r']);
    matches!(trimmed.chars().next(), Some('y') | Some('Y'))
}

/// Resolve `model` to an endpoint (hosted or local), starting a local server
/// when the resolved model lives on disk. `harness_command` names the
/// subcommand to suggest in the "not certified" hint (`opencode` when the
/// caller did not say).
pub fn resolve(model: &str, options: &GlobalOptions, harness_command: &str) -> Option<Endpoint> {
    if model.is_empty() {
        return None;
    }
    if !model_id_is_safe(model) {
        out::error_line(&format!("'{model}' is not a valid model id"));
        return None;
    }

    // The kit consumer brings the SDK up through bootstrap rather than the
    // CLI's own lazy Start(), and bootstrap is also what resolves the storage
    // home the model walk below needs.
    let env = bootstrap::bootstrap(options).ok()?;

    // The names a person types (`bonsai-27b`, `mlx-qwen3-0.6b`, `qwen3`) are
    // catalog ids, aliases and `models list` merge keys; the directory on disk
    // is the registry id (`mlx-qwen3-0.6b-4bit`). Accept every spelling the
    // catalog does, the same way `run` and `models pull` do, and prefer a
    // downloaded variant of a merged row over one that is not here.
    let mut wanted = vec![model.to_string()];
    if let Some(entry) = catalog::find(model) {
        wanted.push(entry.id.to_string());
    }
    for entry in catalog::all() {
        if catalog::merge_key_for(entry.id) == model {
            wanted.push(entry.id.to_string());
        }
    }

    // First spelling wins, except that a directory with no weight file in it
    // (a cancelled pull leaves the manifest and a `.part`) loses to any
    // variant whose weights are actually there. Beyond that the load below is
    // what decides whether the model opens; the walk cannot judge
    // completeness.
    let installed = local_models(&env.home);
    let mut local_index: Option<usize> = None;
    'wanted: for id in &wanted {
        for (index, candidate) in installed.iter().enumerate() {
            if &candidate.id != id {
                continue;
            }
            let has_weights = !candidate.path.is_empty() || candidate.framework == "CoreML";
            let local_has_no_weights = local_index
                .map(|i| installed[i].path.is_empty())
                .unwrap_or(false);
            if local_index.is_none() || (has_weights && local_has_no_weights) {
                local_index = Some(index);
            }
        }
        if let Some(index) = local_index {
            if !installed[index].path.is_empty() {
                break 'wanted;
            }
        }
    }

    // Both branches below always overwrite `base_url` before it is read in
    // the `wally_has_server` build this crate ships; a build without the
    // server support cfg'd out returns before reaching either write.
    #[allow(unused_assignments)]
    let mut base_url = String::new();
    let mut api_key = String::new();
    let mut console_url = String::new();
    let mut serving = false;
    let mut context_window: i64 = 0;

    if let Some(index) = local_index {
        let local = &installed[index];
        let local_entry = catalog::find(&local.id);
        if !local_entry
            .map(|entry| entry.harness_compatible)
            .unwrap_or(false)
        {
            out::error_line(
                "This model is not certified for coding harnesses. Tool calls or long-context \
                 operation may fail.",
            );
            let command = if harness_command.is_empty() {
                "opencode"
            } else {
                harness_command
            };
            out::status_line(&format!("Try: wally {command} -m qwen3-4b-instruct-2507"));
            return None;
        }
        // A directory with the manifest but no weights is a pull that did not
        // finish. Serving it fails inside llama.cpp with "No .gguf file
        // found", which reads as a bug; say what it is instead.
        if local.path.is_empty() && local.framework != "CoreML" {
            out::error_line(&format!(
                "{model} is on this machine but incomplete (a cancelled download?)"
            ));
            out::status_line(&format!("run `wally models pull {model}` to finish it"));
            return None;
        }
        // Any backend the kit registered. The server's rac_llm_create(path)
        // looks the path up in the registry and routes on the framework it
        // finds, so an MLX directory reaches MLX the same way a GGUF reaches
        // llama.cpp.
        let port = free_port();
        if port == 0 {
            out::error_line("could not find a free port for the local server");
            return None;
        }

        #[cfg(not(wally_has_server))]
        {
            // This kit was built without the OpenAI-compatible server, so
            // there is nothing here that can serve a file on disk. An
            // upstream model still works, and saying which is the case beats
            // starting nothing and reporting success.
            out::error_line(&format!(
                "{model} is on this machine, but this build has no local server to serve it"
            ));
            out::status_line("point at an upstream model instead, or use a build with the server");
            return None;
        }

        #[cfg(wally_has_server)]
        {
            // A single-file model (GGUF) is its file; a directory model (MLX
            // safetensors shards, Core ML) is its directory, which is also
            // what `wally serve` hands the server.
            let path = if local.framework == "LlamaCpp" && !local.path.is_empty() {
                local.path.as_str()
            } else {
                local.dir.as_str()
            };
            let path_c = match std::ffi::CString::new(path) {
                Ok(value) => value,
                Err(_) => {
                    out::error_line("resolved model path contains an embedded NUL byte");
                    return None;
                }
            };
            let model_id_c = match std::ffi::CString::new(model) {
                Ok(value) => value,
                Err(_) => {
                    out::error_line("model id contains an embedded NUL byte");
                    return None;
                }
            };
            // Sized from this machine, not a constant: a coding agent's
            // opening request is a 15k-token system prompt, and a fixed 8k
            // window rejected it. See local_context_size.
            let requested_context = local_context_size(&local.id);
            let config = sys::rac_server_config_t {
                host: c"127.0.0.1".as_ptr(),
                port,
                model_path: path_c.as_ptr(),
                model_id: model_id_c.as_ptr(),
                context_size: requested_context as i32,
                threads: 0, // Let the backend choose for this machine.
                gpu_layers: i32::MIN,
                enable_cors: sys::RAC_FALSE as sys::rac_bool_t,
                cors_origins: c"*".as_ptr(),
                request_timeout_seconds: 300,
                max_concurrent_requests: 4,
                verbose: if options.verbose {
                    sys::RAC_TRUE as sys::rac_bool_t
                } else {
                    sys::RAC_FALSE as sys::rac_bool_t
                },
            };
            out::status_line(&format!(
                "loading {model} on 127.0.0.1:{port} (requesting {} token context)",
                config.context_size
            ));
            // SAFETY: config is fully populated and every pointer field
            // (path_c, model_id_c, and the 'static host/cors_origins C string
            // literals) outlives this call.
            let started = unsafe { sys::rac_server_start(&config) };
            if started != sys::SUCCESS {
                out::error_line(&format!(
                    "the local server would not start for {model}: {}",
                    out::describe_result(started)
                ));
                out::status_line(&format!(
                    "check `wally models show {model}` and finish its download with `wally \
                     models pull {model}` (using the same --home)"
                ));
                return None;
            }
            let mut loaded_context: i32 = 0;
            // SAFETY: loaded_context is a valid out-param for the duration of
            // this call, made right after a successful rac_server_start.
            let context_result = unsafe { sys::rac_server_get_context_length(&mut loaded_context) };
            if context_result == sys::SUCCESS {
                let loaded_context = loaded_context as i64;
                if loaded_context < MINIMUM_CODING_HARNESS_CONTEXT {
                    // SAFETY: no arguments; stops the server just started
                    // above.
                    unsafe {
                        sys::rac_server_stop();
                    }
                    out::error_line(&format!(
                        "This model can use a {loaded_context}-token context on this machine, \
                         but coding harnesses require at least \
                         {MINIMUM_CODING_HARNESS_CONTEXT} tokens."
                    ));
                    out::status_line(&format!(
                        "Use a machine with more available memory, or run it directly with \
                         `wally run {model}`."
                    ));
                    return None;
                }
                if loaded_context != config.context_size as i64 {
                    out::status_line(&format!(
                        "this machine allocated {loaded_context} tokens from the requested {}",
                        config.context_size
                    ));
                }
                context_window = loaded_context;
            } else {
                out::status_line(
                    "Wally could not determine the loaded context; using the requested limit",
                );
                context_window = config.context_size as i64;
            }
            serving = true;
            base_url = format!("http://127.0.0.1:{port}/v1");
        }
    } else {
        let requested = catalog::find(model);
        if requested
            .map(|entry| entry.harness_compatible)
            .unwrap_or(false)
        {
            if !term::stdin_is_tty() {
                out::error_line(&format!("{model} is not downloaded on this machine"));
                out::status_line(&format!(
                    "run `wally models pull {model}` first (using the same --home)"
                ));
                return None;
            }
            if !confirm_model_pull(model) {
                out::status_line(&format!(
                    "download cancelled; run `wally models pull {model}` when ready"
                ));
                return None;
            }
            let requested_id = match requested {
                Some(entry) => entry.id,
                None => model,
            };
            if commands::pull_model_flow(options, requested_id) != 0 {
                return None;
            }
            return resolve(model, options, harness_command);
        }

        let mut credentials = match account::load() {
            Ok(c) => c,
            Err(message) => {
                out::error_line(&message);
                return None;
            }
        };
        if !credentials.signed_in() {
            // A known local model that has not been pulled should not send a
            // keyless user into the cloud login flow. Signed-in users still
            // get the normal catalog refresh for a hosted id of this
            // spelling.
            if catalog::find(model).is_some() || wanted.len() > 1 {
                out::error_line(&format!("{model} is not downloaded on this machine"));
                out::status_line(&format!(
                    "run `wally models pull {model}` first (using the same --home)"
                ));
            } else {
                report_not_signed_in();
            }
            return None;
        }
        // Keep the catalog fresh for next time without blocking this launch, and
        // catch a mistyped hosted id from the cache. Local models already took
        // the branch above; fail open when the cache is empty (offline / never
        // refreshed) so a launch is never blocked for lack of a network call.
        account::refresh_model_cache_if_stale(account::MODEL_CACHE_TTL_SECONDS);
        if account::cache_has_models() && !account::model_is_cached(model) {
            // Not in the cache: it may just be stale. Refresh live and retry, so
            // a valid new model launches instead of being wrongly rejected.
            if !refresh_and_recheck_model(&credentials, model) {
                return None;
            }
        }
        // signed_in() only proves a token is present, not that it is real: a
        // hand-written credentials.json satisfies it with any non-empty string.
        // Everything past this point is destructive to a caller's running app or
        // session, so confirm the session against the console first — the same
        // identity check `wally account whoami` makes, with the same
        // refresh-on-401 dance `wally account usage` uses.
        let console = ConsoleClient::default();
        let mut email = String::new();
        match verify_cloud_session(&console, &mut credentials) {
            Ok(verified_email) => email = verified_email,
            Err(err) => {
                if !err.unverified {
                    report_cloud_session_invalid(model);
                    return None;
                }
                // The console could not be ASKED - it is rate limiting or down.
                // That is no disproof of the session already on disk, and
                // refusing here locked every signed-in person out of their own
                // harness while a load test ran against the same console
                // (InferenceInfra#444). Go in on the stored session; the
                // harness's own calls surface the real error if it is still
                // there.
                out::status_line(&format!(
                    "could not confirm the cloud session ({}) - continuing on the stored session",
                    err.message
                ));
            }
        }
        base_url = format!("{}/v1", credentials.console_url);
        api_key = credentials.access_token.clone();
        console_url = credentials.console_url.clone();
        out::status_line(&format!(
            "using {model}{}",
            if email.is_empty() {
                String::new()
            } else {
                format!(" as {email}")
            }
        ));
    }

    Some(Endpoint {
        base_url,
        api_key,
        console_url,
        serving,
        context_window,
        max_output: local_output_size(context_window),
    })
}

/// Stops whatever `resolve` started. Safe on an endpoint it did not serve.
pub fn release(endpoint: &Endpoint) {
    if endpoint.serving {
        #[cfg(wally_has_server)]
        {
            // SAFETY: rac_server_stop() takes no arguments; it is safe to call
            // whenever a server this process started (endpoint.serving) is
            // being torn down.
            unsafe {
                crate::sys::rac_server_stop();
            }
        }
    }
}

#[cfg(windows)]
const PATH_SEPARATOR: char = ';';
#[cfg(not(windows))]
const PATH_SEPARATOR: char = ':';

/// The file names a launchable `tool` can take in a directory. Windows
/// carries the extension in the name (an npm shim is `tool.cmd`, a native
/// build `tool.exe`); POSIX has just the bare name.
fn executable_names(tool: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            format!("{tool}.exe"),
            format!("{tool}.cmd"),
            format!("{tool}.bat"),
            tool.to_string(),
        ]
    } else {
        vec![tool.to_string()]
    }
}

fn dir_has_tool(dir: &Path, tool: &str) -> bool {
    for name in executable_names(tool) {
        let candidate = dir.join(&name);
        if candidate.is_file() {
            return true;
        }
        if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
            if meta.file_type().is_symlink() {
                return true;
            }
        }
    }
    false
}

fn on_path(tool: &str) -> bool {
    let path = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    for raw in path.split(PATH_SEPARATOR) {
        // POSIX reads an empty PATH component as the working directory, and
        // execvp honours that. Skipping it here made this preflight stricter
        // than the exec it is meant to predict, so `./tool` on an empty
        // component was reported missing and never run.
        let dir = if raw.is_empty() && !cfg!(windows) {
            "."
        } else {
            raw
        };
        if !dir.is_empty() && dir_has_tool(Path::new(dir), tool) {
            return true;
        }
    }
    false
}

/// Per-user install locations a fresh harness install lands in before the
/// shell has picked it up on PATH: npm's global bin, the native installers'
/// own bin, and on Windows the AppData npm shims. Probed so a just-installed
/// tool is not wrongly reported missing before its bin directory reaches
/// PATH.
#[cfg(windows)]
fn common_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.is_empty() {
            dirs.push(Path::new(&appdata).join("npm"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        if !local.is_empty() {
            dirs.push(Path::new(&local).join("npm"));
            dirs.push(Path::new(&local).join("hermes").join("bin"));
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            dirs.push(Path::new(&profile).join(".local").join("bin"));
        }
    }
    dirs
}

#[cfg(not(windows))]
fn common_install_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            dirs.push(Path::new(&home).join(".local").join("bin"));
            dirs.push(Path::new(&home).join(".npm-global").join("bin"));
        }
    }
    dirs
}

/// The common install dir that holds `tool`, or None when none does. This
/// only answers the "installed but not yet on PATH" case; a tool already on
/// PATH is `on_path`'s job.
fn locate_off_path(tool: &str) -> Option<PathBuf> {
    common_install_dirs()
        .into_iter()
        .find(|dir| dir_has_tool(dir, tool))
}

/// Puts `dir` first on this process's PATH so the launch below can exec a
/// tool found off PATH. The child inherits the change; nothing outside wally
/// sees it.
fn prepend_to_path(dir: &Path) {
    let existing = std::env::var("PATH").unwrap_or_default();
    let mut updated = dir.to_string_lossy().into_owned();
    if !existing.is_empty() {
        updated.push(PATH_SEPARATOR);
        updated.push_str(&existing);
    }
    // SAFETY: mirrors the C++ setenv/_putenv_s call this ports; wally is
    // single-threaded at the point a tool launch resolves its PATH.
    unsafe {
        std::env::set_var("PATH", updated);
    }
}

/// How you get a harness we do not ship. Kept beside the spawn so a missing
/// tool answers the only question the person actually has. Verified against
/// each tool's own docs: opencode-ai and @deepseek-ai/dsh are npm packages;
/// Claude Code and Hermes ship a native install script (npm for Claude Code
/// is deprecated); OpenClaw's npm package is openclaw@latest.
fn install_hint(tool: &str) -> String {
    match tool {
        "opencode" => "install it with `npm i -g opencode-ai`, then run this again".to_string(),
        "openclaw" => "install it with `npm i -g openclaw@latest`, then run this again".to_string(),
        "dsh" => "install it with `npm i -g @deepseek-ai/dsh`, then run this again".to_string(),
        "claude" => {
            if cfg!(windows) {
                "install it with `irm https://claude.ai/install.ps1 | iex` in PowerShell, then run this again".to_string()
            } else {
                "install it with `curl -fsSL https://claude.ai/install.sh | bash`, then run this again".to_string()
            }
        }
        "hermes" => {
            if cfg!(windows) {
                "install it with `iex (irm https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.ps1)` in PowerShell, then run this again".to_string()
            } else {
                "install it with `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash -s -- --skip-setup`, then run this again".to_string()
            }
        }
        other => format!("install {other} and put it on PATH, then run this again"),
    }
}

/// True when the CLI `tool` is on PATH. When it is not, prints the clean
/// "not installed" message and its accurate install command, then returns
/// false — so a caller can stop before resolving a model or printing
/// anything else, which is the only thing a person without the tool needs to
/// see.
pub fn ensure_installed(tool: &str) -> bool {
    if on_path(tool) {
        return true;
    }
    // Not on PATH, but a fresh install often sits in a well-known bin the
    // shell has not picked up yet (a just-run `npm i -g`, or a Windows
    // AppData shim). If it does, put that directory on PATH for this run so
    // the launch can exec it, rather than telling the person to install what
    // is already there.
    if let Some(dir) = locate_off_path(tool) {
        prepend_to_path(&dir);
        return true;
    }
    out::error_line(&format!("{tool} is not installed on this machine"));
    out::status_line(&install_hint(tool));
    false
}

/// Quotes one argument so a Windows child re-parses it as a single token; the
/// _spawn* family joins argv into a command line without quoting. Rules per
/// the documented MSVCRT parser: double the run of backslashes that precedes
/// a quote (or the closing quote), and backslash-escape embedded quotes. The
/// POSIX path needs none of this — execvp hands argv to the child verbatim.
#[cfg(windows)]
pub fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return arg.to_string();
    }
    let chars: Vec<char> = arg.chars().collect();
    let mut quoted = String::from("\"");
    let mut i = 0usize;
    loop {
        let mut backslashes = 0usize;
        while i < chars.len() && chars[i] == '\\' {
            i += 1;
            backslashes += 1;
        }
        if i == chars.len() {
            quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
            break;
        }
        if chars[i] == '"' {
            quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
            quoted.push('"');
        } else {
            quoted.extend(std::iter::repeat_n('\\', backslashes));
            quoted.push(chars[i]);
        }
        i += 1;
    }
    quoted.push('"');
    quoted
}

/// Builds the command line that runs a Windows batch (`.cmd`/`.bat`) `script`
/// through cmd.exe, forwarding `args`. Windows cannot launch a batch file
/// through CreateProcess directly, and cmd.exe re-enables the command
/// injection that quoting for a normal child closes, so a token holding a
/// character that cannot be made safe for the command processor — a double
/// quote, a percent, or a CR/LF — is refused rather than run
/// (CVE-2024-24576). On success the return holds `cmd.exe /d /s /c "…"`. Kept
/// free of Windows-only APIs so the escaping can be tested on any platform —
/// unlike `quote_windows_arg`, this is not behind `cfg(windows)`.
pub fn build_batch_command_line(script: &str, args: &[String]) -> Result<String, String> {
    fn offending(token: &str) -> Option<&'static str> {
        for c in token.chars() {
            match c {
                '"' => return Some("a double quote"),
                '%' => return Some("a percent sign"),
                '\r' => return Some("a carriage return"),
                '\n' => return Some("a newline"),
                _ => {}
            }
        }
        None
    }
    // Wrap a token (already known to hold no double quote) so both cmd.exe
    // and the target's C runtime read it as one literal: the quotes make cmd
    // treat & | < > ( ) ^ as text, and doubling a trailing backslash run
    // stops the closing quote being escaped when the child re-parses the
    // line.
    fn quote(token: &str) -> String {
        let chars: Vec<char> = token.chars().collect();
        let mut trailing = 0usize;
        while trailing < chars.len() && chars[chars.len() - 1 - trailing] == '\\' {
            trailing += 1;
        }
        format!("\"{token}{}\"", "\\".repeat(trailing))
    }

    if let Some(bad) = offending(script) {
        return Err(format!("cannot launch this tool: its path holds {bad}"));
    }
    let mut inner = quote(script);
    for arg in args {
        if let Some(bad) = offending(arg) {
            return Err(format!("cannot pass {bad} to a Windows .cmd/.bat tool"));
        }
        inner.push(' ');
        inner.push_str(&quote(arg));
    }
    // /d skips any AutoRun, /s makes cmd strip exactly the one outer quote
    // pair added here and run the remainder verbatim, /c runs and exits.
    Ok(format!("cmd.exe /d /s /c \"{inner}\""))
}

/// Prints the one shared "cloud session is no longer valid" error, in red,
/// that every harness shows when a hosted `model` cannot be used because the
/// session failed verification. One phrasing, one place, so it reads the
/// same whichever caller hit it.
pub fn report_cloud_session_invalid(model: &str) {
    // Stderr, on its own line: a red "Error:" a person cannot miss, and the
    // action `wally account login` highlighted so the fix stands out. Color
    // is dropped under NO_COLOR or when stderr is not a terminal.
    let pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(&format!(
        "{}Error:{} You cannot use {model}, your cloud session is no longer valid, do: {}wally account login{} and try again",
        pal.red, pal.reset, pal.bold_cyan, pal.reset
    ));
}

pub fn report_not_signed_in() {
    let pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(&format!(
        "{}Error:{} You are not logged in, log in with {}wally account login{}",
        pal.red, pal.reset, pal.bold_cyan, pal.reset
    ));
}

pub fn refresh_and_recheck_model(credentials: &Credentials, model: &str) -> bool {
    let pal = cli_color::make_palette(color_output_enabled(false));
    out::status_line(&format!("could not find '{model}' in the catalog"));
    out::status_line(&format!(
        "{}fetching the latest catalog...{}",
        pal.blue, pal.reset
    ));
    if account::refresh_model_cache_now(credentials).is_err() {
        out::status_line(&format!(
            "{}Error:{} sorry, the server is busy, try again after some time",
            pal.red, pal.reset
        ));
        return false;
    }
    out::status_line(&format!(
        "{}catalog updated, checking your model{}",
        pal.green, pal.reset
    ));
    if !account::model_is_cached(model) {
        out::error_line(&format!("unknown model '{model}'"));
        // The cache is fresh (just refreshed), so this list is accurate.
        let available = account::cached_model_ids();
        if !available.is_empty() {
            out::status_line("available models:");
            for id in &available {
                out::status_line(&format!("  {id}"));
            }
        }
        return false;
    }
    out::status_line(&format!(
        "{}found the model, launching the harness{}",
        pal.blue, pal.reset
    ));
    true
}

const CONFIG_VARIABLE: &str = "OPENCODE_CONFIG_CONTENT";

fn set_config_variable(value: &str) {
    if value.contains('\0') {
        // A NUL cannot appear in a real access token or model id; refuse to
        // pass it to std::env::set_var, which panics on an embedded NUL,
        // rather than the C++ setenv this ports, which would just stop at
        // the first NUL silently.
        return;
    }
    // SAFETY: mirrors the C++ setenv/_putenv_s call this ports; `launch` is
    // the only writer active during the spawn it wraps.
    unsafe { std::env::set_var(CONFIG_VARIABLE, value) };
}

fn unset_config_variable() {
    // SAFETY: as above.
    unsafe { std::env::remove_var(CONFIG_VARIABLE) };
}

#[cfg(windows)]
fn resolve_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(PATH_SEPARATOR) {
        if dir.is_empty() {
            continue;
        }
        for name in executable_names(tool) {
            let candidate = Path::new(dir).join(&name);
            if candidate.is_file() {
                return Some(candidate);
            }
            if let Ok(meta) = std::fs::symlink_metadata(&candidate) {
                if meta.file_type().is_symlink() {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Runs a fully-formed command line through CreateProcessW and waits for it,
/// returning the child's exit code. Used for the batch path, where a plain
/// argv spawn's own joining would fight the quoting the command processor
/// needs.
#[cfg(windows)]
fn spawn_command_line(tool: &str, command_line: &str) -> i32 {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, GetExitCodeProcess, WaitForSingleObject, INFINITE, PROCESS_INFORMATION,
        STARTUPINFOW,
    };

    // Rust strings are always valid UTF-8, so `encode_utf16` produces exactly
    // what MultiByteToWideChar(CP_UTF8, ...) would for this input.
    let mut wide: Vec<u16> = command_line.encode_utf16().collect();
    wide.push(0);

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();

    // SAFETY: `wide` is a valid, NUL-terminated UTF-16 buffer that outlives
    // the call; `startup`/`process` are zero-initialized OS structs of the
    // size the API expects, matching the C++ SpawnCommandLine this ports.
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            wide.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            0,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if ok == 0 {
        out::error_line(&format!("{tool} could not be launched"));
        return 127;
    }
    // SAFETY: `process.hProcess`/`hThread` are the handles CreateProcessW
    // just returned above; each is closed exactly once, after the wait and
    // exit-code read.
    unsafe {
        WaitForSingleObject(process.hProcess, INFINITE);
        let mut code: u32 = 0;
        GetExitCodeProcess(process.hProcess, &mut code);
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
        code as i32
    }
}

#[cfg(windows)]
fn spawn(tool: &str, args: &[String]) -> i32 {
    use std::os::windows::process::CommandExt;

    // Checked before the spawn, not after: a failed launch on Windows still
    // reports something useful, but this keeps the message identical to the
    // POSIX path's preflight.
    if !ensure_installed(tool) {
        return 127;
    }

    // A .cmd/.bat target — an npm-installed CLI is a `tool.cmd` shim —
    // cannot be launched through CreateProcess directly. Route those through
    // the command processor instead; a native `.exe` keeps the direct spawn.
    if let Some(resolved) = resolve_on_path(tool) {
        let extension = resolved
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if extension == "cmd" || extension == "bat" {
            return match build_batch_command_line(&resolved.to_string_lossy(), args) {
                Ok(command_line) => spawn_command_line(tool, &command_line),
                Err(error) => {
                    out::error_line(&format!("{tool}: {error}"));
                    1
                }
            };
        }
    }

    // A plain argv spawn joins arguments with bare spaces, so each piece is
    // quoted the same way the MSVCRT parser expects, or a prompt such as
    // "fix the tests" would reach the tool as three separate arguments.
    // `raw_arg` passes the already-quoted text through untouched, bypassing
    // Command's own (different) quoting, which would otherwise quote it a
    // second time.
    let mut command = std::process::Command::new(tool);
    for arg in args {
        command.raw_arg(quote_windows_arg(arg));
    }
    match command.status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(_) => {
            out::error_line(&format!("{tool} is not on PATH"));
            127
        }
    }
}

#[cfg(not(windows))]
fn spawn(tool: &str, args: &[String]) -> i32 {
    use std::ffi::CString;

    // Checked before the fork, not after: a failed exec happens in the
    // child, where the only thing it can report back is the exit code a
    // shell uses for "command not found" — so without this the person sees
    // nothing at all.
    if !ensure_installed(tool) {
        return 127;
    }

    let mut owned: Vec<CString> = Vec::with_capacity(args.len() + 1);
    owned.push(CString::new(tool).unwrap_or_default());
    for arg in args {
        owned.push(CString::new(arg.as_str()).unwrap_or_default());
    }
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // fork rather than exec: the local server lives in this process, and
    // replacing the image would take it down with us before the tool ran.
    //
    // SAFETY: fork() duplicates the process; the child branch below only
    // calls async-signal-safe functions (execvp, _exit) before either
    // replacing its image or exiting, and never returns into the rest of
    // this function.
    let child = unsafe { libc::fork() };
    if child < 0 {
        out::error_line(&format!("could not start {tool}"));
        return 1;
    }
    if child == 0 {
        // SAFETY: `argv` is a NUL-terminated array of valid C strings kept
        // alive by `owned`, which this child branch never returns past —
        // execvp either replaces this image or does not return, and `_exit`
        // terminates unconditionally.
        unsafe {
            libc::execvp(owned[0].as_ptr(), argv.as_ptr());
            // Only reached when exec failed. 127 is what a shell reports for
            // a command it cannot find, and the parent cannot tell why
            // otherwise.
            libc::_exit(127);
        }
    }
    let mut status: i32 = 0;
    loop {
        // SAFETY: `child` is the pid fork() just returned to this (parent)
        // branch; `status` is a valid out-param for the duration of the
        // call.
        let rc = unsafe { libc::waitpid(child, &mut status, 0) };
        if rc >= 0 {
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        out::error_line(&format!("lost track of {tool}"));
        return 1;
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        1
    }
}

/// Launch `tool` against `model`, forwarding `args`. Returns the exit code.
pub fn launch(tool: &str, model: &str, args: &[String], options: &GlobalOptions) -> i32 {
    if model.is_empty() {
        // Nothing to wire, so do not pretend to: run the tool as the user
        // has it configured.
        return spawn(tool, args);
    }

    let Some(endpoint) = resolve(model, options, tool) else {
        return 1;
    };

    let config = build_open_code_config(
        model,
        &endpoint.base_url,
        &endpoint.api_key,
        &catalog_models_for(&endpoint, model),
    );
    let previous = std::env::var(CONFIG_VARIABLE).ok();
    set_config_variable(&config);

    let status = spawn(tool, args);

    // Launch runs more than once in a process during tests, and a stale
    // value here would override the tool's own configuration on a later
    // call that named no model.
    match &previous {
        Some(value) => set_config_variable(value),
        None => unset_config_variable(),
    }
    release(&endpoint);
    status
}
