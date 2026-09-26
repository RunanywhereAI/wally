//! Port of src/commands/cmd_editors.cpp.

use std::fs;
use std::path::Path;

use crate::account;
use crate::anthropic::{self, ModelAliases, Shim};
use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::editor_env;
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::desktop;
use crate::harness;
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
    },
    Editor {
        id: "claude-desktop",
        command: "",
        bundle: "Claude.app",
        summary: "Open Claude Desktop with a model",
        wiring: Wiring::ClaudeProfile,
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
struct ScopedEnv {
    name: String,
    previous: Option<String>,
}

impl ScopedEnv {
    fn new(name: &str, value: &str) -> Self {
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
        for file in ["settings.json", "CLAUDE.md"] {
            let src = og_dir.join(file);
            if src.exists() {
                let _ = fs::copy(&src, ours.join(file));
            }
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
fn cloud_context_window(model: &str) -> i64 {
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

/// Starts the translator and holds it open, printing what to point at it.
///
/// Worth having beyond debugging: it is how anything that speaks the Anthropic
/// API but is not on the list above gets wired up, without wally needing to know
/// that tool exists.
fn serve(editor: &Editor, model: &str, options: &GlobalOptions) -> i32 {
    let Some(endpoint) = harness::resolve(model, options, editor.id) else {
        return 1;
    };
    let Some(shim) = anthropic::start(&endpoint, model, options.verbose, "", &ModelAliases::new())
    else {
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

fn run(editor: &Editor, model: &str, args: &[String], options: &GlobalOptions) -> i32 {
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
        "claude-sonnet-4-5".to_string()
    } else {
        model.to_string()
    };

    // Claude Desktop's picker is Anthropic-family, so each catalog model is
    // offered under a family name and the shim routes a request naming that
    // family back to the real id. The launched model is first, so it stays the
    // default (Sonnet). The CLI path takes real ids directly and needs none of it.
    let mut desktop_aliases: ModelAliases = Vec::new();
    if editor.wiring == Wiring::ClaudeProfile {
        let catalog = harness::catalog_models_for(&endpoint, model);
        const FAMILIES: [&str; 3] = [
            "claude-sonnet-4-5",
            "claude-opus-5",
            "claude-haiku-4-5-20251001",
        ];
        for (index, catalog_model) in catalog.iter().take(3).enumerate() {
            desktop_aliases.push((FAMILIES[index].to_string(), catalog_model.id.clone()));
        }
    }

    let Some(mut shim) = anthropic::start(
        &endpoint,
        model,
        options.verbose,
        &advertised,
        &desktop_aliases,
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
        // The profile, not the environment. Written before the app starts and
        // taken back when it exits, so a crash here is the one case that leaves
        // it applied — which is what `--restore` is for.
        if let Err(failure) = desktop::apply_gateway(
            &shim.base_url,
            &shim.auth_token,
            &desktop_aliases,
            &format!("RunAnywhere \u{b7} {model}"),
        ) {
            out::error_line(&failure);
            anthropic::stop(&mut shim);
            harness::release(&endpoint);
            return 1;
        }
        // A new instance reads the gateway profile at startup. The one already
        // running keeps the profile it started with, and keeps whatever the
        // reader has open in it, which is the trade we want.
        status = harness::launch(
            "open",
            "",
            &editor_env::open_args(&bundle, &shim, args, model),
            options,
        );
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
        let _config_dir =
            prepare_claude_config_dir().map(|dir| ScopedEnv::new("CLAUDE_CONFIG_DIR", &dir));
        // Claude Code budgets against the local server's configured window, or
        // the hosted catalog when available.
        let mut _context_window = None;
        let context = if endpoint.serving {
            endpoint.context_window
        } else {
            cloud_context_window(model)
        };
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
        let _selected_model = ScopedEnv::new("ANTHROPIC_MODEL", model);
        // Claude Code's picker is Anthropic-family (Opus/Sonnet/Haiku), not a model
        // list, so each catalog model is bound to a family slot: they all then show
        // in the picker, labelled with their real ids. The launched model is first,
        // so it stays on Haiku, the background-task default.
        let catalog = harness::catalog_models_for(&endpoint, model);
        const FAMILY_SLOTS: [&str; 3] = [
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
        ];
        let mut _family_slots: Vec<ScopedEnv> = Vec::new();
        for (index, catalog_model) in catalog.iter().take(3).enumerate() {
            _family_slots.push(ScopedEnv::new(FAMILY_SLOTS[index], &catalog_model.id));
        }
        status = harness::launch(editor.command, "", args, options);
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
            // A missing harness shows only that it is missing and how to get
            // it, before any model resolution or preamble. --serve holds the
            // endpoint open without launching the tool, so it needs none present.
            // Claude Desktop is an app bundle; the rest are CLIs on PATH.
            let needs_tool = !p.flag("--serve");
            if needs_tool && !editor.bundle.is_empty() {
                #[cfg(target_os = "macos")]
                {
                    if bundle_path(&editor).is_empty() {
                        out::error_line(&format!("{} is not installed on this machine", editor.id));
                        out::status_line(
                            "download it from https://claude.ai/download, then run this again",
                        );
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
                run(&editor, &effective, &p.get_strs("args"), g)
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes every test below that touches the state-dir env vars --
    // the same pattern src/harness/agents.rs uses for its own env-touching
    // tests.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
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
