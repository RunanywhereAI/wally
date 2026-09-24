//! The coding-agent table and the per-agent config builders (port of
//! src/harness/agents.cpp).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::account::{self, ConsoleClient};
use crate::io::json::dump;
use crate::io::output as out;

use super::catalog_models::{catalog_models_for, CatalogModel};
use super::harness::{launch, release, resolve, Endpoint};
use super::local_models::local_context_size;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
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

/// Sets `name`, guarding against a value that cannot be an environment
/// variable (an embedded NUL, which `std::env::set_var` panics on rather than
/// truncating the way the C runtime's setenv would); returns whether it
/// actually applied.
fn set_environment(name: &str, value: &str) -> bool {
    if name.is_empty() || name.contains('=') || name.contains('\0') || value.contains('\0') {
        return false;
    }
    // SAFETY: name/value were just checked for the byte sequences that make
    // set_var panic; ScopedEnv holds this for one launch at a time.
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

impl TemporaryConfig {
    fn new() -> Self {
        TemporaryConfig { path: None }
    }

    fn write(&mut self, contents: &str, extension: &str) -> Result<(), String> {
        let directory = std::env::temp_dir();
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
            if file.write_all(contents.as_bytes()).is_err() {
                return Err(format!(
                    "could not write the agent config to {}",
                    candidate.display()
                ));
            }
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
    if let Ok(state) = std::env::var("OPENCLAW_STATE_DIR") {
        if !state.is_empty() {
            return PathBuf::from(state);
        }
    }
    if let Ok(home) = std::env::var("OPENCLAW_HOME") {
        if !home.is_empty() {
            return Path::new(&home).join(".openclaw");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Path::new(&home).join(".openclaw");
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
        limits.context_window = local_context_size(model);
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
    let path = match std::env::var("OPENCLAW_CONFIG_PATH") {
        Ok(over) if !over.is_empty() => PathBuf::from(over),
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
/// run skip its wizard. Converts the Unix epoch with the standard
/// civil-from-days algorithm (Howard Hinnant's `civil_from_days`) rather than
/// pulling in a date/time crate for one UTC timestamp.
fn iso_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = epoch.div_euclid(86400);
    let secs_of_day = epoch.rem_euclid(86400);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
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

pub fn launch_agent(agent: &Agent, model: &str, args: &[String]) -> i32 {
    if model.is_empty() {
        // Nothing to wire, so do not pretend to. Same contract as
        // `wally opencode` with no model.
        return launch(agent.command, "", args);
    }

    let Some(endpoint) = resolve(model) else {
        return 1;
    };
    let child_args = effective_args(agent, args);

    let mut config = TemporaryConfig::new();
    // The second document the dsh overlay needs; unused by the other
    // handoffs and removed with the first.
    let mut settings = TemporaryConfig::new();
    let status;

    match agent.handoff {
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
            status = launch(agent.id, "", &hermes_argv(model, &child_args));
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
            status = launch(agent.command, "", &child_args);
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
            status = launch(agent.command, "", &launch_args);
        }
    }

    release(&endpoint);
    status
}
