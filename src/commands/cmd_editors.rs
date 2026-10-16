//! Port of src/commands/cmd_editors.cpp.

use std::fs;
use std::path::{Path, PathBuf};

use crate::account;
use crate::anthropic::{self, ModelAliases, Shim};
use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::editor_env;
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::desktop;
use crate::harness::{self, claude_effort};
use crate::io::json::dump_pretty;
use crate::io::output as out;
use crate::util::getenv;

/// How a tool is told where the model lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wiring {
    /// Variables in the launched process. Works for anything that reads
    /// ANTHROPIC_BASE_URL itself, or spawns something that does.
    Environment,
    /// Claude Desktop's third-party gateway profile, because it ignores the
    /// environment for authentication and says so.
    ClaudeProfile,
}

/// One editor or agent wally can point at a model.
///
/// The list is the whole integration surface: a new target is a row here plus
/// whatever `apply` has to set. Everything before that — resolving the model,
/// serving it, translating the wire format — is shared, which is the point.
#[derive(Debug, Clone, Copy)]
struct Editor {
    /// What the reader types after `wally`.
    id: &'static str,
    /// An executable on PATH, or empty when this is a desktop app.
    command: &'static str,
    /// A macOS application bundle, or empty when `command` is on PATH.
    bundle: &'static str,
    summary: &'static str,
    wiring: Wiring,
    /// What the translator declares upstream for this tool (`X-RA-Harness`),
    /// so the endpoint attributes the traffic to it rather than to nobody.
    declared: harness::DeclaredHarness,
}

/// Only tools that speak the Anthropic Messages API belong here. Anything
/// OpenAI-shaped needs no translator and goes through `wally opencode`.
///
/// Claude Desktop earns its place because it forwards a fixed set of variables
/// to the Claude Code it runs inside itself, and ANTHROPIC_BASE_URL is one of
/// them. That is the same trick as `wally claude-code`, one process further out.
const EDITORS: &[Editor] = &[
    Editor {
        id: "claude-code",
        command: "claude",
        bundle: "",
        summary: "Open Claude Code with a model",
        wiring: Wiring::Environment,
        declared: harness::DeclaredHarness::KClaudeCode,
    },
    Editor {
        id: "claude-desktop",
        command: "",
        bundle: "Claude.app",
        summary: "Open Claude Desktop with a model",
        wiring: Wiring::ClaudeProfile,
        declared: harness::DeclaredHarness::KClaudeDesktop,
    },
];

/// Where `editor`'s application bundle is, or empty when it is not installed.
#[cfg(target_os = "macos")]
fn bundle_path(editor: &Editor) -> String {
    if editor.bundle.is_empty() {
        return String::new();
    }
    let mut roots = vec!["/Applications/".to_string()];
    if let Some(home) = getenv("HOME") {
        roots.push(format!("{home}/Applications/"));
    }
    for root in &roots {
        let path = format!("{root}{}", editor.bundle);
        if fs::File::open(format!("{path}/Contents/Info.plist")).is_ok() {
            return path;
        }
    }
    String::new()
}

/// Sets `name` for the child, remembering what was there so it can be undone.
pub(crate) struct ScopedEnv {
    name: String,
    previous: Option<String>,
}

impl ScopedEnv {
    pub(crate) fn new(name: &str, value: &str) -> Self {
        let previous = std::env::var(name).ok();
        std::env::set_var(name, value);
        ScopedEnv {
            name: name.to_string(),
            previous,
        }
    }
}

impl Drop for ScopedEnv {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(&self.name, value),
            None => std::env::remove_var(&self.name),
        }
    }
}

/// Removes `name` for the child and restores it on scope exit — the mirror of
/// ScopedEnv. Used to keep a stray ANTHROPIC_API_KEY in the reader's shell from
/// reaching Claude Code: our bearer token already outranks it, but its mere
/// presence makes Claude Code prompt to approve the key and warn that claude.ai
/// connectors are off.
struct ScopedUnsetEnv {
    name: String,
    previous: Option<String>,
}

impl ScopedUnsetEnv {
    fn new(name: &str) -> Self {
        let previous = std::env::var(name).ok();
        if previous.is_some() {
            std::env::remove_var(name);
        }
        ScopedUnsetEnv {
            name: name.to_string(),
            previous,
        }
    }
}

impl Drop for ScopedUnsetEnv {
    fn drop(&mut self) {
        if let Some(value) = &self.previous {
            std::env::set_var(&self.name, value);
        }
    }
}

/// Ours to own, never seeded from the real profile: the login, plus the
/// runtime and cache trees. Everything else in ~/.claude is context to keep.
const RUNTIME_ENTRIES: &[&str] = &[
    ".credentials.json",
    "projects",
    "sessions",
    "shell-snapshots",
    "statsig",
    "cache",
    "caches",
    "telemetry",
    "downloads",
    "uploads",
    "paste-cache",
    "file-history",
    "backups",
    "history.jsonl",
    ".last-cleanup",
    "chrome",
    "ide",
    "session-env",
    "mcp-needs-auth-cache.json",
    "stats-cache.json",
    ".last-update-result.json",
];

/// Recursive copy, overwriting an existing destination — `std::filesystem::copy`
/// with `recursive | overwrite_existing`, discarding errors the way the C++
/// `std::error_code` overload does.
fn copy_recursive_overwrite(src: &Path, dst: &Path) {
    if src.is_dir() {
        let _ = fs::create_dir_all(dst);
        let Ok(entries) = fs::read_dir(src) else {
            return;
        };
        for entry in entries.flatten() {
            copy_recursive_overwrite(&entry.path(), &dst.join(entry.file_name()));
        }
    } else if src.is_file() {
        let _ = fs::copy(src, dst);
    }
}

/// Variables wally sets for the Claude Code it launches.
const GATEWAY_ENV: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];

/// `settings.json` with the gateway variables dropped from its `env` block.
///
/// Claude Code applies `env` from settings over the process environment, so a
/// reader whose own settings pin `ANTHROPIC_BASE_URL` (another gateway, a
/// proxy) would see wally's endpoint silently ignored: the launched Claude Code
/// talks to their gateway, not the model wally resolved. Text that is not a JSON
/// object is returned unchanged.
fn without_gateway_env(text: &str) -> String {
    let Ok(mut doc) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.to_string();
    };
    let Some(env) = doc
        .get_mut("env")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return text.to_string();
    };
    let before = env.len();
    for name in GATEWAY_ENV {
        env.remove(*name);
    }
    if env.len() == before {
        return text.to_string();
    }
    serde_json::to_string_pretty(&doc).unwrap_or_else(|_| text.to_string())
}

/// A wally-owned config directory for the Claude Code we launch, seeded from the
/// reader's real `~/.claude` so their settings, agents, rules, skills and memory
/// come along, but WITHOUT the login: a separate dir has no claude.ai session to
/// collide with, which is exactly what silences the "connectors are disabled"
/// warning. `.credentials.json` (the login file) and the runtime/cache trees are
/// never copied. Full context is seeded on first run; the small settings files
/// refresh every run so later edits to the real profile flow through.
///
/// On macOS the active login is a Keychain entry keyed to the config-dir path,
/// so even the account metadata in `~/.claude.json` is safe to bring — verified
/// that a seeded dir still prints no warning.
fn prepare_claude_config_dir() -> Option<String> {
    let state_dir = cli_paths::state_dir();
    // An empty state_dir (no HOME, no XDG_STATE_HOME, and on Windows no
    // LOCALAPPDATA/USERPROFILE) would build the root-level "/claude", which
    // Claude Code cannot create and wally would export anyway. Leave
    // CLAUDE_CONFIG_DIR unset instead, so Claude Code falls back to its own
    // default resolution.
    if state_dir.is_empty() {
        return None;
    }
    let ours_str = format!("{state_dir}/claude");
    let ours = Path::new(&ours_str);

    // PowerShell and cmd.exe leave HOME unset; Claude Code's home there is the
    // profile.
    #[cfg(windows)]
    let home = getenv("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| getenv("USERPROFILE"));
    #[cfg(not(windows))]
    let home = getenv("HOME");
    // An empty value would resolve against the working directory, so treat it
    // as unset.
    let home = home.filter(|h| !h.is_empty());
    let og_dir = home.as_ref().map(|home| Path::new(home).join(".claude"));
    let og_json = home
        .as_ref()
        .map(|home| Path::new(home).join(".claude.json"));

    let first_run = !ours.exists();
    let _ = fs::create_dir_all(ours);

    if first_run {
        if let Some(og_dir) = &og_dir {
            if let Ok(entries) = fs::read_dir(og_dir) {
                for entry in entries.flatten() {
                    if og_dir.exists()
                        && RUNTIME_ENTRIES.contains(&entry.file_name().to_string_lossy().as_ref())
                    {
                        continue;
                    }
                    copy_recursive_overwrite(&entry.path(), &ours.join(entry.file_name()));
                }
            }
        }
    }

    // A cheap refresh every run so edits to the real settings and memory flow
    // through without re-copying the heavy trees.
    if let Some(og_dir) = &og_dir {
        let settings = og_dir.join("settings.json");
        if let Ok(text) = fs::read_to_string(&settings) {
            let _ = fs::write(ours.join("settings.json"), without_gateway_env(&text));
        }
        let memory = og_dir.join("CLAUDE.md");
        if memory.exists() {
            let _ = fs::copy(&memory, ours.join("CLAUDE.md"));
        }
    }
    if let Some(og_json) = &og_json {
        if og_json.exists() {
            let _ = fs::copy(og_json, ours.join(".claude.json"));
        }
    }

    // Mark onboarding done so a first-ever Claude Code launch skips its setup
    // wizard — the gateway and auth are already wired. Patch the seeded file, or
    // write a minimal one when the reader has no ~/.claude.json of their own.
    {
        let claude_json = ours.join(".claude.json");
        let mut doc = fs::read_to_string(&claude_json)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        doc.as_object_mut()
            .expect("filtered to an object above")
            .insert(
                "hasCompletedOnboarding".to_string(),
                serde_json::json!(true),
            );
        let _ = fs::write(&claude_json, format!("{}\n", dump_pretty(&doc, 2)));
    }

    Some(ours_str)
}

/// The context window `/v1/models` advertises for `model`, or 0 when it can't be
/// learned. A failed catalog fetch WARNS and returns 0 — it must never block a
/// launch. Fed to Claude Code as CLAUDE_CODE_MAX_CONTEXT_TOKENS, this is what
/// makes its auto-compaction fire at the model's real limit instead of a guessed
/// default (which overruns qwen/gemma's 256k and wastes glm's 1M).
pub(crate) fn cloud_context_window(model: &str) -> i64 {
    let Ok(credentials) = account::load() else {
        return 0;
    };
    if !credentials.signed_in() {
        return 0;
    }
    let console = account::ConsoleClient::new(None);
    let (result, models, error) =
        console.fetch_models(&credentials.console_url, &credentials.access_token);
    if result != account::IdentityResult::Ok {
        out::status_line(&format!(
            "could not read the model catalog ({error}); launching without a context-window hint"
        ));
        return 0;
    }
    models
        .iter()
        .find(|info| info.id == model)
        .map(|info| info.context_window)
        .unwrap_or(0)
}

/// Claude Code command-line settings carrying `env`: the variables that point
/// it at this translator, which then outrank any the reader's own settings
/// files set. With `models` (the catalog, launched model first) they also
/// carry `modelPicker`, one row per model, so `/model` lists every model the
/// account can use rather than the three family slots; the rows replace the
/// built-in Claude lineup, which nothing here serves.
fn endpoint_settings(env: &[(String, String)], models: &[harness::CatalogModel]) -> String {
    let variables: serde_json::Map<String, serde_json::Value> = env
        .iter()
        .map(|(name, value)| (name.clone(), serde_json::Value::String(value.clone())))
        .collect();
    let mut settings = serde_json::json!({ "env": variables });
    if !models.is_empty() {
        let options: Vec<serde_json::Value> = models
            .iter()
            .map(|model| {
                let mut row = serde_json::json!({
                    "model": model.id,
                    "label": model.id,
                    "description": "RunAnywhere model",
                });
                // The Claude model whose effort menu is closest to this
                // model's own levels; without one Claude Code shows the same
                // menu for every model.
                if let Some(claude) = claude_effort::behaves_as(model) {
                    row["behavesAs"] = serde_json::json!(claude);
                }
                row
            })
            .collect();
        settings["modelPicker"] = serde_json::json!({
            "options": options,
            "replaceBuiltInOptions": true,
        });
    }
    settings.to_string()
}

/// Whether a settings file Claude Code will read sets `ANTHROPIC_MODEL` in
/// its `env`: the user settings in `config_dir`, and the project settings
/// of the working directory.
fn settings_name_a_model(config_dir: Option<&str>) -> bool {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = config_dir {
        files.push(Path::new(dir).join("settings.json"));
    }
    files.push(Path::new(".claude").join("settings.json"));
    files.push(Path::new(".claude").join("settings.local.json"));
    files.iter().any(|file| {
        fs::read_to_string(file)
            .map(|text| settings_text_names_model(&text))
            .unwrap_or(false)
    })
}

fn settings_text_names_model(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|settings| settings.get("env")?.get("ANTHROPIC_MODEL").cloned())
        .is_some_and(|value| value.as_str().is_some_and(|model| !model.is_empty()))
}

/// `--settings <path>` ahead of the reader's own arguments, unless they passed
/// their own `--settings`: that one is theirs to decide, and Claude Code reads
/// one.
fn settings_args(args: &[String], path: Option<&str>) -> Vec<String> {
    let theirs = args
        .iter()
        .any(|a| a == "--settings" || a.starts_with("--settings="));
    match path {
        Some(path) if !theirs => vec!["--settings".to_string(), path.to_string()],
        Some(_) => {
            out::status_line(
                "--settings was passed, so it decides Claude Code's endpoint, not wally",
            );
            Vec::new()
        }
        None => Vec::new(),
    }
}

/// A per-session file removed when this goes out of scope, a panic included.
pub(crate) struct RemovedOnDrop(pub(crate) Option<String>);

impl Drop for RemovedOnDrop {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Writes `contents` to `path` readable by its owner only (it carries the
/// translator's token).
pub(crate) fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

/// The 1M-token window Claude Desktop can be told a model holds.
const MILLION_TOKENS: i64 = 1_000_000;

/// The smallest `--context-window` accepted: below it a coding tool's own
/// prompt and tools leave no room to work.
const MINIMUM_CONTEXT_WINDOW: i64 = 16_384;

/// The window a tool is told about: the model's own (`real`, 0 when unknown),
/// or the smaller one the person asked for with `--context-window`, which
/// makes the tool compact sooner. A request above the real window cannot be
/// served, so the real window wins and the person is told.
pub(crate) fn context_window_for_tool(real: i64, asked: Option<i64>) -> i64 {
    match asked {
        Some(asked) if real > 0 && asked > real => {
            out::status_line(&format!(
                "the model holds {real} tokens, so --context-window {asked} is capped there"
            ));
            real
        }
        Some(asked) => asked,
        None => real,
    }
}

/// The problem with a `--context-window` value, before anything is started.
pub(crate) fn context_window_problem(asked: Option<i64>) -> Option<String> {
    match asked {
        Some(asked) if asked < MINIMUM_CONTEXT_WINDOW => Some(format!(
            "--context-window must be at least {MINIMUM_CONTEXT_WINDOW} tokens"
        )),
        _ => None,
    }
}

/// The family names in `aliases` whose catalog model holds at least a
/// 1M-token window, for the desktop profile's `supports1m`.
fn million_token_aliases(aliases: &ModelAliases, catalog: &[harness::CatalogModel]) -> Vec<String> {
    aliases
        .iter()
        .filter(|(_, id)| {
            catalog
                .iter()
                .any(|model| &model.id == id && model.context_window >= MILLION_TOKENS)
        })
        .map(|(family, _)| family.clone())
        .collect()
}

/// The Anthropic family name the `index`th picker slot is listed under.
///
/// The name decides two things in the app. Its window: Claude Code, inside
/// Claude Desktop, sizes a recognised Claude model by that model's own window,
/// so Opus 5 and Haiku 5.5 run at 1M whatever the profile says, while Sonnet
/// 4.6 and Opus 4.6 run at 200K unless their entry offers 1M. And whether the
/// effort control shows: the app offers it only for models that take effort
/// (Sonnet 4.6, Opus 4.5 and later, Haiku 5.5). A slot whose model holds 1M,
/// or whose window is unknown, gets the 1M name; any other, including when
/// `--context-window` asks for less, gets a 200K one. No 200K Haiku takes
/// effort, so that slot borrows Opus 4.5 (200K, effort low to high); the
/// profile still pins it to the haiku tier (`desktop::apply_gateway`), so the
/// app's background haiku requests reach it.
fn desktop_family(
    index: usize,
    model: &harness::CatalogModel,
    context_override: Option<i64>,
) -> &'static str {
    // Held below 1M by `--context-window`: every slot takes its 1M name, which
    // all take effort, and the app's Claude Code is told to size them at
    // 200K instead (`CLAUDE_CODE_DISABLE_1M_CONTEXT`, see `run`).
    let held = context_override.is_some_and(|asked| asked < MILLION_TOKENS);
    let window = context_override.unwrap_or(model.context_window);
    let million = held || window == 0 || window >= MILLION_TOKENS;
    match (index, million) {
        (0, _) => DESKTOP_DEFAULT_FAMILY,
        (1, true) => "claude-opus-5",
        (1, false) => "claude-opus-4-6",
        (_, true) => "claude-haiku-5-5",
        (_, false) => "claude-opus-4-5-20251101",
    }
}

/// The family the launched model, first in the picker, is listed under, and
/// the one id discovery advertises.
const DESKTOP_DEFAULT_FAMILY: &str = "claude-sonnet-4-6";

/// The families Claude Desktop is told hold 1M tokens. The app picks between
/// only two windows, the Claude model's own (200K) and 1M, so a
/// `--context-window` below 1M keeps every entry at 200K.
fn desktop_million_token(
    aliases: &ModelAliases,
    catalog: &[harness::CatalogModel],
    context_override: Option<i64>,
) -> Vec<String> {
    match context_override {
        Some(asked) if asked < MILLION_TOKENS => {
            out::status_line(
                "claude desktop sizes models at 200K or 1M only; --context-window below 1M keeps 200K",
            );
            Vec::new()
        }
        _ => million_token_aliases(aliases, catalog),
    }
}

/// Starts the translator and holds it open, printing what to point at it.
///
/// Worth having beyond debugging: it is how anything that speaks the Anthropic
/// API but is not on the list above gets wired up, without wally needing to know
/// that tool exists.
fn serve(editor: &Editor, model: &str, options: &GlobalOptions) -> i32 {
    let Some(endpoint) = harness::resolve(model, options, editor.id) else {
        return 1;
    };
    // `--serve` is named after a tool too (`wally claude-code --serve`), so the
    // endpoint it holds open declares that tool.
    let Some(shim) = anthropic::start(
        &endpoint,
        model,
        editor.declared,
        options.verbose,
        "",
        &ModelAliases::new(),
        &harness::catalog_models_for(&endpoint, model),
    ) else {
        harness::release(&endpoint);
        return 1;
    };
    out::result_line(&format!("ANTHROPIC_BASE_URL={}", shim.base_url));
    out::result_line(&format!("ANTHROPIC_AUTH_TOKEN={}", shim.auth_token));
    out::status_line(&format!("serving {model}; press Ctrl-C to stop"));
    // No signal handling: Ctrl-C ends the process, and the OS reclaims the port
    // and the model. Anything subtler would be pretending this outlives it.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Puts the app back on Anthropic without starting anything.
///
/// The way out when a run was interrupted before it could undo itself.
fn restore(editor: &Editor) -> i32 {
    if let Err(failure) = desktop::restore_gateway() {
        out::error_line(&failure);
        return 1;
    }
    out::status_line(&format!(
        "{} is back on Anthropic; restart it to pick that up",
        editor.id
    ));
    0
}

fn run(
    editor: &Editor,
    model: &str,
    args: &[String],
    context_override: Option<i64>,
    options: &GlobalOptions,
) -> i32 {
    let is_bundle = !editor.bundle.is_empty();
    // Only macOS fills this in; elsewhere a bundle editor is an error.
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut bundle = String::new();
    if is_bundle {
        #[cfg(target_os = "macos")]
        {
            bundle = bundle_path(editor);
            if bundle.is_empty() {
                out::error_line(&format!("{} is not installed", editor.bundle));
                return 1;
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            out::error_line(&format!("{} is a macOS application", editor.id));
            return 1;
        }
    }

    if model.is_empty() {
        // No model named means no wiring to do, so the tool runs exactly as the
        // reader has it configured. Same contract as `wally opencode`.
        return if is_bundle {
            harness::launch(
                "open",
                "",
                &editor_env::open_args(&bundle, &Shim::default(), args, ""),
                options,
            )
        } else {
            harness::launch(editor.command, "", args, options)
        };
    }

    let Some(endpoint) = harness::resolve(model, options, editor.id) else {
        return 1;
    };

    // Claude Desktop only lists gateway models it can map to an Anthropic
    // family, so the gateway answers under one of those ids while serving the
    // model the reader asked for. Only the desktop app needs this; the CLI
    // takes the real id happily.
    let advertised = if editor.wiring == Wiring::ClaudeProfile {
        DESKTOP_DEFAULT_FAMILY.to_string()
    } else {
        model.to_string()
    };

    // Claude Desktop's picker is Anthropic-family, so each catalog model is
    // offered under a family name and the shim routes a request naming that
    // family back to the real id. The launched model is first, so it stays the
    // default (Sonnet). The CLI path takes real ids directly and needs none of it.
    // Read once: it names the picker's models and which of them take no images.
    let catalog = harness::catalog_models_for(&endpoint, model);
    let mut desktop_aliases: ModelAliases = Vec::new();
    let mut desktop_max_effort: Vec<(String, &'static str)> = Vec::new();
    if editor.wiring == Wiring::ClaudeProfile {
        // Every catalog model, each under the Claude id whose effort menu is
        // closest to its own levels (`claude_effort`). A model the list does
        // not describe takes a slot by place, and only the first three do; a
        // model left without a free Claude id is not listed.
        let names = claude_effort::desktop_names(&catalog);
        for (index, (catalog_model, name)) in catalog.iter().zip(names).enumerate() {
            let name = match name {
                Some(name) => name,
                None if index < 3 && claude_effort::effort_menu(catalog_model).is_none() => {
                    desktop_family(index, catalog_model, context_override).to_string()
                }
                None => continue,
            };
            if let Some(cap) = claude_effort::max_effort(catalog_model) {
                desktop_max_effort.push((name.clone(), cap));
            }
            desktop_aliases.push((name, catalog_model.id.clone()));
        }
    }

    let Some(mut shim) = anthropic::start(
        &endpoint,
        model,
        editor.declared,
        options.verbose,
        &advertised,
        &desktop_aliases,
        &catalog,
    ) else {
        harness::release(&endpoint);
        return 1;
    };
    out::status_line(&format!(
        "{} will talk to {model} through {}",
        editor.id, shim.base_url
    ));
    if advertised != model {
        out::status_line(&format!(
            "advertised to the app as {advertised}; the picker shows {model}"
        ));
    }

    let status;
    if editor.wiring == Wiring::ClaudeProfile {
        // The profile, not the environment: written before the app starts and
        // taken back when wally exits. `open -W` keeps wally here for the
        // session; the interrupt handler, installed *before* apply so no signal
        // slips through the gap, covers a Ctrl-C or SIGTERM, so the reader never
        // has to run `--restore` by hand. Only an uncatchable end (SIGKILL,
        // power loss), or a restore that itself fails, can still leave it
        // applied; `--restore` stays for that, and the handler says to run it.
        //
        // apply_gateway writes several files. The handler runs on ctrlc's own
        // thread and takes `applied` before restoring, so a signal mid-apply
        // waits for those writes to finish rather than racing them, and it only
        // restores once `applied` is set — there is nothing to undo before then.
        //
        // The run and the app it opened end together. `instance` is the pid of
        // the Claude Desktop this run started (0 until it is found): Ctrl-C
        // quits that instance before restoring, and the instance exiting ends
        // the run even when `open -W` does not notice.
        let applied = std::sync::Arc::new(std::sync::Mutex::new(false));
        let applied_for_signal = std::sync::Arc::clone(&applied);
        #[cfg(target_os = "macos")]
        let instance = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
        #[cfg(target_os = "macos")]
        let instance_for_signal = std::sync::Arc::clone(&instance);
        let editor_id = editor.id;
        let _restore_on_signal = crate::util::interrupt::on_interrupt(move || {
            #[cfg(target_os = "macos")]
            {
                let pid = instance_for_signal.load(std::sync::atomic::Ordering::SeqCst);
                if pid > 0 && !desktop::instance::quit(pid, std::time::Duration::from_secs(5)) {
                    out::error_line(&format!("{editor_id} did not quit; close it by hand"));
                }
            }
            let done = applied_for_signal
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // A failed restore leaves the profile applied, so say so and point
            // at the manual way out rather than exiting silently.
            if *done {
                if let Err(failure) = desktop::restore_gateway() {
                    out::error_line(&failure);
                    out::error_line(&format!(
                        "could not put {editor_id} back on Anthropic; run `wally {editor_id} --restore`"
                    ));
                }
            }
            // Mirror the default interrupt disposition (exit 130), skipping
            // destructors while the shim's thread may still be inside the SDK.
            #[cfg(unix)]
            // SAFETY: _exit has no preconditions and does not return.
            unsafe {
                libc::_exit(130)
            };
            #[cfg(not(unix))]
            std::process::exit(130);
        });
        {
            // Held across apply so the handler above blocks here on a signal
            // instead of restoring mid-write; released before `launch`.
            let mut done = applied.lock().unwrap_or_else(|poison| poison.into_inner());
            if let Err(failure) = desktop::apply_gateway(
                &shim.base_url,
                &shim.auth_token,
                &desktop_aliases,
                &desktop_million_token(&desktop_aliases, &catalog, context_override),
                &desktop_max_effort,
                &format!("RunAnywhere \u{b7} {model}"),
            ) {
                out::error_line(&failure);
                anthropic::stop(&mut shim);
                harness::release(&endpoint);
                return 1;
            }
            *done = true;
        }
        // Set once `launch` returns, so the watcher below leaves the normal
        // ending to this thread when `open -W` did notice the app quit.
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(target_os = "macos")]
        {
            let before = desktop::instance::main_pids(&bundle);
            let bundle = bundle.clone();
            let instance = std::sync::Arc::clone(&instance);
            let finished = std::sync::Arc::clone(&finished);
            let applied = std::sync::Arc::clone(&applied);
            std::thread::spawn(move || {
                let Some(pid) = desktop::instance::wait_for_new(
                    &bundle,
                    &before,
                    std::time::Duration::from_secs(60),
                ) else {
                    return;
                };
                instance.store(pid, std::sync::atomic::Ordering::SeqCst);
                desktop::instance::wait_for_exit(pid);
                std::thread::sleep(std::time::Duration::from_secs(1));
                if finished.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let _done = applied.lock().unwrap_or_else(|poison| poison.into_inner());
                if let Err(failure) = desktop::restore_gateway() {
                    out::error_line(&failure);
                    out::error_line(&format!(
                        "could not put {editor_id} back on Anthropic; run `wally {editor_id} --restore`"
                    ));
                }
                out::status_line(&format!("{editor_id} quit; wally is done"));
                // SAFETY: _exit has no preconditions and does not return; it
                // skips destructors the same way the interrupt path does.
                unsafe { libc::_exit(0) };
            });
        }
        // A new instance reads the gateway profile at startup. The one already
        // running keeps the profile it started with, and keeps whatever the
        // reader has open in it, which is the trade we want.
        let mut open_args = editor_env::open_args(&bundle, &shim, args, model);
        if context_override.is_some_and(|asked| asked < MILLION_TOKENS) {
            // Claude Code inside the app sizes the 1M names above at 200K
            // with this, the smallest window the app has.
            open_args.splice(
                2..2,
                [
                    "--env".to_string(),
                    "CLAUDE_CODE_DISABLE_1M_CONTEXT=1".to_string(),
                ],
            );
        }
        status = harness::launch("open", "", &open_args, options);
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
        let _done = applied.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Err(failure) = desktop::restore_gateway() {
            out::error_line(&failure);
        }
    } else if is_bundle {
        // An app that reads the variables itself, or spawns something that
        // does. A process only ever gets the environment it was started with,
        // so the wiring reaches a new instance and not the running one — which
        // is the whole reason `OpenArgs` passes `-n`.
        status = harness::launch(
            "open",
            "",
            &editor_env::open_args(&bundle, &shim, args, model),
            options,
        );
    } else {
        // Scoped so the reader's own environment is back before we report
        // anything, and before a later call in the same process reads it.
        let _base = ScopedEnv::new("ANTHROPIC_BASE_URL", &shim.base_url);
        // The bearer token only (auth precedence rank 2), never ANTHROPIC_API_KEY
        // (rank 3): the token already outranks any key, and setting a key is what
        // makes Claude Code prompt to approve it and warn that claude.ai
        // connectors are off. A stray key in the reader's shell is unset for the
        // same reason.
        let _token = ScopedEnv::new("ANTHROPIC_AUTH_TOKEN", &shim.auth_token);
        let _no_key = ScopedUnsetEnv::new("ANTHROPIC_API_KEY");
        // Its own config dir, seeded from the reader's ~/.claude minus the login,
        // so there is no claude.ai session to collide with (no warning) but their
        // settings and memory still apply. See prepare_claude_config_dir. With no
        // usable state dir (no HOME/XDG_STATE_HOME), leave CLAUDE_CONFIG_DIR
        // unset rather than forcing Claude Code onto an unwritable root path.
        let config_dir = prepare_claude_config_dir();
        let _config_dir = config_dir
            .as_ref()
            .map(|dir| ScopedEnv::new("CLAUDE_CONFIG_DIR", dir));
        // Claude Code budgets against the local server's configured window, or
        // the hosted catalog when available.
        let mut _context_window = None;
        // The catalog read above already carries the hosted window (the
        // launched model is first); asking the console again only when it
        // did not.
        let real = if endpoint.serving {
            endpoint.context_window
        } else if catalog[0].context_window > 0 {
            catalog[0].context_window
        } else {
            cloud_context_window(model)
        };
        let context = context_window_for_tool(real, context_override);
        if context > 0 {
            _context_window = Some(ScopedEnv::new(
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS",
                &context.to_string(),
            ));
            out::status_line(&format!("context window: {context} tokens"));
        }
        // TELL CLAUDE CODE WHICH MODEL IT IS TALKING TO, because otherwise it
        // labels our answers with its own default and reports that as fact.
        //
        // Wally 0.5.6 printed `using glm-5.3-flash`, returned the GLM sentinel,
        // and the wrapped tool's own JSON reported
        // `modelUsage.claude-sonnet-5` / `provider: firstParty`. Every hosted
        // model did the same, because nothing here ever set a model and the
        // wrapped tool has no other way to know.
        //
        // THIS CHANGES NO ROUTING. `anthropic::RequestToOpenAI` sets
        // `openai["model"] = runtime.model` on every request it forwards
        // (`src/anthropic/translate.cpp`), so the selected model is already what
        // serves and already what the ledger charges -- nine usage rows
        // reconciled correctly against the deployed catalog on 2026-09-12. Only
        // the label was wrong, and these two variables are what make it true.
        //
        // BOTH, and the second is the one that is easy to miss. Claude Code
        // makes background requests of its own -- titles, summaries -- and
        // resolves them through the `haiku` alias, not the main model. Those
        // reached us too and were served by `runtime.model` like everything
        // else: measured on the same day, an auxiliary GLM pair of 766/599
        // tokens costing 1,778 micros, and 816/821 for Qwen costing 2,790.
        // Without the second variable those calls stay labelled as a Claude
        // model that nothing here ever contacted.
        //
        // `ANTHROPIC_SMALL_FAST_MODEL` used to be the variable for that and is
        // documented as deprecated in favour of `ANTHROPIC_DEFAULT_HAIKU_MODEL`,
        // so it is deliberately not set. Names and precedence checked against
        // code.claude.com/docs/en/env-vars on 2026-09-13 rather than recalled:
        // `ANTHROPIC_MODEL` is read before the `model` settings key, and
        // `--model` or `/model` still override it, which is correct -- a reader
        // who asks for something else inside the session should get it.
        // Carry the model as a `--model` flag instead of ANTHROPIC_MODEL, so
        // the real id is never renamed and labels stay correct. If the reader
        // already passed their own --model, theirs wins and nothing is
        // prepended. ANTHROPIC_MODEL is deliberately unset here: when both
        // are present the env value shadows the flag, which would defeat this.
        let _no_selected_model = ScopedUnsetEnv::new("ANTHROPIC_MODEL");
        // Claude Code's picker is Anthropic-family (Opus/Sonnet/Haiku), not a model
        // list, so each catalog model is bound to a family slot: they all then show
        // in the picker, labelled with their real ids. The launched model is first,
        // so it stays on Haiku, the background-task default.
        const FAMILY_SLOTS: [&str; 3] = [
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
        ];
        let mut _family_slots: Vec<ScopedEnv> = Vec::new();
        for (index, catalog_model) in catalog.iter().take(3).enumerate() {
            _family_slots.push(ScopedEnv::new(FAMILY_SLOTS[index], &catalog_model.id));
        }
        // The picker renders each bound slot as "Custom <Family> model"
        // unless its description override is set, so label all three with
        // the provider. The label keeps the real id; only this suffix changes.
        const SLOT_DESCRIPTIONS: [&str; 3] = [
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION",
        ];
        let mut _slot_descriptions: Vec<ScopedEnv> = Vec::new();
        for name in SLOT_DESCRIPTIONS {
            _slot_descriptions.push(ScopedEnv::new(name, "RunAnywhere model"));
        }
        // A settings file's `env` block outranks the environment a process
        // starts with, so a reader whose ~/.claude/settings.json (seeded into
        // the dir above) or project settings point Claude Code at another
        // endpoint would bypass this translator entirely: verified with an env
        // block naming a relay. The same variables therefore also go in as
        // command-line settings, which Claude Code ranks above user and project
        // settings, in a 0600 file wally owns for this session only (the token
        // never on a command line, where any local user can read it). The
        // reader's own settings files are never written.
        let mut endpoint_env: Vec<(String, String)> = vec![
            ("ANTHROPIC_BASE_URL".to_string(), shim.base_url.clone()),
            ("ANTHROPIC_AUTH_TOKEN".to_string(), shim.auth_token.clone()),
        ];
        if context > 0 {
            endpoint_env.push((
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
                context.to_string(),
            ));
        }
        for (index, catalog_model) in catalog.iter().take(3).enumerate() {
            endpoint_env.push((FAMILY_SLOTS[index].to_string(), catalog_model.id.clone()));
        }
        for name in SLOT_DESCRIPTIONS {
            endpoint_env.push((name.to_string(), "RunAnywhere model".to_string()));
        }
        // A cloud-provider switch in the reader's settings would skip the
        // base URL altogether; Claude Code reads only 1/true/yes/on as set.
        for name in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            endpoint_env.push((name.to_string(), "0".to_string()));
        }
        // Their settings' ANTHROPIC_MODEL would shadow the --model passed
        // below, so ours outranks it; the reader's own --model is theirs, so
        // only alongside ours. Set only then: it also outranks the default a
        // person saves from `/model`, and Claude Code says so on every switch.
        if !args.iter().any(|a| a == "--model") && settings_name_a_model(config_dir.as_deref()) {
            endpoint_env.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
        }
        let settings_file = RemovedOnDrop(config_dir.as_ref().and_then(|dir| {
            let path = Path::new(dir).join(format!("wally-endpoint-{}.json", std::process::id()));
            match write_private(&path, &endpoint_settings(&endpoint_env, &catalog)) {
                Ok(()) => Some(path.to_string_lossy().into_owned()),
                Err(error) => {
                    out::status_line(&format!(
                        "could not write wally's endpoint settings ({error}); a settings.json env block may override them"
                    ));
                    None
                }
            }
        }));
        let mut launch_args = Vec::with_capacity(args.len() + 4);
        if !args.iter().any(|a| a == "--model") {
            launch_args.push("--model".to_string());
            launch_args.push(model.to_string());
        }
        launch_args.extend(settings_args(args, settings_file.0.as_deref()));
        launch_args.extend(args.iter().cloned());
        status = harness::launch(editor.command, "", &launch_args, options);
    }

    anthropic::stop(&mut shim);
    harness::release(&endpoint);
    status
}

pub fn register_editors(app: &mut App) {
    for editor in EDITORS {
        let editor = *editor;
        let command = app.add_subcommand(editor.id, editor.summary);
        let invocation = format!("wally {}", editor.id);
        command.footer(&examples_footer(&[
            Example::new(
                &format!("{invocation} -m qwen3-4b-instruct-2507"),
                "The certified local coding model",
            ),
            Example::new(
                &format!("{invocation} -m glm-5.3-flash"),
                "A hosted model (needs `wally account login`)",
            ),
        ]));
        command.add_option(
            "-m,--model",
            ValueType::Text,
            "A model on this machine, or a hosted one from your account",
        );
        command.add_flag(
            "--serve",
            "Print the endpoint and keep it open instead of launching",
        );
        if editor.wiring == Wiring::ClaudeProfile {
            command.add_flag("--restore", "Put Claude Desktop back on Anthropic and exit");
        }
        command.add_option(
            "--context-window",
            ValueType::Int64,
            "Tell the tool the model holds this many tokens, so it compacts sooner",
        );
        // Tokens after the wally flags belong to the tool, its own flags
        // included. They reach here as positionals because `run()` inserts a
        // `--` ahead of them (see SplitPassthroughArgv); CLI11 would otherwise
        // read a leading `--flag` as an unknown wally option and reject it.
        command
            .add_option("args", ValueType::Text, "Passed through to the tool")
            .multi();
        command.prefix_command(true);
        command.callback(move |p, g| {
            if p.flag("--restore") {
                return restore(&editor);
            }
            if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
                return 2;
            }
            if let Some(problem) = context_window_problem(p.get_i64("--context-window")) {
                out::error_line(&problem);
                return 2;
            }
            // A missing harness shows only that it is missing and how to get
            // it, before any model resolution or preamble. --serve holds the
            // endpoint open without launching the tool, so it needs none present.
            // Claude Desktop is an app bundle; the rest are CLIs on PATH.
            let needs_tool = !p.flag("--serve");
            if needs_tool && !editor.bundle.is_empty() {
                #[cfg(target_os = "macos")]
                {
                    if bundle_path(&editor).is_empty() {
                        const DOWNLOAD: &str = "https://claude.ai/download";
                        out::error_line(&format!("{} is not installed on this machine", editor.id));
                        // An app is installed by hand, so the most wally can
                        // do is open the page; it cannot wait for the install.
                        if crate::util::term::stdin_is_tty()
                            && crate::util::term::stderr_is_tty()
                            && harness::confirm("Open the download page? [y/N] ")
                        {
                            crate::commands::cmd_account::open_browser(DOWNLOAD);
                            out::status_line(&format!(
                                "install it from {DOWNLOAD}, then run this again"
                            ));
                        } else {
                            out::status_line(&format!(
                                "download it from {DOWNLOAD}, then run this again"
                            ));
                        }
                        return 1;
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    out::error_line(&format!("{} is a macOS application", editor.id));
                    return 1;
                }
            } else if needs_tool && !harness::ensure_installed(editor.command) {
                return 127;
            }
            let effective =
                resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
            if p.flag("--serve") {
                serve(&editor, &effective, g)
            } else {
                run(
                    &editor,
                    &effective,
                    &p.get_strs("args"),
                    p.get_i64("--context-window"),
                    g,
                )
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::env_lock::lock as env_lock;

    /// A ~/.claude/settings.json `env` block naming a relay outranks the
    /// environment wally launches Claude Code with, so wally's endpoint goes in
    /// as command-line settings, which outrank it: every variable, as `env`.
    #[test]
    fn a_smaller_context_window_is_passed_and_a_larger_one_is_capped() {
        assert_eq!(context_window_for_tool(1_048_567, None), 1_048_567);
        assert_eq!(context_window_for_tool(1_048_567, Some(50_000)), 50_000);
        assert_eq!(context_window_for_tool(200_000, Some(500_000)), 200_000);
        // Unknown real window: the asked one is all there is.
        assert_eq!(context_window_for_tool(0, Some(50_000)), 50_000);
    }

    #[test]
    fn a_context_window_too_small_for_a_session_is_refused() {
        assert!(context_window_problem(Some(1_000)).is_some());
        assert!(context_window_problem(Some(16_384)).is_none());
        assert!(context_window_problem(None).is_none());
    }

    #[test]
    fn each_slot_takes_a_name_with_the_models_window_and_effort_where_one_exists() {
        let model = |context_window: i64| harness::CatalogModel {
            id: "m".to_string(),
            context_window,
            ..Default::default()
        };
        assert_eq!(desktop_family(1, &model(1_048_567), None), "claude-opus-5");
        assert_eq!(desktop_family(1, &model(0), None), "claude-opus-5");
        assert_eq!(desktop_family(1, &model(200_000), None), "claude-opus-4-6");
        // Held below 1M, the 1M names (which take effort) are sized at 200K
        // by CLAUDE_CODE_DISABLE_1M_CONTEXT instead.
        assert_eq!(
            desktop_family(1, &model(1_048_567), Some(17_000)),
            "claude-opus-5"
        );
        assert_eq!(
            desktop_family(0, &model(200_000), None),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            desktop_family(2, &model(1_048_567), None),
            "claude-haiku-5-5"
        );
        assert_eq!(
            desktop_family(2, &model(1_048_567), Some(17_000)),
            "claude-haiku-5-5"
        );
        assert_eq!(
            desktop_family(2, &model(200_000), None),
            "claude-opus-4-5-20251101"
        );
    }

    #[test]
    fn below_1m_desktop_offers_no_1m_variant() {
        let aliases: ModelAliases = vec![("claude-sonnet-4-5".to_string(), "big".to_string())];
        let catalog = vec![harness::CatalogModel {
            id: "big".to_string(),
            context_window: 1_048_567,
            ..Default::default()
        }];
        assert!(desktop_million_token(&aliases, &catalog, Some(50_000)).is_empty());
        assert_eq!(desktop_million_token(&aliases, &catalog, None).len(), 1);
    }

    #[test]
    fn only_aliases_of_million_token_models_get_the_1m_variant() {
        let aliases: ModelAliases = vec![
            ("claude-sonnet-4-5".to_string(), "big".to_string()),
            ("claude-opus-5".to_string(), "small".to_string()),
            (
                "claude-haiku-4-5-20251001".to_string(),
                "unknown".to_string(),
            ),
        ];
        let model = |id: &str, context_window: i64| harness::CatalogModel {
            id: id.to_string(),
            context_window,
            ..Default::default()
        };
        let catalog = vec![model("big", 1_048_567), model("small", 200_000)];
        assert_eq!(
            million_token_aliases(&aliases, &catalog),
            vec!["claude-sonnet-4-5".to_string()]
        );
    }

    #[test]
    fn endpoint_settings_carry_wallys_variables_as_env() {
        let env = vec![
            (
                "ANTHROPIC_BASE_URL".to_string(),
                "http://127.0.0.1:5000".to_string(),
            ),
            (
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "loopback-token".to_string(),
            ),
        ];
        let settings: serde_json::Value =
            serde_json::from_str(&endpoint_settings(&env, &[])).unwrap();
        assert_eq!(
            settings,
            serde_json::json!({"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:5000", "ANTHROPIC_AUTH_TOKEN": "loopback-token"}})
        );
    }

    #[test]
    fn only_a_settings_file_that_sets_a_model_needs_ours_to_outrank_it() {
        assert!(settings_text_names_model(
            r#"{"env":{"ANTHROPIC_MODEL":"opus"}}"#
        ));
        assert!(!settings_text_names_model(r#"{"env":{"OTHER":"x"}}"#));
        assert!(!settings_text_names_model(
            r#"{"env":{"ANTHROPIC_MODEL":""}}"#
        ));
        assert!(!settings_text_names_model(r#"{"model":"opus"}"#));
        assert!(!settings_text_names_model("not json"));
    }

    #[test]
    fn every_catalog_model_gets_a_picker_row_in_catalog_order() {
        let ids = [
            "glm-5.3-flash",
            "deepseek-v4.1-flash",
            "mimo-v2.6-pro",
            "muse-spark",
        ];
        let catalog: Vec<harness::CatalogModel> = ids
            .iter()
            .map(|id| harness::CatalogModel {
                id: id.to_string(),
                ..Default::default()
            })
            .collect();
        let settings: serde_json::Value =
            serde_json::from_str(&endpoint_settings(&[], &catalog)).unwrap();
        let picker = &settings["modelPicker"];
        assert_eq!(picker["replaceBuiltInOptions"], true);
        let rows: Vec<&str> = picker["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["model"].as_str().unwrap())
            .collect();
        assert_eq!(rows, ids);
    }

    #[test]
    fn settings_go_ahead_of_the_readers_args_unless_they_pass_their_own() {
        let path = Some("/state/claude/wally-endpoint-1.json");
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert_eq!(
            settings_args(&args(&["-p", "hi"]), path),
            args(&["--settings", "/state/claude/wally-endpoint-1.json"])
        );
        assert!(settings_args(&args(&["--settings", "mine.json"]), path).is_empty());
        assert!(settings_args(&args(&["--settings={}"]), path).is_empty());
        assert!(settings_args(&args(&["-p", "hi"]), None).is_empty());
    }

    #[test]
    fn the_endpoint_settings_file_goes_when_the_session_does() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wally-endpoint-1.json");
        write_private(&path, "{}").unwrap();
        drop(RemovedOnDrop(Some(path.to_string_lossy().into_owned())));
        assert!(!path.exists());
    }

    /// The file carries the translator's token: its owner alone may read it.
    #[cfg(unix)]
    #[test]
    fn the_endpoint_settings_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wally-endpoint-1.json");
        write_private(&path, "{}").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn without_gateway_env_drops_only_the_gateway_variables() {
        let out = without_gateway_env(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://other","ANTHROPIC_AUTH_TOKEN":"t","KEEP":"1"},"model":"sonnet"}"#,
        );
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["env"], serde_json::json!({"KEEP": "1"}));
        assert_eq!(doc["model"], "sonnet");
    }

    #[test]
    fn without_gateway_env_leaves_other_text_alone() {
        for text in ["not json", "{}", r#"{"env":{"KEEP":"1"}}"#, "[1]"] {
            assert_eq!(without_gateway_env(text), text);
        }
    }

    // With no usable HOME or XDG state dir, `prepare_claude_config_dir` used
    // to build the root-level "/claude" and export it, so Claude Code
    // couldn't create its config and wally silently lost the setup. It must
    // return None instead, leaving CLAUDE_CONFIG_DIR unset.
    #[test]
    fn prepare_claude_config_dir_returns_none_with_no_state_dir() {
        let _lock = env_lock();
        #[cfg(windows)]
        let names: Vec<&str> = vec!["XDG_STATE_HOME", "HOME", "LOCALAPPDATA", "USERPROFILE"];
        #[cfg(not(windows))]
        let names: [&str; 2] = ["XDG_STATE_HOME", "HOME"];
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe {
            for (name, _) in &saved {
                std::env::remove_var(name);
            }
        }

        let result = prepare_claude_config_dir();

        // SAFETY: still holding env_lock().
        unsafe {
            for (name, value) in &saved {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }

        assert_eq!(
            result, None,
            "an unresolvable state dir must not export a root-level CLAUDE_CONFIG_DIR"
        );
    }
}
