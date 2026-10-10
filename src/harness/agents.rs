//! The coding-agent table and the per-agent config builders (port of
//! src/harness/agents.cpp).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::account::{self, ConsoleClient};
use crate::bootstrap::GlobalOptions;
use crate::io::json::{dump, dump_pretty};
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
    /// A temp extension, loaded with `-e`, that registers our provider for
    /// the one run. Nothing is written into the person's own agent directory.
    Extension,
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
        Agent {
            id: "prime-agent",
            command: "prime-agent",
            summary: "Open Prime Agent with a model",
            handoff: Handoff::Extension,
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
/// `wally-agent-<digits>` with a `.json`, `.yaml` or `.js` extension. Nothing
/// else in the temp directory matches, including a name we wrote ourselves
/// for a different purpose.
fn is_leftover_config_name(name: &str) -> bool {
    for extension in [".json", ".yaml", ".js"] {
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

/// The environment variable the dsh and Prime Agent providers read the key
/// from. Fixed rather than host-derived: both resolve the reference
/// themselves and put no host rule on the name. It also keeps the key out of
/// the files they read.
const KEY_VARIABLE: &str = "RUNANYWHERE_API_KEY";

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
/// `OPENCLAW_STATE_DIR` wins, then `<home>/.openclaw` — the order
/// `resolveStateDir` (src/config/state-dir.ts) uses. Both go through the same
/// home OpenClaw itself resolves against (`effective_home`: `OPENCLAW_HOME`,
/// then the OS home) and a `~` in either is expanded the same way, so this
/// finds the directory OpenClaw really uses even when a variable was set to a
/// literal `~/...`. Naming the state directory explicitly
/// matters more than it looks: OpenClaw otherwise derives it from the config
/// file's own folder, so pointing `OPENCLAW_CONFIG_PATH` at a temp file
/// would move their agents and sessions into the temp directory for the run.
fn open_claw_state_directory() -> PathBuf {
    // `var_os`, not `var`: `std::getenv` in C++ returns the raw bytes
    // regardless of encoding, and `std::filesystem::path` is encoding-agnostic
    // on POSIX, so a legacy-encoded HOME/OPENCLAW_* must still resolve here
    // instead of silently looking unset. Only a UTF-8 value can carry a `~`
    // to expand; anything else is used as given.
    if let Some(state) = std::env::var_os("OPENCLAW_STATE_DIR") {
        if !state.is_empty() {
            return match state.to_str() {
                Some(state) => expand_home(state),
                None => PathBuf::from(state),
            };
        }
    }
    match effective_home() {
        Some(home) => Path::new(&home).join(".openclaw"),
        None => PathBuf::new(),
    }
}

/// Drop our provider from every agent's generated `models.json`, so OpenClaw
/// rebuilds it from this run's config. In its default `merge` mode OpenClaw
/// keeps an existing provider's `apiKey` and `baseUrl` over the config's, so
/// the first run's key and endpoint would otherwise stick: a new login, a
/// local server on a fresh port, or a switch to hosted all failed auth. Other
/// providers in the file are left as they are.
///
/// Agents live under `<state>/agents/<id>/agent` unless their config entry
/// (`agents.entries` or `agents.list`) names an `agentDir`, the same lookup as
/// OpenClaw's `resolveAgentDir`.
fn drop_stale_open_claw_provider(state: &Path, config: &str) {
    let mut agent_dirs: Vec<PathBuf> = std::fs::read_dir(state.join("agents"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path().join("agent"))
                .collect()
        })
        .unwrap_or_default();
    let config: Value = serde_json::from_str(config).unwrap_or(Value::Null);
    let roster = config["agents"]
        .get("entries")
        .or_else(|| config["agents"].get("list"));
    let entries: Vec<&Value> = match roster {
        Some(Value::Object(entries)) => entries.values().collect(),
        Some(Value::Array(entries)) => entries.iter().collect(),
        _ => Vec::new(),
    };
    for entry in entries {
        if let Some(dir) = entry["agentDir"].as_str().map(str::trim) {
            if !dir.is_empty() {
                agent_dirs.push(expand_home(dir));
            }
        }
    }
    for dir in agent_dirs {
        let path = dir.join("models.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut document) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let removed = document
            .get_mut("providers")
            .and_then(Value::as_object_mut)
            .and_then(|providers| providers.remove(PROVIDER_ID))
            .is_some();
        if removed {
            if let Err(error) = std::fs::write(&path, dump_pretty(&document, 2)) {
                out::status_line(&format!(
                    "warning: could not update {} ({error}); openclaw may reuse an old key or endpoint",
                    path.display()
                ));
            }
        }
    }
}

/// The OS home: HOME, falling back to USERPROFILE on Windows the way
/// `open_claw_state_directory` does (PowerShell and cmd.exe leave HOME
/// unset).
fn os_home() -> Option<OsString> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE").filter(|home| !home.is_empty()));
    home
}

/// The remainder after a leading `~`, when that `~` is a home reference —
/// bare, or immediately followed by `/` or `\` — mirroring the lookahead in
/// OpenClaw's own regex (`^~(?=$|[\\/])`, `expandHomePrefix` in
/// https://github.com/openclaw/openclaw/blob/main/src/infra/home-dir.ts
/// lines 42-60). `~work` is a literal name, not a reference, so it is not
/// touched.
fn home_relative_suffix(path: &str) -> Option<&str> {
    let rest = path.strip_prefix('~')?;
    (rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\')).then_some(rest)
}

/// OpenClaw's effective home
/// (`resolveEffectiveHomeDir`, https://github.com/openclaw/openclaw/blob/main/packages/normalization-core/src/home-dir.ts
/// lines 45-62, reached from `resolveUserPath` via `resolveEffectiveAgentDir`
/// in agent-scope-config.ts): `OPENCLAW_HOME` wins over the OS home when set.
/// If `OPENCLAW_HOME` is itself `~`-relative, that `~` is expanded against
/// the OS home first; otherwise it is used as given.
fn effective_home() -> Option<OsString> {
    let openclaw_home = std::env::var_os("OPENCLAW_HOME").filter(|home| !home.is_empty());
    match openclaw_home {
        Some(openclaw_home) => match openclaw_home.to_str().and_then(home_relative_suffix) {
            Some(rest) => {
                let mut expanded = os_home()?;
                expanded.push(rest);
                Some(expanded)
            }
            None => Some(openclaw_home),
        },
        None => os_home(),
    }
}

/// `~`, `~/…`, and `~\…` against `effective_home()`, as OpenClaw's
/// `resolveUserPath` does. Only the `~` itself is substituted — like
/// `expandHomePrefix`'s regex replace, the separator character that follows
/// it (if any) is left untouched — so on POSIX a `~\...` path keeps its
/// backslash as a literal character in the last component rather than being
/// read as a directory separator, exactly as it would be under OpenClaw.
fn expand_home(path: &str) -> PathBuf {
    let Some(rest) = home_relative_suffix(path) else {
        return PathBuf::from(path);
    };
    match effective_home() {
        Some(mut home) => {
            home.push(rest);
            PathBuf::from(home)
        }
        None => PathBuf::from(path),
    }
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

/// The input modalities a harness declares for `model`: text, and image when
/// the catalog lists it (`CatalogModel::image_input`), so the harness attaches
/// an image the person gives it instead of refusing it or describing it in
/// text. Never image for a model the catalog does not say takes one.
fn input_modalities(model: &CatalogModel) -> Value {
    if model.image_input {
        json!(["text", "image"])
    } else {
        json!(["text"])
    }
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
            "input": input_modalities(model),
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
                "cacheRead": model.cached_input_per_mtok as f64 * PER_MICRO,
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

/// The `llm-pi-ai` config that registers our provider with dsh. JSON, which
/// the YAML patch embeds as a flow mapping, so every value is escaped by the
/// serializer rather than by hand.
pub fn build_deep_seek_llm_config(
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
    dump(&json!({ "providers": { PROVIDER_ID: provider } }))
}

/// The Prime Agent extension that registers our provider. The provider is a
/// JSON literal, which is also a valid JavaScript expression, so every value
/// is escaped by the serializer rather than by hand. `apiKey` names the
/// variable holding the key, not the key.
pub fn build_prime_agent_extension(
    base_url: &str,
    key_variable: &str,
    models: &[CatalogModel],
) -> String {
    let mut entries: Vec<Value> = Vec::new();
    for model in models {
        let mut entry = json!({
            "id": model.id,
            "name": model.id,
            "input": input_modalities(model),
            // The capabilities checked against this gateway, as for OpenClaw.
            "compat": {
                "supportsUsageInStreaming": true,
                "maxTokensField": "max_tokens",
            },
        });
        if model.context_window > 0 {
            entry["contextWindow"] = json!(model.context_window);
            let max_tokens = if model.max_output > 0 {
                model.max_output
            } else {
                model.context_window.min(65536)
            };
            entry["maxTokens"] = json!(max_tokens);
        }
        if model.input_per_mtok > 0 || model.output_per_mtok > 0 {
            const PER_MICRO: f64 = 1.0 / 1_000_000.0;
            entry["cost"] = json!({
                "input": model.input_per_mtok as f64 * PER_MICRO,
                "output": model.output_per_mtok as f64 * PER_MICRO,
                "cacheRead": model.cached_input_per_mtok as f64 * PER_MICRO,
                "cacheWrite": 0,
            });
        }
        entries.push(entry);
    }
    let provider = json!({
        "name": "RunAnywhere",
        "baseUrl": base_url,
        "apiKey": key_variable,
        "api": "openai-completions",
        "models": entries,
    });
    format!(
        "export default function (pi) {{\n  pi.registerProvider({}, {});\n}}\n",
        dump(&json!(PROVIDER_ID)),
        dump(&provider)
    )
}

/// `-e <extension> --model runanywhere/<model>`, then their own arguments.
pub fn prime_agent_argv(extension: &str, model: &str, child_args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "-e".to_string(),
        extension.to_string(),
        "--model".to_string(),
        format!("{PROVIDER_ID}/{model}"),
    ];
    argv.extend(child_args.iter().cloned());
    argv
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

/// The `--patch` overlay that registers our provider on dsh's `llm-pi-ai`
/// row and selects `model` on it for a fresh agent. The provider goes on the
/// row itself: dsh 0.1.7 stopped reading a settings document named by the
/// `settings` row, and the row config works on 0.1.5 as well.
pub fn build_deep_seek_patch(llm_config: &str, model: &str) -> String {
    format!(
        "- id: llm-pi-ai\n  config: {llm_config}\n- id: agent-default-model\n  config:\n    provider: {PROVIDER_ID}\n    model: {}\n",
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
            drop_stale_open_claw_provider(&state, &read_open_claw_config());
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
            let llm_config = build_deep_seek_llm_config(&endpoint.base_url, KEY_VARIABLE, &catalog);
            // `.yaml`, not `.yml`: the leftover sweep only knows `.yaml`.
            if let Err(failure) = config.write(&build_deep_seek_patch(&llm_config, model), ".yaml")
            {
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
            let key = ScopedEnv::new(KEY_VARIABLE, key_value);
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
        Handoff::Extension => {
            let catalog = catalog_models_for(&endpoint, model);
            if catalog[0].context_window > 0 {
                out::status_line(&format!(
                    "context window: {} tokens",
                    catalog[0].context_window
                ));
            }
            let built = build_prime_agent_extension(&endpoint.base_url, KEY_VARIABLE, &catalog);
            if let Err(failure) = config.write(&built, ".js") {
                out::error_line(&failure);
                release(&endpoint);
                return 1;
            }
            let key = ScopedEnv::new(KEY_VARIABLE, key_or_placeholder(&endpoint.api_key));
            if !key.applied() {
                out::error_line(&format!("could not set the endpoint for {}", agent.id));
                release(&endpoint);
                return 1;
            }
            out::status_line(&format!(
                "{} will talk to {model} through {}",
                agent.id, endpoint.base_url
            ));
            launch(
                agent.command,
                "",
                &prime_agent_argv(&config.path(), model, &child_args),
                options,
            )
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

    #[test]
    fn stale_runanywhere_provider_is_dropped_and_others_kept() {
        let state = tempfile::tempdir().unwrap();
        let agent = state.path().join("agents").join("main").join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let models = agent.join("models.json");
        std::fs::write(
            &models,
            r#"{"providers":{"runanywhere":{"baseUrl":"https://old/v1","apiKey":"old"},"openai":{"apiKey":"theirs"}}}"#,
        )
        .unwrap();

        drop_stale_open_claw_provider(state.path(), "");

        let left: Value = serde_json::from_str(&std::fs::read_to_string(&models).unwrap()).unwrap();
        assert!(left["providers"].get(PROVIDER_ID).is_none());
        assert_eq!(left["providers"]["openai"]["apiKey"], "theirs");
    }

    #[test]
    fn stale_provider_is_dropped_from_a_configured_agent_dir() {
        let state = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        let models = custom.path().join("models.json");
        std::fs::write(&models, r#"{"providers":{"runanywhere":{"apiKey":"old"}}}"#).unwrap();
        let config = json!({"agents": {"list": [{"id": "work", "agentDir": custom.path()}]}});

        drop_stale_open_claw_provider(state.path(), &config.to_string());

        let left: Value = serde_json::from_str(&std::fs::read_to_string(&models).unwrap()).unwrap();
        assert!(left["providers"].get(PROVIDER_ID).is_none());
    }

    /// Puts each named variable back on drop, so a failed assertion in a
    /// test body cannot leak them into the tests after it.
    struct RestoreEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                // SAFETY: dropped before the env_lock() guard it sits beside.
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }

    /// Runs `body` with each named variable set as given (`None` meaning
    /// unset), restoring all of them after, even if `body` panics.
    fn with_env<T>(vars: &[(&'static str, Option<&str>)], body: impl FnOnce() -> T) -> T {
        let _lock = env_lock();
        let _restore = RestoreEnv(
            vars.iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect(),
        );
        for (name, value) in vars {
            // SAFETY: env_lock() is held until after `_restore` has run.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        body()
    }

    /// Runs `body` with HOME (and on Windows USERPROFILE) set as given, `None`
    /// meaning unset, restoring both after, even if `body` panics.
    /// OPENCLAW_HOME is always cleared, so a value from the ambient
    /// environment cannot leak into these HOME/USERPROFILE-only cases.
    fn with_home<T>(home: Option<&str>, profile: Option<&str>, body: impl FnOnce() -> T) -> T {
        with_env(
            &[
                ("OPENCLAW_HOME", None),
                ("HOME", home),
                ("USERPROFILE", profile),
            ],
            body,
        )
    }

    #[test]
    fn expand_home_follows_home() {
        with_home(Some("/h"), None, || {
            assert_eq!(expand_home("~"), PathBuf::from("/h"));
            assert_eq!(expand_home("~/work"), Path::new("/h").join("work"));
            assert_eq!(expand_home("~work"), PathBuf::from("~work"));
            assert_eq!(expand_home("/abs"), PathBuf::from("/abs"));
        });
    }

    // PowerShell and cmd.exe leave HOME unset, so `~` must fall back to
    // USERPROFILE, and stay literal when that is empty too.
    #[cfg(windows)]
    #[test]
    fn expand_home_falls_back_to_userprofile_on_windows() {
        with_home(None, Some(r"C:\Users\me"), || {
            assert_eq!(
                expand_home("~/work"),
                Path::new(r"C:\Users\me").join("work")
            );
        });
        with_home(Some("/h"), Some(r"C:\Users\me"), || {
            assert_eq!(expand_home("~/work"), Path::new("/h").join("work"));
        });
        with_home(None, Some(""), || {
            assert_eq!(expand_home("~/work"), PathBuf::from("~/work"));
        });
    }

    // resolveEffectiveHomeDir (home-dir.ts lines 45-62) checks OPENCLAW_HOME
    // before falling back to the OS home.
    // The state directory resolves against the same home, with the same `~`
    // handling, as the agent directories cleaned inside it: OpenClaw's
    // resolveStateDir expands OPENCLAW_STATE_DIR through resolveHomeRelativePath
    // and otherwise uses <effective home>/.openclaw. A literal `~/...` value
    // must not become a directory named `~` under the working directory.
    #[test]
    fn open_claw_state_directory_expands_home_like_openclaw() {
        with_env(
            &[
                ("OPENCLAW_STATE_DIR", None),
                ("OPENCLAW_HOME", Some("~/oc-home")),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(
                    open_claw_state_directory(),
                    Path::new("/h").join("oc-home").join(".openclaw")
                );
            },
        );
        with_env(
            &[
                ("OPENCLAW_STATE_DIR", Some("~/state")),
                ("OPENCLAW_HOME", None),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(open_claw_state_directory(), Path::new("/h").join("state"));
            },
        );
        with_env(
            &[
                ("OPENCLAW_STATE_DIR", None),
                ("OPENCLAW_HOME", None),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(
                    open_claw_state_directory(),
                    Path::new("/h").join(".openclaw")
                );
            },
        );
    }

    #[test]
    fn expand_home_prefers_openclaw_home_over_home() {
        with_env(
            &[
                ("OPENCLAW_HOME", Some("/oc")),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(expand_home("~/work"), Path::new("/oc").join("work"));
            },
        );
    }

    // A `~`-relative OPENCLAW_HOME is itself expanded against the OS home
    // first (home-dir.ts lines 54-59), rather than taken literally.
    #[test]
    fn expand_home_expands_a_tilde_relative_openclaw_home() {
        with_env(
            &[
                ("OPENCLAW_HOME", Some("~/oc-home")),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(
                    expand_home("~/work"),
                    Path::new("/h").join("oc-home").join("work")
                );
            },
        );
    }

    // expandHomePrefix (home-dir.ts lines 42-60) substitutes only the `~`,
    // leaving the separator character after it untouched; on POSIX a
    // backslash is not a path separator, so it stays a literal character in
    // the last component instead of splitting it, exactly as it would under
    // OpenClaw.
    #[test]
    fn expand_home_recognizes_both_slash_and_backslash_prefixes() {
        with_env(
            &[
                ("OPENCLAW_HOME", None),
                ("HOME", Some("/h")),
                ("USERPROFILE", None),
            ],
            || {
                assert_eq!(expand_home("~/x"), Path::new("/h").join("x"));
                #[cfg(unix)]
                assert_eq!(expand_home("~\\x"), PathBuf::from("/h\\x"));
                #[cfg(windows)]
                assert_eq!(expand_home("~\\x"), Path::new("/h").join("x"));
            },
        );
    }

    // On Windows, backslash is a normal separator, so a fully backslashed
    // OpenClaw agentDir override resolves under the home dir like any other
    // multi-segment path.
    #[cfg(windows)]
    #[test]
    fn expand_home_resolves_a_windows_backslash_path_under_home() {
        with_home(None, Some(r"C:\Users\me"), || {
            assert_eq!(
                expand_home(r"~\agents\work"),
                Path::new(r"C:\Users\me").join("agents").join("work")
            );
        });
    }

    // A models.json wally cannot rewrite is warned about, not a crash, and
    // is left as it was.
    #[cfg(unix)]
    #[test]
    fn unwritable_models_json_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let agent = state.path().join("agents").join("main").join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let models = agent.join("models.json");
        let original = r#"{"providers":{"runanywhere":{"apiKey":"old"}}}"#;
        std::fs::write(&models, original).unwrap();
        std::fs::set_permissions(&models, std::fs::Permissions::from_mode(0o444)).unwrap();
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&models)
            .is_ok()
        {
            return; // root ignores the mode; nothing to prove here
        }

        drop_stale_open_claw_provider(state.path(), "");

        assert_eq!(std::fs::read_to_string(&models).unwrap(), original);
    }

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
        assert!(is_leftover_config_name("wally-agent-7.js"));
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

    #[test]
    fn prime_agent_extension_registers_our_provider_without_the_key() {
        let models = [CatalogModel {
            id: "glm-5.3-flash".to_string(),
            context_window: 200_000,
            max_output: 16_384,
            input_per_mtok: 0,
            output_per_mtok: 0,
            cached_input_per_mtok: 0,
            image_input: false,
        }];
        let built = build_prime_agent_extension("https://example.test/v1", KEY_VARIABLE, &models);
        let json = built
            .strip_prefix("export default function (pi) {\n  pi.registerProvider(\"runanywhere\", ")
            .and_then(|rest| rest.strip_suffix(");\n}\n"))
            .expect("the extension wraps one registerProvider call");
        let provider: Value = serde_json::from_str(json).unwrap();
        assert_eq!(provider["baseUrl"], "https://example.test/v1");
        assert_eq!(provider["apiKey"], "RUNANYWHERE_API_KEY");
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["models"][0]["id"], "glm-5.3-flash");
        assert_eq!(provider["models"][0]["contextWindow"], 200_000);
        assert_eq!(provider["models"][0]["maxTokens"], 16_384);
    }

    #[test]
    fn openclaw_and_prime_agent_declare_image_input_only_where_the_catalog_lists_it() {
        let models = [
            CatalogModel {
                id: "deepseek-v4.1-flash".to_string(),
                image_input: true,
                ..Default::default()
            },
            CatalogModel {
                id: "mimo-v2.6-pro".to_string(),
                ..Default::default()
            },
        ];
        let claw: Value = serde_json::from_str(&build_open_claw_config(
            "",
            "deepseek-v4.1-flash",
            "https://example.test/v1",
            "k",
            &models,
        ))
        .unwrap();
        let claw_models = &claw["models"]["providers"]["runanywhere"]["models"];
        assert_eq!(claw_models[0]["input"], json!(["text", "image"]));
        assert_eq!(claw_models[1]["input"], json!(["text"]));

        let built = build_prime_agent_extension("https://example.test/v1", KEY_VARIABLE, &models);
        let json = built
            .strip_prefix("export default function (pi) {\n  pi.registerProvider(\"runanywhere\", ")
            .and_then(|rest| rest.strip_suffix(");\n}\n"))
            .expect("the extension wraps one registerProvider call");
        let provider: Value = serde_json::from_str(json).unwrap();
        assert_eq!(provider["models"][0]["input"], json!(["text", "image"]));
        assert_eq!(provider["models"][1]["input"], json!(["text"]));
    }

    #[test]
    fn prime_agent_argv_names_the_extension_and_model_before_theirs() {
        let argv = prime_agent_argv(
            "/tmp/wally-agent-1.js",
            "glm-5.3-flash",
            &["-c".to_string()],
        );
        assert_eq!(
            argv,
            [
                "-e",
                "/tmp/wally-agent-1.js",
                "--model",
                "runanywhere/glm-5.3-flash",
                "-c"
            ]
        );
    }
}
