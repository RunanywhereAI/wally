//! opencode with a hosted model (port of src/harness/opencode.cpp).

use serde_json::json;

use crate::account::{self, ConsoleClient};
use crate::io::json::dump;
use crate::io::output as out;

use super::catalog_models::{catalog_models_with, output_limit, CatalogModel};
use super::declared_harness::{harness_header_value, DeclaredHarness, HARNESS_HEADER};
use super::harness::{
    model_id_is_safe, refresh_and_recheck_model, report_cloud_session_invalid,
    report_not_signed_in, verify_cloud_session,
};

/// Spawns the tool; returns its exit code (tests inject a fake).
pub type SpawnFunction = std::sync::Arc<dyn Fn(&str, &[String]) -> i32 + Send + Sync>;

const CONFIG_VARIABLE: &str = "OPENCODE_CONFIG_CONTENT";

// OpenCode 2 serves `opencode`, `opencode run` and its other session commands
// from a BACKGROUND SERVICE (`opencode serve --service`) that starts once and
// stays up. The command a launch starts is only a client of it, and the
// service read its configuration when IT started, so the
// OPENCODE_CONFIG_CONTENT set for that client never reaches it: the
// runanywhere provider does not exist there and OpenCode answers with its own
// default model (seen 2026-10-15 on 2.0.28 as "build · cat-jam-free").
// `--standalone` gives the launch a private server of its own, started with
// the launch's environment. OpenCode 1.x has no service and no such flag.
const STANDALONE_FLAG: &str = "--standalone";
// Where OpenCode 2 accepts `--standalone`, read from each command's own FLAGS
// in `opencode <command> --help` (2.0.28). Its parser refuses a flag the
// command does not declare, and a root flag placed before a subcommand, so the
// flag goes only where it is declared: after one of these commands, after
// `session <one of these>`, or first when no command follows (the interactive
// UI). Everything else (acp, debug, mcp, plugin, service, ...) is untouched.
const STANDALONE_COMMANDS: [&str; 6] = ["run", "mini", "models", "api", "stats", "reload"];
const STANDALONE_SESSION_COMMANDS: [&str; 4] = ["list", "delete", "export", "import"];
// Global flags may come before the command; the ones that take a value.
const GLOBAL_FLAGS: [&str; 6] = [
    "--help",
    "-h",
    "--version",
    "-v",
    "--wizard",
    "--print-logs",
];
const GLOBAL_VALUE_FLAGS: [&str; 2] = ["--log-level", "--completions"];

/// The major version in `opencode --version` output ("opencode v2.0.28",
/// "1.18.34"), or None when there is none to read.
pub fn open_code_major_version(output: &str) -> Option<u64> {
    output.split_whitespace().find_map(|word| {
        let digits = word.strip_prefix('v').unwrap_or(word);
        let major = digits.split('.').next()?;
        if digits.contains('.') && !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit()) {
            major.parse().ok()
        } else {
            None
        }
    })
}

/// The installed OpenCode's major version, or None when it cannot be read
/// (not installed, or an output this does not recognise).
pub fn installed_open_code_major_version() -> Option<u64> {
    let names: &[&str] = if cfg!(windows) {
        &["opencode.cmd", "opencode"]
    } else {
        &["opencode"]
    };
    names.iter().find_map(|name| {
        let output = std::process::Command::new(name)
            .arg("--version")
            .output()
            .ok()?;
        open_code_major_version(&String::from_utf8_lossy(&output.stdout))
    })
}

/// `arguments` with `--standalone` where OpenCode 2 needs it and takes it
/// (see STANDALONE_FLAG and STANDALONE_COMMANDS). Unchanged for OpenCode 1
/// or an unknown version, for a command that takes no such flag or that this
/// does not know, and when the person already chose a server (`--standalone`
/// or `--server`).
pub fn with_private_server(arguments: &[String], major_version: Option<u64>) -> Vec<String> {
    let chose_server = arguments
        .iter()
        .any(|arg| arg == STANDALONE_FLAG || arg == "--server" || arg.starts_with("--server="));
    if major_version.unwrap_or(0) < 2 || chose_server {
        return arguments.to_vec();
    }
    // The first word after any global flags (and their values).
    let mut index = 0;
    while let Some(arg) = arguments.get(index) {
        if GLOBAL_FLAGS.contains(&arg.as_str())
            || GLOBAL_VALUE_FLAGS
                .iter()
                .any(|flag| arg.starts_with(&format!("{flag}=")))
        {
            index += 1;
        } else if GLOBAL_VALUE_FLAGS.contains(&arg.as_str()) {
            index += 2;
        } else {
            break;
        }
    }
    let word = arguments.get(index).map(String::as_str);
    let insert_at = match word {
        Some(command) if STANDALONE_COMMANDS.contains(&command) => Some(index + 1),
        Some("session") => arguments
            .get(index + 1)
            .filter(|sub| STANDALONE_SESSION_COMMANDS.contains(&sub.as_str()))
            .map(|_| index + 2),
        // No command: the interactive UI, given nothing, a flag of its own, or
        // the directory to open.
        None => Some(0),
        Some(other) if other.starts_with('-') || std::path::Path::new(other).exists() => Some(0),
        // A command that takes no `--standalone`, or one this does not know.
        Some(_) => None,
    };
    let mut out = arguments.to_vec();
    if let Some(at) = insert_at {
        out.insert(at, STANDALONE_FLAG.to_string());
    }
    out
}

fn set_environment(name: &str, value: &str) -> bool {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return false;
    }
    // setenv(name, value.c_str()) stops at the first NUL; so does this.
    let value = match value.find('\0') {
        Some(index) => &value[..index],
        None => value,
    };
    // SAFETY: name was just checked for the byte sequences that make set_var
    // panic, and `value` was truncated at its first NUL (if any);
    // ScopedOpenCodeConfig holds this for one launch at a time.
    unsafe { std::env::set_var(name, value) };
    true
}

fn unset_environment(name: &str) {
    // SAFETY: removing an environment variable by a fixed, checked name.
    unsafe { std::env::remove_var(name) };
}

/// Sets `OPENCODE_CONFIG_CONTENT` for the child and restores whatever was
/// there on the way out — its own copy of harness::agents' ScopedEnv, kept
/// separate so this file diffs against opencode.cpp on its own.
struct ScopedOpenCodeConfig {
    previous: Option<std::ffi::OsString>,
    had_previous: bool,
    active: bool,
}

impl ScopedOpenCodeConfig {
    fn new() -> Self {
        // `var_os`, not `var`: `std::getenv` in C++ returns the raw bytes
        // regardless of encoding, so a pre-existing non-UTF-8 value must
        // still be captured (and restored on drop) rather than silently
        // dropped as `var`'s `Result<String, VarError>` would do.
        let previous = std::env::var_os(CONFIG_VARIABLE);
        let had_previous = previous.is_some();
        ScopedOpenCodeConfig {
            previous,
            had_previous,
            active: false,
        }
    }

    fn activate(&mut self, value: &str) -> bool {
        self.active = set_environment(CONFIG_VARIABLE, value);
        self.active
    }
}

impl Drop for ScopedOpenCodeConfig {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if self.had_previous {
            if let Some(previous) = self.previous.take() {
                // SAFETY: `previous` came from this same environment
                // variable before this guard changed it, so it is already a
                // value the OS accepted.
                unsafe { std::env::set_var(CONFIG_VARIABLE, previous) };
            }
        } else {
            unset_environment(CONFIG_VARIABLE);
        }
    }
}

/// Not an error line. Nothing went wrong — the tool simply is not here yet,
/// and the only useful thing to say is how to get it. Windows gets the shared
/// launcher's message instead.
#[cfg(not(windows))]
fn missing_opencode() {
    out::status_line("opencode is not installed on this machine");
    out::status_line("install it with `npm i -g opencode-ai`, then run this again");
}

#[cfg(not(windows))]
fn spawn(executable: &str, arguments: &[String]) -> i32 {
    use std::ffi::CString;

    let mut owned: Vec<CString> = Vec::with_capacity(arguments.len() + 1);
    owned.push(CString::new(executable).unwrap_or_default());
    for arg in arguments {
        owned.push(CString::new(arg.as_str()).unwrap_or_default());
    }
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // SAFETY: fork() duplicates the process; the child branch below only
    // calls async-signal-safe functions (execvp, _exit) before either
    // replacing its image or exiting, and never returns into the rest of
    // this function.
    let child = unsafe { libc::fork() };
    if child < 0 {
        out::error_line("could not start OpenCode");
        return 1;
    }
    if child == 0 {
        // SAFETY: `argv` is a NUL-terminated array of valid C strings kept
        // alive by `owned`, which this child branch never returns past.
        unsafe {
            libc::execvp(owned[0].as_ptr(), argv.as_ptr());
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
        out::error_line("lost track of OpenCode");
        return 1;
    }
    if !libc::WIFEXITED(status) {
        return 1;
    }
    let exit_code = libc::WEXITSTATUS(status);
    if exit_code == 127 {
        missing_opencode();
    }
    exit_code
}

#[cfg(windows)]
fn spawn(executable: &str, arguments: &[String]) -> i32 {
    // npm installs OpenCode as `opencode.cmd`, which `Command::new("opencode")`
    // never finds (it only tries `.exe`), and a batch file needs cmd.exe's
    // quoting rather than the MSVCRT rules (CVE-2024-24576). The shared
    // launcher already resolves PATHEXT names and routes batch files safely.
    super::harness::spawn(executable, arguments)
}

/// OpenCode's complete, ephemeral provider configuration for a local or
/// hosted model. Every model in `models` becomes a selectable entry with its
/// real limits (context/output) and price so OpenCode's compaction and usage
/// display are correct; a 0 for any of them omits that field. `primary` is
/// the default selection. Built through `io::json::dump`, as the C++
/// `.dump()`'d it.
pub fn build_open_code_config(
    primary: &str,
    base_url: &str,
    access_token: &str,
    models: &[CatalogModel],
) -> String {
    let mut entries = json!({});
    for model in models {
        let mut entry = json!({ "name": model.id });
        // The real limits, so opencode's context gauge and auto-compaction
        // fire at the model's actual window instead of a wrong default
        // (which makes it nag to compact and never stop). Output is a sane
        // cap, never the whole context — opencode's own docs warn against
        // that. It becomes opencode's `max_tokens`, the whole budget for
        // thinking and answer (`output_limit`).
        if let Some(output) = output_limit(model, 65536) {
            entry["limit"] = json!({ "context": model.context_window, "output": output });
        }
        // Image input exactly where the catalog lists it. opencode 1.18 reads
        // a configured model's image support from `modalities.input` (text
        // must be listed too) and its file attachments from `attachment`;
        // both default to off, so without them `-f image.png` is refused
        // ("this model doesn't support image input") or never attached.
        if model.image_input {
            entry["attachment"] = json!(true);
            entry["modalities"] = json!({ "input": ["text", "image"], "output": ["text"] });
        }
        open_code_reasoning(&mut entry, model);
        // The real price, so opencode shows spend instead of $0.00.
        // opencode's cost is USD per million tokens; the catalog is
        // micro-dollars per million, so a million micros is one dollar.
        if model.input_per_mtok > 0 || model.output_per_mtok > 0 {
            entry["cost"] = json!({
                "input": model.input_per_mtok as f64 / 1_000_000.0,
                "output": model.output_per_mtok as f64 / 1_000_000.0,
            });
            if model.cached_input_per_mtok > 0 {
                entry["cost"]["cache_read"] =
                    json!(model.cached_input_per_mtok as f64 / 1_000_000.0);
            }
        }
        entries[model.id.as_str()] = entry;
    }
    // A key is always present because opencode's OpenAI client sends an
    // Authorization header regardless; a local server ignores what is in it.
    let key = if access_token.is_empty() {
        "local"
    } else {
        access_token
    };
    let provider = json!({
        "npm": "@ai-sdk/openai-compatible",
        "name": "RunAnywhere",
        // `options.headers` rides on every request (checked against opencode
        // 1.18.31); the declaration makes attribution independent of
        // opencode's own User-Agent.
        "options": {
            "baseURL": base_url,
            "apiKey": key,
            "headers": { HARNESS_HEADER: harness_header_value(DeclaredHarness::KOpencode) },
        },
        "models": entries,
    });
    let config = json!({
        "provider": { "runanywhere": provider },
        "model": format!("runanywhere/{primary}"),
    });
    dump(&config)
}

/// OpenCode's reasoning controls for `model`, from the model list: one
/// variant per level, each sending itself as `reasoning_effort` (the
/// `@ai-sdk/openai-compatible` provider option `reasoningEffort`), plus
/// `none` where the model can turn thinking off, and the model's default
/// level as its model-level option, which a chosen variant overrides.
/// Measured against OpenCode 2.0.28 with a capture server: `-m
/// runanywhere/<id>#high` sends `reasoning_effort: "high"`, no variant sends
/// the default, and `#none` sends `"none"`. Without variants OpenCode offers
/// no effort choice for a custom provider's model at all, and the config's
/// `model` key ignores a `#variant`, so the default lives in `options`.
/// Nothing for a model the list does not describe.
fn open_code_reasoning(entry: &mut serde_json::Value, model: &CatalogModel) {
    let Some(reasoning) = model.reasoning.as_ref() else {
        return;
    };
    let mut variants = serde_json::Map::new();
    for level in &reasoning.levels {
        variants.insert((*level).to_string(), json!({ "reasoningEffort": level }));
    }
    if reasoning.thinking_off {
        variants.insert("none".to_string(), json!({ "reasoningEffort": "none" }));
    }
    if variants.is_empty() {
        return;
    }
    entry["reasoning"] = json!(true);
    entry["variants"] = serde_json::Value::Object(variants);
    if let Some(default_level) = reasoning.default_level {
        entry["options"] = json!({ "reasoningEffort": default_level });
    }
}

/// Compatibility name for callers configuring a hosted catalog.
pub fn build_open_code_cloud_config(
    primary: &str,
    base_url: &str,
    access_token: &str,
    models: &[CatalogModel],
) -> String {
    build_open_code_config(primary, base_url, access_token, models)
}

/// Test seam for the console refresh transport and child process.
pub fn launch_open_code_cloud_with(
    model: &str,
    arguments: &[String],
    console: &ConsoleClient,
    spawn: &SpawnFunction,
) -> i32 {
    // The same two gates the local path gets. This function does not go
    // through harness::resolve(), so before this it had its own weaker
    // model check (control characters only, so `/` and `<` sailed through)
    // and trusted signed_in() — a non-empty string — as proof of a session.
    // A fabricated token launched a real editor against the hosted
    // endpoint.
    if !model_id_is_safe(model) {
        out::error_line(&format!("'{model}' is not a valid model id"));
        return 2;
    }
    // `--cloud` skips harness::resolve(), and with it resolve()'s refusal of
    // decision models, so the same refusal runs here, before any session
    // or catalog call.
    if super::is_decisions_model(model) {
        out::error_line(&super::decisions_model_refusal(model));
        return 2;
    }

    let mut credentials = match account::load() {
        Ok(c) => c,
        Err(message) => {
            out::error_line(&message);
            return 1;
        }
    };
    if !credentials.signed_in() {
        report_not_signed_in();
        return 1;
    }
    // Before the catalog cache gate below, not after: that gate can reject a
    // newly cataloged model outright, and a stale or expired access token
    // must not be given the chance to fail the identity check first. Refresh
    // here so the cache check that follows runs with a token already known
    // good, instead of dead-ending a valid refresh token on "server is busy".
    match verify_cloud_session(console, &mut credentials) {
        Ok(_) => {}
        Err(err) => {
            if !err.unverified {
                report_cloud_session_invalid(model);
                return 1;
            }
            // The console could not be asked right now. That is not a
            // disproof of the session already on disk, and refusing here
            // locks a signed-in person out of their harness over a
            // transient 429 (InferenceInfra#444). Go in on the stored
            // session; the harness's own calls surface the real error if it
            // is still there.
            out::status_line(&format!(
                "could not confirm the cloud session ({}) - continuing on the stored session",
                err.message
            ));
        }
    }
    // Refresh the catalog for next time without blocking, and reject a
    // mistyped id from the cache. Fail open on an empty cache.
    account::refresh_model_cache_if_stale(account::MODEL_CACHE_TTL_SECONDS);
    if account::cache_has_models() && !account::model_is_cached(model) {
        // Stale cache: refresh live and retry rather than reject a valid
        // model.
        if !refresh_and_recheck_model(&credentials, model) {
            return 1;
        }
    }

    let base_url = format!("{}/v1", credentials.console_url);
    // Every catalog model, so opencode's picker lists them all; the
    // launched one stays the default. Each carries its real window and
    // price so opencode's compaction fires at the right point and its
    // usage shows real spend.
    let catalog = catalog_models_with(
        console,
        &credentials.console_url,
        &credentials.access_token,
        model,
    );
    if catalog[0].context_window > 0 {
        out::status_line(&format!(
            "context window: {} tokens",
            catalog[0].context_window
        ));
    }
    let config =
        build_open_code_cloud_config(model, &base_url, &credentials.access_token, &catalog);
    let mut environment = ScopedOpenCodeConfig::new();
    if !environment.activate(&config) {
        out::error_line("could not set the temporary OpenCode configuration");
        return 1;
    }

    out::status_line("launching OpenCode with the RunAnywhere cloud session");
    spawn("opencode", arguments)
}

/// Launch OpenCode against the signed-in RunAnywhere cloud session.
///
/// Only OPENCODE_CONFIG_CONTENT is changed, only for the duration of the
/// child. No OpenCode or project configuration file is read or written.
/// Starts `opencode` directly (never through a shell).
pub fn launch_open_code_cloud(model: &str, arguments: &[String]) -> i32 {
    let console = ConsoleClient::default();
    let spawn: SpawnFunction = std::sync::Arc::new(|tool: &str, args: &[String]| spawn(tool, args));
    let arguments = with_private_server(arguments, installed_open_code_major_version());
    launch_open_code_cloud_with(model, &arguments, &console, &spawn)
}

#[cfg(all(test, unix))]
mod tests {
    //! `ScopedOpenCodeConfig` must capture a pre-existing
    //! `OPENCODE_CONFIG_CONTENT` with `var_os` (raw bytes, matching C++'s
    //! `std::getenv`), not `var` (which drops a non-UTF-8 value entirely).
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::*;
    use crate::util::env_lock::lock as env_lock;

    fn described(
        id: &str,
        levels: &[&'static str],
        default_level: Option<&'static str>,
        thinking_off: bool,
    ) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            reasoning: Some(crate::account::ReasoningSupport {
                levels: levels.to_vec(),
                default_level,
                thinking_off,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn each_model_gets_its_own_levels_as_variants_and_its_default_as_an_option() {
        let models = [
            described(
                "deepseek-v4.1-flash",
                &["low", "high", "xhigh", "max"],
                Some("high"),
                true,
            ),
            described(
                "muse-spark",
                &["minimal", "low", "medium", "high", "xhigh"],
                Some("medium"),
                false,
            ),
            described("mimo-v2.6-pro", &[], None, true),
            CatalogModel {
                id: "local-model".to_string(),
                ..Default::default()
            },
        ];
        let doc: serde_json::Value = serde_json::from_str(&build_open_code_config(
            "deepseek-v4.1-flash",
            "https://example.test/v1",
            "k",
            &models,
        ))
        .unwrap();
        let entries = &doc["provider"]["runanywhere"]["models"];
        let deepseek = &entries["deepseek-v4.1-flash"];
        assert_eq!(deepseek["reasoning"], json!(true));
        assert_eq!(
            deepseek["variants"],
            json!({
                "low": {"reasoningEffort": "low"},
                "high": {"reasoningEffort": "high"},
                "xhigh": {"reasoningEffort": "xhigh"},
                "max": {"reasoningEffort": "max"},
                "none": {"reasoningEffort": "none"},
            })
        );
        assert_eq!(deepseek["options"], json!({"reasoningEffort": "high"}));
        // Thinking cannot be turned off on muse-spark: no `none`.
        let muse = &entries["muse-spark"];
        assert!(muse["variants"].get("none").is_none());
        assert_eq!(
            muse["variants"]["minimal"],
            json!({"reasoningEffort": "minimal"})
        );
        assert_eq!(muse["options"], json!({"reasoningEffort": "medium"}));
        // On or off only: thinking off is the one choice, and no default is sent.
        let mimo = &entries["mimo-v2.6-pro"];
        assert_eq!(
            mimo["variants"],
            json!({"none": {"reasoningEffort": "none"}})
        );
        assert!(mimo.get("options").is_none());
        // A model the list does not describe is left as it was.
        let local = &entries["local-model"];
        assert!(local.get("variants").is_none());
        assert!(local.get("reasoning").is_none());
        assert!(local.get("options").is_none());
    }

    #[test]
    fn scoped_config_restores_non_utf8_previous_value_on_drop() {
        let _lock = env_lock();
        let real_previous = std::env::var_os(CONFIG_VARIABLE);

        // Not valid UTF-8 (a lone continuation byte), the same kind of value
        // a wrapper script could set via raw bytes.
        let non_utf8 = OsString::from_vec(vec![b'x', 0xFF, 0xFE]);
        // SAFETY: `env_lock()` is held for this whole test body, and no
        // other thread in this process touches the environment while it is.
        unsafe { std::env::set_var(CONFIG_VARIABLE, &non_utf8) };

        {
            let mut guard = ScopedOpenCodeConfig::new();
            assert!(
                guard.had_previous,
                "a pre-existing non-UTF-8 value must still be observed as \"had a previous value\""
            );
            assert!(guard.activate("{}"), "activate should succeed");
        }
        // `guard` just dropped; it must have restored the exact original
        // bytes rather than unsetting the variable.

        let restored = std::env::var_os(CONFIG_VARIABLE);
        assert_eq!(
            restored.as_deref(),
            Some(non_utf8.as_os_str()),
            "the original non-UTF-8 value must be restored byte-for-byte, not dropped"
        );

        // SAFETY: still holding env_lock(); restoring whatever was really
        // there before this test ran.
        unsafe {
            match real_previous {
                Some(value) => std::env::set_var(CONFIG_VARIABLE, value),
                None => std::env::remove_var(CONFIG_VARIABLE),
            }
        }
    }
}
