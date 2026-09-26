//! The coding-agent table and the per-agent config builders (port of
//! src/harness/agents.cpp).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::account::{self, ConsoleClient};
use crate::bootstrap::GlobalOptions;
use crate::io::json::dump;
use crate::io::output as out;

use super::catalog_models::{catalog_models_for, CatalogModel};
use super::declared_harness::{harness_header_value, DeclaredHarness, HARNESS_HEADER};
use super::harness::{launch, release, resolve, Endpoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    /// The one handoff that declares no harness (`X-RA-Harness`): Hermes takes
    /// extra request headers only from `config.yaml`, never the environment,
    /// and wally does not write that file.
    CustomEndpointEnvironment,
    ConfigFile,
    PatchOverlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub id: &'static str,
    pub command: &'static str,
    pub summary: &'static str,
    pub handoff: Handoff,
    pub default_args: &'static str,
}

/// C++ `kAgents` / `kAgentCount`.
pub fn agents() -> &'static [Agent] {
    const AGENTS: &[Agent] = &[
        Agent {
            id: "hermes",
            command: "hermes",
            summary: "Open Hermes with a model",
            handoff: Handoff::CustomEndpointEnvironment,
            default_args: "--tui",
        },
        Agent {
            id: "openclaw",
            command: "openclaw",
            summary: "Open OpenClaw with a model",
            handoff: Handoff::ConfigFile,
            default_args: "tui --local",
        },
        // No default arguments: this row picks its own profile below, and a
        // `web` default would arrive here as the person's first positional —
        // which is to say, as a prompt.
        Agent {
            id: "deepseek",
            command: "dsh",
            summary: "Open DeepSeek Harness with a model",
            handoff: Handoff::PatchOverlay,
            default_args: "",
        },
    ];
    AGENTS
}

/// An OpenAI client sends an Authorization header whatever is in it, and the
/// local server ignores the value. A placeholder keeps both ends happy.
fn key_or_placeholder(api_key: &str) -> &str {
    if api_key.is_empty() {
        "local"
    } else {
        api_key
    }
}

/// Sets `name`, guarding `name` against byte sequences that make
/// `std::env::set_var` panic (`name` is always one of our own fixed
/// identifiers, never server-controlled, so this never actually rejects
/// anything in practice). `value` is handled the way the C runtime's `setenv`
/// does: `setenv(name, value.c_str(), 1)` truncates silently at the first
/// embedded NUL rather than failing, so an embedded NUL here is truncated the
/// same way instead of refusing to set the variable at all. Returns whether
/// it actually applied.
fn set_environment(name: &str, value: &str) -> bool {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return false;
    }
    let value = match value.find('\0') {
        Some(index) => &value[..index],
        None => value,
    };
    // SAFETY: name was just checked for the byte sequences that make
    // set_var panic, and `value` was truncated at its first NUL (if any);
    // ScopedEnv holds this for one launch at a time.
    unsafe { std::env::set_var(name, value) };
    true
}

fn unset_environment(name: &str) {
    // SAFETY: removing an environment variable by a fixed, checked name.
    unsafe { std::env::remove_var(name) };
}

/// Sets a variable for the child and puts back what was there on the way out.
///
/// The process outlives one launch — tests run several, and the REPL can too
/// — so a value left behind would silently configure the next tool that
/// named no model.
struct ScopedEnv {
    name: String,
    previous: Option<std::ffi::OsString>,
    had_previous: bool,
    applied: bool,
}

impl ScopedEnv {
    fn new(name: &str, value: &str) -> Self {
        let previous = std::env::var_os(name);
        let had_previous = previous.is_some();
        let applied = set_environment(name, value);
        ScopedEnv {
            name: name.to_string(),
            previous,
            had_previous,
            applied,
        }
    }

    fn applied(&self) -> bool {
        self.applied
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        if !self.applied {
            return;
        }
        if self.had_previous {
            if let Some(previous) = self.previous.take() {
                // SAFETY: `previous` came from this same environment variable
                // before this guard changed it, so it is already a value the
                // OS accepted.
                unsafe { std::env::set_var(&self.name, previous) };
            }
        } else {
            unset_environment(&self.name);
        }
    }
}

/// A config file that exists only while the tool is running.
///
/// Written into the temp directory rather than the person's own config tree:
/// `~/.openclaw/config.json` is theirs, and a run of wally is not a reason to
/// rewrite it.
struct TemporaryConfig {
    path: Option<PathBuf>,
}

/// `std::filesystem::temp_directory_path()`: TMPDIR, TMP, TEMP, TEMPDIR (the
/// first that is set), else /tmp — and nothing unless that names an existing
/// directory. `std::env::temp_dir()` is not the same: it reads only TMPDIR and,
/// on macOS, falls back to the per-user /var/folders/…/T instead of /tmp.
fn temp_directory_path() -> Option<PathBuf> {
    #[cfg(not(windows))]
    let directory = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"]
        .iter()
        .find_map(std::env::var_os)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // MSVC's temp_directory_path is GetTempPathW, which temp_dir() also wraps.
    #[cfg(windows)]
    let directory = std::env::temp_dir();
    std::fs::metadata(&directory)
        .is_ok_and(|metadata| metadata.is_dir())
        .then_some(directory)
}

// Neither C++ nor this port ever cleaned these up: a launch that is killed
// (a crash, `kill -9`, a closed terminal) skips TemporaryConfig's Drop, so
// its file -- which for OpenClaw's handoff still holds the session's API
// key -- sits in the temp directory indefinitely. Anything younger than this
// may belong to a launch still running right now.
const LEFTOVER_CONFIG_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// `true` for exactly the names `TemporaryConfig::write` creates:
/// `wally-agent-<digits>.json` or `wally-agent-<digits>.yaml`. Nothing else
/// in the temp directory matches, including a name we wrote ourselves for a
/// different purpose.
fn is_leftover_config_name(name: &str) -> bool {
    for extension in [".json", ".yaml"] {
        if let Some(digits) = name
            .strip_prefix("wally-agent-")
            .and_then(|rest| rest.strip_suffix(extension))
        {
            return !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
        }
    }
    false
}

/// Whether `clean_leftover_configs` may delete the entry `metadata` describes:
/// a regular file (never a symlink -- never follow one into deleting
/// something else), owned by the account running this process, and older
/// than `LEFTOVER_CONFIG_MAX_AGE`. Ownership has no check on Windows: the
/// per-user temp directory `temp_directory_path` resolves there is already
/// ACL-restricted to its owner, the same reasoning `TemporaryConfig::write`
/// already relies on to skip setting Unix-style file permissions there.
fn is_removable_leftover_config(metadata: &std::fs::Metadata, now: std::time::SystemTime) -> bool {
    if metadata.is_symlink() || !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid() takes no arguments and cannot fail.
        let euid = unsafe { libc::geteuid() };
        if metadata.uid() != euid {
            return false;
        }
    }
    match metadata.modified() {
        Ok(modified) => now
            .duration_since(modified)
            .is_ok_and(|age| age >= LEFTOVER_CONFIG_MAX_AGE),
        // No mtime to judge by: never treat it as safe to remove.
        Err(_) => false,
    }
}

/// Removes our own abandoned `wally-agent-*` configs from the temp directory
/// before this launch writes a new one. Best-effort throughout: a directory
/// that cannot be listed, or a single entry whose metadata cannot be read or
/// which cannot be removed (already gone, permissions, a live process still
/// holding it open on Windows), is silently skipped -- this is background
/// housekeeping, never the operation the caller actually asked for.
fn clean_leftover_configs() {
    let Some(directory) = temp_directory_path() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_leftover_config_name(name) {
            continue;
        }
        // `DirEntry::metadata()` does not traverse a symlink where the
        // platform supports telling the difference -- the same "look, don't
        // follow" guarantee `is_removable_leftover_config` depends on.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if is_removable_leftover_config(&metadata, now) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

impl TemporaryConfig {
    fn new() -> Self {
        TemporaryConfig { path: None }
    }

    fn write(&mut self, contents: &str, extension: &str) -> Result<(), String> {
        let Some(directory) = temp_directory_path() else {
            return Err("no temp directory to write the agent config into".to_string());
        };
        // Matches std::random_device entropy(); entropy() — any process-wide
        // source of unpredictability is enough, this only has to dodge a name
        // collision, not resist an attacker who can already write here.
        let random = getrandom::u32().unwrap_or(0);
        let candidate = directory.join(format!("wally-agent-{random}{extension}"));

        #[cfg(not(windows))]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            // The config carries the session's API key, so the file is
            // created 0600 up front rather than chmod'd after the write:
            // chmod leaves a window where the key sits in a 0644
            // (umask-default) file, and create_new (O_EXCL) refuses a name a
            // local attacker pre-created or symlinked (CWE-378).
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&candidate);
            let mut file = match file {
                Ok(f) => f,
                Err(_) => {
                    return Err(format!(
                        "could not create the agent config at {}",
                        candidate.display()
                    ))
                }
            };
            if file.write_all(contents.as_bytes()).is_err() {
                drop(file);
                let _ = std::fs::remove_file(&candidate);
                return Err(format!(
                    "could not write the agent config to {}",
                    candidate.display()
                ));
            }
        }
        #[cfg(windows)]
        {
            use std::io::Write;
            // The per-user temp directory is ACL-restricted to its owner, so
            // a plain write is already private on the platform that has no
            // mode bits to set.
            let mut file = match std::fs::File::create(&candidate) {
                Ok(f) => f,
                Err(_) => {
                    return Err(format!(
                        "could not write the agent config to {}",
                        candidate.display()
                    ))
                }
            };
            // C++'s Windows branch (`file << contents; file.close();`) never
            // checks the write's result, so a mid-write I/O failure (disk
            // full, ...) is silently reported as success there. Match that
            // instead of surfacing an error C++ never would; `self.path` is
            // still set below regardless, so the partial file is tracked
            // and removed by Drop like any other run.
            let _ = file.write_all(contents.as_bytes());
        }
        self.path = Some(candidate);
        Ok(())
    }

    fn path(&self) -> String {
        self.path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

impl Drop for TemporaryConfig {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

const PROVIDER_ID: &str = "runanywhere";

/// The environment variable the dsh provider's `apiKeyEnv` points at. Fixed
/// rather than host-derived: dsh resolves the reference itself and puts no
/// host rule on the name.
const DEEP_SEEK_KEY_VARIABLE: &str = "RUNANYWHERE_API_KEY";

/// `agent.default_args` split on spaces, or `args` when the person passed
/// any.
///
/// Theirs wins whole: a tool started with their own subcommand is theirs to
/// drive, and mixing our default into it would produce a command line
/// neither of us wrote.
fn effective_args(agent: &Agent, args: &[String]) -> Vec<String> {
    if !args.is_empty() || agent.default_args.is_empty() {
        return args.to_vec();
    }
    agent
        .default_args
        .split(' ')
        .filter(|piece| !piece.is_empty())
        .map(|piece| piece.to_string())
        .collect()
}

/// Where OpenClaw keeps its state, and the config inside it.
///
/// `OPENCLAW_STATE_DIR` wins, then `OPENCLAW_HOME`, then `~/.openclaw` — the
/// order `resolveConfigDir` uses. Naming the state directory explicitly
/// matters more than it looks: OpenClaw otherwise derives it from the config
/// file's own folder, so pointing `OPENCLAW_CONFIG_PATH` at a temp file
/// would move their agents and sessions into the temp directory for the run.
fn open_claw_state_directory() -> PathBuf {
    // `var_os`, not `var`: `std::getenv` in C++ returns the raw bytes
    // regardless of encoding, and `std::filesystem::path` is encoding-agnostic
    // on POSIX, so a legacy-encoded HOME/OPENCLAW_* must still resolve here
    // instead of silently looking unset.
    if let Some(state) = std::env::var_os("OPENCLAW_STATE_DIR") {
        if !state.is_empty() {
            return PathBuf::from(state);
        }
    }
    if let Some(home) = std::env::var_os("OPENCLAW_HOME") {
        if !home.is_empty() {
            return Path::new(&home).join(".openclaw");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        if !home.is_empty() {
            return Path::new(&home).join(".openclaw");
        }
    }
    // PowerShell and cmd.exe leave HOME unset; openclaw falls back to the
    // profile.
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        if !profile.is_empty() {
            return Path::new(&profile).join(".openclaw");
        }
    }
    PathBuf::new()
}

/// What the console says this model costs and how much it can hold.
///
/// A local server has no catalog entry and no price: it is served with the
/// context size `Resolve` starts it with, which is the honest number to
/// declare.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ModelLimits {
    context_window: i64,
    max_output: i64,
    input_per_mtok: i64,
    output_per_mtok: i64,
}

fn lookup_limits(endpoint: &Endpoint, model: &str) -> ModelLimits {
    let mut limits = ModelLimits::default();
    if endpoint.api_key.is_empty() {
        // The size `harness::resolve` started the local server with.
        let local = &catalog_models_for(endpoint, model)[0];
        limits.context_window = local.context_window;
        limits.max_output = local.max_output;
        return limits;
    }

    let credentials = match account::load() {
        Ok(c) => c,
        Err(_) => return limits,
    };
    if !credentials.signed_in() {
        return limits;
    }
    let console = ConsoleClient::default();
    let (result, models, error) =
        console.fetch_models(&credentials.console_url, &credentials.access_token);
    if result == account::IdentityResult::Ok {
        if let Some(info) = models.iter().find(|m| m.id == model) {
            limits.context_window = info.context_window;
            limits.max_output = info.max_output_tokens;
        }
    } else {
        out::status_line(&format!(
            "could not read the model list ({error}); launching without a context-window hint"
        ));
    }
    let (price_result, prices, _) =
        console.fetch_catalog(&credentials.console_url, &credentials.access_token);
    if price_result == account::IdentityResult::Ok {
        if let Some(price) = prices.iter().find(|p| p.id == model) {
            limits.input_per_mtok = price.input_per_mtok;
            limits.output_per_mtok = price.output_per_mtok;
        }
    }
    limits
}

/// Their current config document, or empty when they have none.
fn read_open_claw_config() -> String {
    let path = match std::env::var_os("OPENCLAW_CONFIG_PATH") {
        Some(over) if !over.is_empty() => PathBuf::from(over),
        _ => {
            let state = open_claw_state_directory();
            if state.as_os_str().is_empty() {
                return String::new();
            }
            state.join("openclaw.json")
        }
    };
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Mirrors hermes_cli/runtime_provider._host_derived_api_key: strip the
/// scheme, drop leading `api.`/`www.` labels, and name the registrable one.
pub fn hermes_key_variable(base_url: &str) -> String {
    let mut host = base_url.to_string();
    if let Some(pos) = host.find("://") {
        host = host[pos + 3..].to_string();
    }
    if let Some(pos) = host.find(['/', ':']) {
        host.truncate(pos);
    }
    if host.is_empty() || host == "localhost" {
        return String::new();
    }

    let mut labels: Vec<String> = host
        .split('.')
        .filter(|label| !label.is_empty())
        .map(|label| label.to_string())
        .collect();

    // An IP address ends in digits, and Hermes reads no key for one.
    match labels.last() {
        None => return String::new(),
        Some(last) => {
            if last
                .chars()
                .last()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            {
                return String::new();
            }
        }
    }
    while !labels.is_empty() && (labels[0] == "api" || labels[0] == "www") {
        labels.remove(0);
    }
    if labels.len() < 2 {
        return String::new();
    }

    let candidate = &labels[labels.len() - 2];
    let mut vendor = String::new();
    for c in candidate.chars() {
        if c.is_ascii_alphanumeric() {
            vendor.push(c.to_ascii_uppercase());
        } else {
            vendor.push('_');
        }
    }
    let starts_with_letter = vendor
        .chars()
        .next()
        .map(|c| c.is_ascii_alphabetic())
        .unwrap_or(false);
    if !starts_with_letter || vendor == "OPENAI" || vendor == "OPENROUTER" || vendor == "OLLAMA" {
        // Those three are host-gated on their own vendors' domains; borrowing
        // the name would hand our token to a check that is not about us.
        return String::new();
    }
    format!("{vendor}_API_KEY")
}

pub fn hermes_context_hint(context_window: i64) -> String {
    if context_window <= 0 {
        return String::new();
    }
    let tokens = context_window.to_string();
    format!(
        "hermes has no way to take a context-window hint from wally; this model supports {tokens} tokens — add model.context_length: {tokens} to your own ~/.hermes/config.yaml if you want hermes to budget the session against the full window"
    )
}

pub fn hermes_argv(model: &str, child_args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "--provider".to_string(),
        "custom".to_string(),
        "--model".to_string(),
        model.to_string(),
    ];
    argv.extend(child_args.iter().cloned());
    argv
}

/// ISO-8601 UTC "now" (`%Y-%m-%dT%H:%M:%SZ`). OpenClaw treats a non-empty
/// `wizard.lastRunAt` as "onboarding complete", so this is what lets a first
/// run skip its wizard. Formatted by `util::format_utc`, not a date/time crate.
fn iso_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    crate::util::format_utc(epoch)
}

/// The OpenClaw config naming `model` at `base_url`, merged onto whatever the
/// person already has in `openclaw.json`. Built through `io::json::dump` (the
/// C++ called `.dump()`); key order is not hand-controlled here because
/// nlohmann's default object is a `std::map` — the same alphabetical order
/// `io::json::dump` produces.
pub fn build_open_claw_config(
    existing: &str,
    primary: &str,
    base_url: &str,
    api_key: &str,
    models: &[CatalogModel],
) -> String {
    let mut config: Value = if !existing.is_empty() {
        match serde_json::from_str::<Value>(existing) {
            Ok(parsed) if parsed.is_object() => parsed,
            _ => json!({}),
        }
    } else {
        json!({})
    };

    // `mode: merge` keeps the catalogs from their own providers; this adds
    // ours and selects one for the run.
    config["models"]["mode"] = json!("merge");
    let mut entries: Vec<Value> = Vec::new();
    for model in models {
        let mut entry = json!({
            "id": model.id,
            "name": model.id,
            "input": ["text"],
            // Only capabilities checked against this gateway. It returns
            // usage on the final streaming chunk when
            // `stream_options.include_usage` is set, and it takes
            // `max_tokens` rather than `max_completion_tokens`.
            "compat": {
                "supportsUsageInStreaming": true,
                "maxTokensField": "max_tokens",
            },
        });
        if model.context_window > 0 {
            entry["contextWindow"] = json!(model.context_window);
            // OpenClaw wants a maxTokens beside the window; without one it
            // budgets the session against a cap it invented.
            let max_tokens = if model.max_output > 0 {
                model.max_output
            } else {
                model.context_window.min(65536)
            };
            entry["maxTokens"] = json!(max_tokens);
        }
        if model.input_per_mtok > 0 || model.output_per_mtok > 0 {
            // Per-million-token rates, in whole currency units. The catalog
            // carries micro-dollars, so this is the same number a dollar
            // sign away.
            const PER_MICRO: f64 = 1.0 / 1_000_000.0;
            entry["cost"] = json!({
                "input": model.input_per_mtok as f64 * PER_MICRO,
                "output": model.output_per_mtok as f64 * PER_MICRO,
                "cacheRead": 0,
                "cacheWrite": 0,
            });
        }
        entries.push(entry);
    }
    config["models"]["providers"][PROVIDER_ID] = json!({
        "baseUrl": base_url,
        "apiKey": key_or_placeholder(api_key),
        "api": "openai-completions",
        // OpenClaw's per-provider static headers (checked against OpenClaw
        // 2026.6.35). On a generic OpenAI-compatible provider it sends the
        // OpenAI SDK's User-Agent, which names no harness at all.
        "headers": { HARNESS_HEADER: harness_header_value(DeclaredHarness::KOpenclaw) },
        "models": entries,
    });
    config["agents"]["defaults"]["model"]["primary"] = json!(format!("{PROVIDER_ID}/{primary}"));
    // Mark onboarding done so a first launch skips OpenClaw's wizard: it goes
    // straight into the tui against the provider we just wrote, no setup
    // page.
    config["wizard"]["lastRunAt"] = json!(iso_now());
    dump(&config)
}

/// Whether the person gave dsh a job to do rather than flags for its web app.
///
/// The FIRST token only. A later bare word is a flag's value — `--port 8080`
/// is the web app being configured, not a prompt — and dsh reads its own
/// command line the same way: the first token the launcher does not
/// recognise is where the app's arguments start.
pub fn deep_seek_wants_headless(args: &[String]) -> bool {
    args.first()
        .map(|first| !first.is_empty() && !first.starts_with('-'))
        .unwrap_or(false)
}

/// The settings document dsh reads our provider out of. JSON on purpose: the
/// settings file's extension picks the format, which keeps a hand-written
/// YAML document out of this. Built through `io::json::dump`, as the C++
/// `.dump()`'d it.
pub fn build_deep_seek_settings(
    base_url: &str,
    key_variable: &str,
    models: &[CatalogModel],
) -> String {
    let mut entries: Vec<Value> = Vec::new();
    for model in models {
        let mut entry = json!({ "id": model.id, "name": model.id });
        if model.context_window > 0 {
            entry["contextWindow"] = json!(model.context_window);
            let max_tokens = if model.max_output > 0 {
                model.max_output
            } else {
                model.context_window.min(32768)
            };
            entry["maxTokens"] = json!(max_tokens);
        }
        entries.push(entry);
    }
    let provider = json!({
        "displayName": "RunAnywhere",
        "api": "openai-completions",
        "baseURL": base_url,
        "models": entries,
        // Always referenced, local server included. This used to be omitted
        // for a loopback endpoint on the theory that no reference meant a
        // keyless route; dsh 0.1.5 instead refuses the turn with "No API key
        // for provider: runanywhere" before any request is made. The
        // variable carries a placeholder for a local server, which ignores
        // the Authorization header anyway.
        "apiKeyEnv": key_variable,
        // dsh's llm-pi-ai profile `headers`, sent on every provider request
        // (checked against dsh 0.1.5).
        "headers": { HARNESS_HEADER: harness_header_value(DeclaredHarness::KDeepseek) },
    });
    let settings = json!({
        "llm-pi-ai": { "providers": { PROVIDER_ID: provider } },
    });
    dump(&settings)
}

/// A YAML single-quoted scalar: the value wrapped in `'...'` with every
/// single quote doubled. A temp path or model id has no business carrying a
/// quote, but an unescaped one would break the whole patch document rather
/// than fail loudly, so the boundary is closed here.
fn yaml_single_quoted(value: &str) -> String {
    let mut escaped = String::from("'");
    for c in value.chars() {
        if c == '\'' {
            escaped.push_str("''");
        } else {
            escaped.push(c);
        }
    }
    escaped.push('\'');
    escaped
}

/// The `--patch` overlay pointing dsh's settings row at `settings_path` and
/// selecting `model` on our provider for a fresh agent. YAML because a
/// cordis patch is YAML, and safe to build by hand because every value in it
/// is either a fixed string or a path wally just created — not run through
/// `io::json::dump`, since the C++ built this one by hand too.
pub fn build_deep_seek_patch(settings_path: &str, model: &str) -> String {
    format!(
        "- id: settings\n  config:\n    path: {}\n- id: agent-default-model\n  config:\n    provider: {PROVIDER_ID}\n    model: {}\n",
        yaml_single_quoted(settings_path),
        yaml_single_quoted(model),
    )
}

pub fn launch_agent(agent: &Agent, model: &str, args: &[String], options: &GlobalOptions) -> i32 {
    // A launch killed before its own Drop runs (crash, kill -9, a closed
    // terminal) leaves its config behind; sweep those before this one
    // possibly writes another, rather than only ever growing the pile.
    clean_leftover_configs();
    if model.is_empty() {
        // Nothing to wire, so do not pretend to. Same contract as
        // `wally opencode` with no model.
        return launch(agent.command, "", args, options);
    }

    let Some(endpoint) = resolve(model, options, agent.id) else {
        return 1;
    };
    let child_args = effective_args(agent, args);

    let mut config = TemporaryConfig::new();
    // The second document the dsh overlay needs; unused by the other
    // handoffs and removed with the first.
    let mut settings = TemporaryConfig::new();
    let status = match agent.handoff {
        Handoff::CustomEndpointEnvironment => {
            let base = ScopedEnv::new("CUSTOM_BASE_URL", &endpoint.base_url);
            let provider = ScopedEnv::new("HERMES_INFERENCE_PROVIDER", "custom");
            // Both names: the TUI launcher reads HERMES_MODEL and the
            // oneshot path reads HERMES_INFERENCE_MODEL, and which one runs
            // depends on arguments wally does not control.
            let model_env = ScopedEnv::new("HERMES_INFERENCE_MODEL", model);
            let tui_model = ScopedEnv::new("HERMES_MODEL", model);
            if !base.applied()
                || !provider.applied()
                || !model_env.applied()
                || !tui_model.applied()
            {
                out::error_line(&format!("could not set the endpoint for {}", agent.id));
                release(&endpoint);
                return 1;
            }
            // A key only reaches a host whose own name asks for it. An
            // upstream endpoint gets one under that name; a loopback server
            // is handed none, which is what it expects.
            let key_variable = hermes_key_variable(&endpoint.base_url);
            let _key: Option<ScopedEnv> = if !endpoint.api_key.is_empty()
                && !key_variable.is_empty()
            {
                Some(ScopedEnv::new(&key_variable, &endpoint.api_key))
            } else {
                if !endpoint.api_key.is_empty() {
                    out::status_line(
                        "this endpoint takes no host-gated key name; hermes will call it unauthenticated",
                    );
                }
                None
            };
            // The environment is not enough on its own. `model.provider` in
            // their config.yaml outranks HERMES_INFERENCE_PROVIDER, and with
            // the provider left at `auto` Hermes guesses one from the model
            // id -- `glm-5.3-flash` resolves to `zai`, which then fails for
            // want of a ZAI key, or worse routes the prompt to a third party
            // the person never chose. Naming it on the argv is what actually
            // pins the route, and it only applies on the `-z` and `--tui`
            // paths, which is why `--tui` is this row's default. Surfaced,
            // not injected — see `hermes_context_hint`'s doc comment for why
            // there is nothing for wally to write instead.
            let limits = lookup_limits(&endpoint, model);
            let hint = hermes_context_hint(limits.context_window);
            if !hint.is_empty() {
                out::status_line(&hint);
            }
            out::status_line(&format!(
                "{} will talk to {model} through {}",
                agent.id, endpoint.base_url
            ));
            launch(agent.id, "", &hermes_argv(model, &child_args), options)
        }
        Handoff::ConfigFile => {
            let catalog = catalog_models_for(&endpoint, model);
            if catalog[0].context_window > 0 {
                out::status_line(&format!(
                    "context window: {} tokens",
                    catalog[0].context_window
                ));
            }
            let built = build_open_claw_config(
                &read_open_claw_config(),
                model,
                &endpoint.base_url,
                &endpoint.api_key,
                &catalog,
            );
            if let Err(failure) = config.write(&built, ".json") {
                out::error_line(&failure);
                release(&endpoint);
                return 1;
            }
            let state = open_claw_state_directory();
            if state.as_os_str().is_empty() {
                out::error_line("could not work out where openclaw keeps its state");
                release(&endpoint);
                return 1;
            }
            // Pinned before the config path, because OpenClaw derives the
            // state directory from the config file's folder when this is
            // unset.
            let state_dir = ScopedEnv::new("OPENCLAW_STATE_DIR", &state.to_string_lossy());
            let path = ScopedEnv::new("OPENCLAW_CONFIG_PATH", &config.path());
            if !state_dir.applied() || !path.applied() {
                out::error_line(&format!("could not set the endpoint for {}", agent.id));
                release(&endpoint);
                return 1;
            }
            out::status_line(&format!(
                "{} will talk to {model} through {}",
                agent.id, endpoint.base_url
            ));
            launch(agent.command, "", &child_args, options)
        }
        Handoff::PatchOverlay => {
            let catalog = catalog_models_for(&endpoint, model);
            if catalog[0].context_window > 0 {
                out::status_line(&format!(
                    "context window: {} tokens",
                    catalog[0].context_window
                ));
            }
            let settings_built =
                build_deep_seek_settings(&endpoint.base_url, DEEP_SEEK_KEY_VARIABLE, &catalog);
            let write_failure = settings.write(&settings_built, ".json").err().or_else(|| {
                config
                    .write(&build_deep_seek_patch(&settings.path(), model), ".yml")
                    .err()
            });
            if let Some(failure) = write_failure {
                out::error_line(&failure);
                release(&endpoint);
                return 1;
            }

            // The real key for a hosted model; a placeholder for a local
            // server, which dsh insists on having and the server never
            // reads.
            let key_value = if endpoint.api_key.is_empty() {
                "local"
            } else {
                &endpoint.api_key
            };
            let key = ScopedEnv::new(DEEP_SEEK_KEY_VARIABLE, key_value);
            if !key.applied() {
                out::error_line(&format!("could not set the endpoint for {}", agent.id));
                release(&endpoint);
                return 1;
            }

            // `--patch` belongs to the launcher, so it goes ahead of
            // anything the app itself parses. A prompt of their own
            // switches the profile: dsh's interactive surface is the
            // browser, and its terminal entry is one-shot.
            let names_profile = child_args.iter().any(|arg| arg == "--profile");
            let mut launch_args: Vec<String> = if names_profile {
                // They are driving the launcher themselves; add the overlay
                // and stay out of the way.
                vec!["--patch".to_string(), config.path()]
            } else if deep_seek_wants_headless(&child_args) {
                vec![
                    "--profile".to_string(),
                    "headless".to_string(),
                    "--patch".to_string(),
                    config.path(),
                ]
            } else {
                out::status_line("opening the dsh web ui; pass a prompt to run headless instead");
                vec!["web".to_string(), "--patch".to_string(), config.path()]
            };
            launch_args.extend(child_args.iter().cloned());
            out::status_line(&format!(
                "{} will talk to {model} through {}",
                agent.id, endpoint.base_url
            ));
            launch(agent.command, "", &launch_args, options)
        }
    };

    release(&endpoint);
    status
}

#[cfg(test)]
mod tests {
    //! OpenClaw's non-UTF-8 environment, setenv's NUL truncation, and the
    //! temp-directory lookup, each as the C++ behaved.
    use super::*;
    use crate::util::env_lock::lock as env_lock;

    /// `setenv(name, value.c_str(), 1)` in C++ truncates silently
    /// at the first embedded NUL byte rather than failing; `set_environment`
    /// must do the same instead of refusing to set the variable at all.
    #[test]
    fn set_environment_truncates_value_at_first_nul_byte() {
        let _lock = env_lock();
        const NAME: &str = "WALLY_FIX_HARNESS_NUL_TEST_VAR";
        let previous = std::env::var_os(NAME);

        let applied = set_environment(NAME, "abc\0def");
        assert!(
            applied,
            "an embedded NUL in value must truncate, not reject, matching setenv(value.c_str())"
        );
        assert_eq!(
            std::env::var_os(NAME).as_deref(),
            Some(std::ffi::OsStr::new("abc")),
            "value must be truncated at the first NUL byte, matching c_str() semantics"
        );

        // SAFETY: env_lock() is held for this whole test body.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(NAME, value),
                None => std::env::remove_var(NAME),
            }
        }
    }

    /// `OpenClawStateDirectory` reads `std::getenv` (raw bytes,
    /// encoding-agnostic) in C++; `open_claw_state_directory` must use
    /// `var_os` so a non-UTF-8 HOME still resolves instead of silently
    /// looking unset.
    #[cfg(unix)]
    #[test]
    fn open_claw_state_directory_resolves_non_utf8_home() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let _lock = env_lock();
        let saved_home = std::env::var_os("HOME");
        let saved_state_dir = std::env::var_os("OPENCLAW_STATE_DIR");
        let saved_openclaw_home = std::env::var_os("OPENCLAW_HOME");

        // SAFETY: env_lock() is held for this whole test body.
        unsafe {
            std::env::remove_var("OPENCLAW_STATE_DIR");
            std::env::remove_var("OPENCLAW_HOME");
        }
        let non_utf8_home = OsString::from_vec(vec![b'/', b't', 0xFF, 0xFE, b'p']);
        // SAFETY: as above.
        unsafe { std::env::set_var("HOME", &non_utf8_home) };

        let state_dir = open_claw_state_directory();

        assert_eq!(
            state_dir,
            Path::new(&non_utf8_home).join(".openclaw"),
            "a non-UTF-8 HOME must still resolve to HOME/.openclaw, not an empty path"
        );

        // SAFETY: as above.
        unsafe {
            match saved_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match saved_state_dir {
                Some(value) => std::env::set_var("OPENCLAW_STATE_DIR", value),
                None => std::env::remove_var("OPENCLAW_STATE_DIR"),
            }
            match saved_openclaw_home {
                Some(value) => std::env::set_var("OPENCLAW_HOME", value),
                None => std::env::remove_var("OPENCLAW_HOME"),
            }
        }
    }
    #[test]
    #[cfg(not(windows))]
    fn temp_directory_follows_the_cpp_lookup_order() {
        let _lock = env_lock();
        let names = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"];
        let saved: Vec<_> = names.iter().map(|n| (n, std::env::var_os(n))).collect();
        let tmp = tempfile::tempdir().expect("temp dir");
        // SAFETY: env_lock serializes every test here that touches the env.
        unsafe {
            for name in names {
                std::env::remove_var(name);
            }
        }
        assert_eq!(temp_directory_path(), Some(PathBuf::from("/tmp")));
        unsafe { std::env::set_var("TEMP", tmp.path()) };
        assert_eq!(temp_directory_path(), Some(tmp.path().to_path_buf()));
        unsafe { std::env::set_var("TMPDIR", "") };
        assert_eq!(
            temp_directory_path(),
            None,
            "a set-but-empty TMPDIR wins and is not a directory"
        );
        unsafe {
            for (name, value) in saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[test]
    fn is_leftover_config_name_matches_only_temporary_configs_own_naming() {
        assert!(is_leftover_config_name("wally-agent-12345.json"));
        assert!(is_leftover_config_name("wally-agent-0.yaml"));
        // No digits at all is not a name `TemporaryConfig::write` ever produced.
        assert!(!is_leftover_config_name("wally-agent-.json"));
        assert!(!is_leftover_config_name("wally-agent-.yaml"));
        // A non-digit anywhere in the id must reject, not just stop matching.
        assert!(!is_leftover_config_name("wally-agent-12a45.json"));
        // Wrong extension, wrong prefix, and no extension at all.
        assert!(!is_leftover_config_name("wally-agent-12345.yml"));
        assert!(!is_leftover_config_name("other-agent-12345.json"));
        assert!(!is_leftover_config_name("wally-agent-12345"));
        assert!(!is_leftover_config_name(""));
    }

    /// Backdates `path`'s mtime by `age` so `is_removable_leftover_config` sees
    /// a file older than `LEFTOVER_CONFIG_MAX_AGE`.
    fn backdate(path: &Path, age: std::time::Duration) {
        // Write access: Windows refuses SetFileTime on a read-only handle.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for backdating");
        let backdated = std::time::SystemTime::now() - age;
        file.set_modified(backdated).expect("set_modified");
    }

    #[test]
    fn is_removable_leftover_config_rejects_a_file_younger_than_the_max_age() {
        // tempdir() reads TMPDIR, which the sweep test below points at a
        // directory it deletes when it finishes.
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wally-agent-1.json");
        std::fs::write(&path, "{}").expect("write fixture");
        let metadata = std::fs::symlink_metadata(&path).expect("metadata");
        assert!(
            !is_removable_leftover_config(&metadata, std::time::SystemTime::now()),
            "a file written moments ago may belong to a launch that is still running"
        );
    }

    #[test]
    fn is_removable_leftover_config_accepts_a_file_older_than_the_max_age() {
        // tempdir() reads TMPDIR, which the sweep test below points at a
        // directory it deletes when it finishes.
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wally-agent-2.json");
        std::fs::write(&path, "{}").expect("write fixture");
        backdate(
            &path,
            LEFTOVER_CONFIG_MAX_AGE + std::time::Duration::from_secs(60),
        );
        let metadata = std::fs::symlink_metadata(&path).expect("metadata");
        assert!(is_removable_leftover_config(
            &metadata,
            std::time::SystemTime::now()
        ));
    }

    #[test]
    fn is_removable_leftover_config_rejects_a_directory() {
        // tempdir() reads TMPDIR, which the sweep test below points at a
        // directory it deletes when it finishes.
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let sub = dir.path().join("wally-agent-3.json");
        std::fs::create_dir(&sub).expect("mkdir fixture");
        let metadata = std::fs::symlink_metadata(&sub).expect("metadata");
        assert!(!is_removable_leftover_config(
            &metadata,
            std::time::SystemTime::now()
        ));
    }

    #[test]
    #[cfg(unix)]
    fn is_removable_leftover_config_rejects_a_symlink_even_when_old() {
        // tempdir() reads TMPDIR, which the sweep test below points at a
        // directory it deletes when it finishes.
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let target = dir.path().join("wally-agent-4-target.json");
        std::fs::write(&target, "{}").expect("write fixture");
        backdate(
            &target,
            LEFTOVER_CONFIG_MAX_AGE + std::time::Duration::from_secs(60),
        );
        let link = dir.path().join("wally-agent-4.json");
        std::os::unix::fs::symlink(&target, &link).expect("symlink fixture");
        let metadata = std::fs::symlink_metadata(&link).expect("metadata");
        assert!(
            !is_removable_leftover_config(&metadata, std::time::SystemTime::now()),
            "never follow a symlink into deleting something else, no matter its age"
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn clean_leftover_configs_removes_only_its_own_old_leftovers() {
        let _lock = env_lock();
        let names = ["TMPDIR", "TMP", "TEMP", "TEMPDIR"];
        let saved: Vec<_> = names.iter().map(|n| (n, std::env::var_os(n))).collect();
        let dir = tempfile::tempdir().expect("temp dir");
        // SAFETY: env_lock serializes every test here that touches the env.
        unsafe {
            for name in names {
                std::env::remove_var(name);
            }
            std::env::set_var("TMPDIR", dir.path());
        }

        let old_json = dir.path().join("wally-agent-100.json");
        let old_yaml = dir.path().join("wally-agent-200.yaml");
        let young = dir.path().join("wally-agent-300.json");
        let unrelated = dir.path().join("wally-agent-400.txt");
        for path in [&old_json, &old_yaml, &young, &unrelated] {
            std::fs::write(path, "{}").expect("write fixture");
        }
        let old_age = LEFTOVER_CONFIG_MAX_AGE + std::time::Duration::from_secs(60);
        backdate(&old_json, old_age);
        backdate(&old_yaml, old_age);
        backdate(&unrelated, old_age);
        // `young` keeps its just-written mtime: still inside the age window.

        clean_leftover_configs();

        assert!(
            !old_json.exists(),
            "an old wally-agent-*.json must be removed"
        );
        assert!(
            !old_yaml.exists(),
            "an old wally-agent-*.yaml must be removed"
        );
        assert!(
            young.exists(),
            "a fresh leftover may belong to a running launch"
        );
        assert!(
            unrelated.exists(),
            "a name that only partly matches must never be touched"
        );

        // SAFETY: as above.
        unsafe {
            for (name, value) in saved {
                match value {
                    Some(v) => std::env::set_var(name, v),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}
