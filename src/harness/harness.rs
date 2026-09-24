//! Endpoint resolution, cloud-session checks, tool install checks and the
//! launch (port of src/harness/harness.cpp).

use std::path::{Path, PathBuf};

use crate::account::{self, ConsoleClient, Credentials};
use crate::bootstrap::{self, GlobalOptions};
use crate::catalog;
use crate::cli_formatter::{cli_color, color_output_enabled};
use crate::io::output as out;

use super::catalog_models::catalog_models_for;
use super::local_models::local_models;

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
        if matches!(
            byte,
            b'<' | b'>' | b'"' | b'\'' | b'&' | b'/' | b'\\'
        ) {
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
fn refresh_session(console: &ConsoleClient, credentials: &mut Credentials) -> Result<(), SessionError> {
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
            credentials.expires_at =
                epoch_seconds() + if grant.expires_in > 0 { grant.expires_in } else { 3600 };
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
pub fn resolve(model: &str) -> Option<Endpoint> {
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
    let env = bootstrap::bootstrap(&GlobalOptions::default()).ok()?;

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

    let serving = false;

    if let Some(index) = local_index {
        let local = &installed[index];
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
        // Coding tools are cloud-only this release. A local model is refused
        // outright rather than gated: the kit's local server re-reads the
        // whole conversation every turn and leaks a reasoning model's
        // thinking into the reply, so an agent degrades from the second turn
        // on. `wally run` still takes any local model; the harnesses take a
        // hosted one. (The C++ has local-server-starting code after this
        // point that is unreachable — guarded behind a `return false` that
        // always fires first — and is not ported here for the same reason.)
        out::error_line(&format!(
            "{model} is on this machine, but coding tools run on hosted models only"
        ));
        out::status_line(
            "sign in and use one: `wally account login`, then `wally opencode --cloud -m glm-5.3-flash`",
        );
        return None;
    }

    let mut credentials = match account::load() {
        Ok(c) => c,
        Err(message) => {
            out::error_line(&message);
            return None;
        }
    };
    if !credentials.signed_in() {
        report_not_signed_in();
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
    let base_url = format!("{}/v1", credentials.console_url);
    let api_key = credentials.access_token.clone();
    let console_url = credentials.console_url.clone();
    out::status_line(&format!(
        "using {model}{}",
        if email.is_empty() {
            String::new()
        } else {
            format!(" as {email}")
        }
    ));

    Some(Endpoint {
        base_url,
        api_key,
        console_url,
        serving,
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
        "openclaw" => {
            "install it with `npm i -g openclaw@latest`, then run this again".to_string()
        }
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
            quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
            break;
        }
        if chars[i] == '"' {
            quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
            quoted.push('"');
        } else {
            quoted.extend(std::iter::repeat('\\').take(backslashes));
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

/// JSON string escaping, for the handful of characters that can appear in a
/// model id, a path or a key. Not a general encoder: it exists so a Windows
/// path with backslashes does not silently produce invalid config.
fn quote_json(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The provider block opencode reads out of OPENCODE_CONFIG_CONTENT.
///
/// Inline rather than a file on purpose: writing to the user's project or to
/// ~/.config/opencode would outlive the session and change how opencode
/// behaves when they run it themselves. Hand-built (not `io::json::dump`) so
/// the key order below matches the C++ byte for byte.
fn opencode_config(
    primary: &str,
    base_url: &str,
    api_key: &str,
    models: &[super::catalog_models::CatalogModel],
) -> String {
    // A key is always present because opencode's OpenAI client sends an
    // Authorization header regardless; a local server ignores what is in it.
    let key = if api_key.is_empty() { "local" } else { api_key };
    // Every catalog model is a selectable entry so opencode's picker lists
    // them all; `primary` stays the default selection.
    let mut entries = String::new();
    for entry in models {
        if !entries.is_empty() {
            entries.push(',');
        }
        entries.push_str(&quote_json(&entry.id));
        entries.push_str(":{\"name\":");
        entries.push_str(&quote_json(&entry.id));
        entries.push('}');
    }
    let mut out = String::from("{\"provider\":{\"runanywhere\":{");
    out.push_str("\"npm\":\"@ai-sdk/openai-compatible\",");
    out.push_str("\"name\":\"RunAnywhere\",");
    out.push_str("\"options\":{\"baseURL\":");
    out.push_str(&quote_json(base_url));
    out.push_str(",\"apiKey\":");
    out.push_str(&quote_json(key));
    out.push_str("},");
    out.push_str("\"models\":{");
    out.push_str(&entries);
    out.push_str("}}},");
    out.push_str("\"model\":");
    out.push_str(&quote_json(&format!("runanywhere/{primary}")));
    out.push('}');
    out
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

    let mut startup = STARTUPINFOW {
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
pub fn launch(tool: &str, model: &str, args: &[String]) -> i32 {
    if model.is_empty() {
        // Nothing to wire, so do not pretend to: run the tool as the user
        // has it configured.
        return spawn(tool, args);
    }

    let Some(endpoint) = resolve(model) else {
        return 1;
    };

    let config = opencode_config(
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
