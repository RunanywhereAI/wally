//! `wally shannon`: Shannon, the open-source AI pentester for authorised
//! security testing, pointed at the models on a RunAnywhere account.
//!
//! Shannon is a Docker and Temporal command-line tool. Its `start` command
//! reads a model from `SHANNON_AI_MODEL=<provider>:<model id>`, the key from
//! `SHANNON_AI_API_KEY`, and any provider its built-in catalogue lacks from a
//! Pi `models.json` passed as `--models-config`. This launcher writes that file
//! from the account's own model list, sets the two variables for the child
//! only, and otherwise gets out of the way: every other Shannon command runs
//! untouched.
//!
//! The provider speaks Chat Completions (`openai-completions`). Shannon's
//! built-in `openai:` provider speaks the Responses API instead, which this
//! endpoint does not serve, so the model is always named `runanywhere:<id>`.
//!
//! The key goes in the child's environment and nowhere else: not in argv, not
//! in `models.json`, not in a message.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::account::{self, ConsoleClient, IdentityResult};
use crate::io::json::dump_pretty;
use crate::io::output as out;

use super::catalog_models::{account_models_with, CatalogModel};
use super::harness::{
    model_id_is_safe, refresh_session, report_cloud_session_invalid, report_not_signed_in,
    verify_cloud_session,
};

/// The Shannon release this launcher was written and checked against. Both the
/// installed command and the `npx` fallback have to be exactly this.
pub const PINNED_VERSION: &str = "3.4.0";

/// The npm package that ships the `shannon` command.
pub const PACKAGE: &str = "@keygraph/shannon";

/// The provider name in `models.json` and in `SHANNON_AI_MODEL`.
pub const PROVIDER_ID: &str = "runanywhere";

/// Where Shannon reads the key for the provider `SHANNON_AI_MODEL` names.
pub const KEY_VARIABLE: &str = "SHANNON_AI_API_KEY";

/// Where Shannon reads `<provider>:<model id>`.
pub const MODEL_VARIABLE: &str = "SHANNON_AI_MODEL";

/// A console Cloud key, for a scan that outlasts a signed-in session. The only
/// way to supply one: a flag would put it in argv and shell history.
pub const CLOUD_KEY_VARIABLE: &str = "WALLY_CLOUD_KEY";

/// Variables that would override the key or the endpoint Shannon is given.
/// Shannon prefers its provider's native key variable over
/// `SHANNON_AI_API_KEY`, and `SHANNON_AI_BASE_URL` sends model traffic, and the
/// key with it, to another host.
const OVERRIDING_VARIABLES: [&str; 4] = [
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "XAI_API_KEY",
    "SHANNON_AI_BASE_URL",
];

/// A session that would end sooner than this is refreshed before the scan
/// starts: a scan runs for hours, and Shannon cannot refresh anything itself.
const REFRESH_WITHIN_SECONDS: i64 = 4 * 60 * 60;

/// How long `shannon version` gets to answer before it is treated as absent.
const VERSION_PROBE_SECONDS: u64 = 20;

pub const WINDOWS_REFUSAL: &str =
    "Shannon runs on Linux, macOS, or Windows through WSL2. Run wally inside WSL2.";

pub const PREREQUISITES: &str = "Shannon needs Node 18 or later (it provides npx) and Docker \
running. Install Node from https://nodejs.org and Docker from https://docs.docker.com/get-docker/, \
then run this again.";

/// What a person is told when `-m` names a model their account does not have.
pub fn not_available_message(model: &str) -> String {
    format!(
        "'{model}' is not available on this account. If it needs approval, request access in \
         the console."
    )
}

// ---------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------

/// What the person asked Shannon to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `start`: Wally supplies the model, the key and `--models-config`.
    /// Holds Shannon's own arguments after `start`.
    Start(Vec<String>),
    /// Anything else, forwarded untouched.
    Passthrough(Vec<String>),
}

fn is_help_or_version_flag(arg: &str) -> bool {
    matches!(arg, "-h" | "--help" | "-v" | "--version")
}

fn names_models_config(arg: &str) -> bool {
    arg == "--models-config" || arg.starts_with("--models-config=")
}

/// Reads Shannon's arguments. No subcommand, or a first argument that is an
/// option, means `start`; a first argument that is Shannon's own help or version
/// flag, or a `start` that asks for its own help, is left alone.
pub fn classify(args: &[String]) -> Result<Invocation, String> {
    let first = args.first().map(String::as_str);
    let start_rest: Vec<String> = match first {
        Some("start") => args[1..].to_vec(),
        Some(flag) if is_help_or_version_flag(flag) => {
            return Ok(Invocation::Passthrough(args.to_vec()))
        }
        Some(flag) if flag.starts_with('-') => args.to_vec(),
        None => Vec::new(),
        Some(_) => return Ok(Invocation::Passthrough(args.to_vec())),
    };
    if start_rest.iter().any(|arg| names_models_config(arg)) {
        return Err(
            "wally shannon writes the --models-config file itself, from your account's models. \
             Remove --models-config, or run Shannon directly to use your own file."
                .to_string(),
        );
    }
    if start_rest
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        let mut argv = vec!["start".to_string()];
        argv.extend(start_rest);
        return Ok(Invocation::Passthrough(argv));
    }
    Ok(Invocation::Start(start_rest))
}

impl Invocation {
    /// The arguments Shannon is run with. `start` gets `--models-config` right
    /// after it, ahead of the person's own options.
    pub fn argv(&self, models_config: &str) -> Vec<String> {
        match self {
            Invocation::Start(rest) => {
                let mut argv = vec![
                    "start".to_string(),
                    "--models-config".to_string(),
                    models_config.to_string(),
                ];
                argv.extend(rest.iter().cloned());
                argv
            }
            Invocation::Passthrough(args) => args.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// The child's environment
// ---------------------------------------------------------------------------

/// The environment changes for the Shannon child. Holds the key, so it has no
/// `Debug`.
pub struct ChildEnv {
    pub set: Vec<(&'static str, String)>,
    pub remove: Vec<&'static str>,
}

impl ChildEnv {
    /// For `start`: the model and the key, and none of the variables that would
    /// override either.
    pub fn for_start(model: &str, key: &str) -> Self {
        ChildEnv {
            set: vec![
                (MODEL_VARIABLE, format!("{PROVIDER_ID}:{model}")),
                (KEY_VARIABLE, key.to_string()),
            ],
            remove: OVERRIDING_VARIABLES.to_vec(),
        }
    }

    /// Everything else runs in the environment it was given.
    pub fn unchanged() -> Self {
        ChildEnv {
            set: Vec::new(),
            remove: Vec::new(),
        }
    }

    fn apply(&self, command: &mut Command) {
        for name in &self.remove {
            command.env_remove(name);
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }
}

// ---------------------------------------------------------------------------
// models.json
// ---------------------------------------------------------------------------

/// A Pi `models.json` naming every model the account can use. It carries no
/// key: Shannon takes that from `SHANNON_AI_API_KEY`.
pub fn models_config(base_url: &str, models: &[CatalogModel]) -> Value {
    let entries: Vec<Value> = models
        .iter()
        .map(|model| {
            let mut input = vec!["text"];
            if model.image_input {
                input.push("image");
            }
            let mut entry = json!({ "id": model.id, "input": input });
            if model.context_window > 0 {
                entry["contextWindow"] = json!(model.context_window);
            }
            if model.max_output > 0 {
                entry["maxTokens"] = json!(model.max_output);
            }
            entry
        })
        .collect();
    json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": base_url,
                "api": "openai-completions",
                "authHeader": true,
                // The gateway takes `system`; a `developer` role would be refused.
                "compat": { "supportsDeveloperRole": false },
                "models": entries,
            }
        }
    })
}

/// Where the file goes: under the wally profile, in a directory of its own
/// (the profile already keeps its model-id cache in a `models.json`).
pub fn models_config_path() -> Option<PathBuf> {
    let profile = account::profile_directory();
    if profile.is_empty() {
        return None;
    }
    Some(PathBuf::from(profile).join("shannon").join("models.json"))
}

/// Replaces `path` with `text`, readable by the owner alone. Written to a
/// sibling at that mode and renamed over the old file, so the contents are
/// never visible at a looser mode and a file left over from an earlier run
/// cannot keep its own.
pub fn write_private_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;

    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("no parent directory"));
    };
    if !parent.is_dir() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    let mut temporary_name = OsString::from(".");
    temporary_name.push(path.file_name().unwrap_or_default());
    temporary_name.push(format!(".{}.tmp", std::process::id()));
    let temporary = parent.join(temporary_name);
    let _ = std::fs::remove_file(&temporary);

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(text.as_bytes())?;
        file.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&temporary, path)) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// Which Shannon runs
// ---------------------------------------------------------------------------

/// The program that runs Shannon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// A `shannon` on PATH that reports the pinned version.
    Installed(PathBuf),
    /// `npx --yes @keygraph/shannon@<pinned>`.
    Npx(PathBuf),
}

impl Runner {
    fn program(&self) -> &Path {
        match self {
            Runner::Installed(path) | Runner::Npx(path) => path,
        }
    }

    /// Arguments that come before Shannon's own.
    fn prefix(&self) -> Vec<String> {
        match self {
            Runner::Installed(_) => Vec::new(),
            Runner::Npx(_) => vec!["--yes".to_string(), format!("{PACKAGE}@{PINNED_VERSION}")],
        }
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The first `name` on `search_path` that can be run.
pub fn find_on_path(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    let names: Vec<String> = if cfg!(windows) {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(search_path) {
        // An empty entry is the working directory to execvp, but a launcher
        // that runs whatever sits in the current directory is not one to copy.
        if dir.as_os_str().is_empty() {
            continue;
        }
        for candidate in &names {
            let path = dir.join(candidate);
            if is_executable(&path) {
                return Some(path);
            }
        }
    }
    None
}

/// The version `shannon version` prints (`shannon 3.4.0`): its last word.
pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.split_whitespace().last())
        .map(str::to_string)
}

/// Runs `<shannon> version`, giving it a bounded time, and returns the version
/// it reports. None if it does not run, fails, or does not answer.
pub fn probe_version(shannon: &Path) -> Option<String> {
    let mut child = Command::new(shannon)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(VERSION_PROBE_SECONDS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut text = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut text).ok()?;
    parse_version(&text)
}

/// Picks the program: a `shannon` on `search_path` whose `version` reports the
/// pinned release, else `npx` at that release. The second value is a note for
/// the person when an installed Shannon was passed over.
pub fn select_runner(
    search_path: &OsStr,
    probe: &dyn Fn(&Path) -> Option<String>,
) -> Result<(Runner, Option<String>), String> {
    let mut note = None;
    if let Some(installed) = find_on_path("shannon", search_path) {
        match probe(&installed) {
            Some(version) if version == PINNED_VERSION => {
                return Ok((Runner::Installed(installed), None));
            }
            Some(version) => {
                note = Some(format!(
                    "the shannon on PATH is {version}, not {PINNED_VERSION}; using npx \
                     {PACKAGE}@{PINNED_VERSION} instead"
                ));
            }
            None => {
                note = Some(format!(
                    "could not read the version of the shannon on PATH; using npx \
                     {PACKAGE}@{PINNED_VERSION} instead"
                ));
            }
        }
    }
    match find_on_path("npx", search_path) {
        Some(npx) => Ok((Runner::Npx(npx), note)),
        None => Err(match note {
            Some(note) => format!("{note}, but npx was not found. {PREREQUISITES}"),
            None => format!("shannon and npx were not found. {PREREQUISITES}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// Runs `runner` with `argv` and `env`, waits, and returns its exit code. A
/// Ctrl-C reaches the child through the terminal; this process waits for it
/// rather than dying first.
pub fn run_child(runner: &Runner, argv: &[String], env: &ChildEnv) -> i32 {
    let interrupted = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&interrupted);
    let _interrupt = crate::util::interrupt::on_interrupt(move || {
        noted.store(true, Ordering::SeqCst);
    });

    let mut command = Command::new(runner.program());
    command.args(runner.prefix()).args(argv);
    env.apply(&mut command);
    let status = match command.status() {
        Ok(status) => status,
        Err(error) => {
            out::error_line(&format!("could not start Shannon: {error}"));
            return 127;
        }
    };
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    if interrupted.load(Ordering::SeqCst) {
        130
    } else {
        1
    }
}

// ---------------------------------------------------------------------------
// The account
// ---------------------------------------------------------------------------

/// Where the models are served from and the key that reads them.
struct Hosted {
    console_url: String,
    key: String,
    /// A signed-in session rather than a Cloud key.
    session: bool,
}

/// `WALLY_CLOUD_KEY`, if it is set to something.
fn cloud_key_from_environment() -> Option<String> {
    let value = std::env::var(CLOUD_KEY_VARIABLE).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// The key to scan with: `WALLY_CLOUD_KEY` if set, else the signed-in session,
/// refreshed first when it would end within four hours. Prints why and returns
/// None when there is neither.
fn hosted_access(console: &ConsoleClient) -> Option<Hosted> {
    let mut credentials = match account::load() {
        Ok(credentials) => credentials,
        Err(message) => {
            out::error_line(&message);
            return None;
        }
    };

    if let Some(key) = cloud_key_from_environment() {
        if !account::session_token_is_safe(&key) {
            out::error_line(&format!(
                "{CLOUD_KEY_VARIABLE} holds characters a key cannot contain"
            ));
            return None;
        }
        return Some(Hosted {
            console_url: credentials.console_url,
            key,
            session: false,
        });
    }

    if !credentials.signed_in() {
        report_not_signed_in();
        out::status_line(&format!(
            "or set {CLOUD_KEY_VARIABLE} to a Cloud key from the console"
        ));
        return None;
    }
    if !credentials.refresh_token.is_empty()
        && credentials.access_token_expired(account::epoch_seconds(), REFRESH_WITHIN_SECONDS)
    {
        if let Err(error) = refresh_session(console, &mut credentials) {
            // The console could not be asked and the token is still good for
            // now: go on with it, as the other tools do.
            let usable =
                error.unverified && !credentials.access_token_expired(account::epoch_seconds(), 60);
            if !usable {
                report_cloud_session_invalid("Shannon");
                return None;
            }
        }
    }
    match verify_cloud_session(console, &mut credentials) {
        Ok(email) => {
            if !email.is_empty() {
                out::status_line(&format!("signed in as {email}"));
            }
        }
        Err(error) if error.unverified => {
            out::status_line(&format!(
                "could not confirm the cloud session ({}) - continuing on the stored session",
                error.message
            ));
        }
        Err(_) => {
            report_cloud_session_invalid("Shannon");
            return None;
        }
    }
    let remaining = credentials.expires_at - account::epoch_seconds();
    if credentials.expires_at > 0 && remaining < REFRESH_WITHIN_SECONDS {
        out::status_line(&format!(
            "this sign-in stays valid for about {} minutes. For a longer scan, create a Cloud \
             key in the console and set {CLOUD_KEY_VARIABLE}.",
            remaining.max(0) / 60
        ));
    }
    Some(Hosted {
        console_url: credentials.console_url,
        key: credentials.access_token,
        session: true,
    })
}

/// The model Shannon is pointed at: `requested` if the account has it, else the
/// preferred default if it has that, else the first model on the list.
pub fn choose_model(
    models: &[CatalogModel],
    requested: Option<&str>,
    preferred_default: &str,
) -> Result<String, String> {
    if let Some(id) = requested {
        return if models.iter().any(|model| model.id == id) {
            Ok(id.to_string())
        } else {
            Err(not_available_message(id))
        };
    }
    if models.iter().any(|model| model.id == preferred_default) {
        return Ok(preferred_default.to_string());
    }
    models
        .first()
        .map(|model| model.id.clone())
        .ok_or_else(|| "your account has no models available for Shannon".to_string())
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// `wally shannon [-m <model>] [--] [<shannon command>] [args...]`.
pub fn launch(requested_model: Option<&str>, args: &[String]) -> i32 {
    if cfg!(windows) {
        out::error_line(WINDOWS_REFUSAL);
        return 1;
    }
    if let Some(id) = requested_model {
        if !model_id_is_safe(id) {
            out::error_line(&format!("'{id}' is not a valid model id"));
            return 2;
        }
    }
    let invocation = match classify(args) {
        Ok(invocation) => invocation,
        Err(message) => {
            out::error_line(&message);
            return 2;
        }
    };

    // First, before any account call: a person without the tools should see
    // only that they are missing.
    let search_path = std::env::var_os("PATH").unwrap_or_default();
    let (runner, note) = match select_runner(&search_path, &probe_version) {
        Ok(selected) => selected,
        Err(message) => {
            out::error_line(&message);
            return 127;
        }
    };
    if let Some(note) = note {
        out::status_line(&note);
    }

    let Invocation::Start(_) = &invocation else {
        if requested_model.is_some() {
            out::status_line("-m only applies to `start`; passing the command through unchanged");
        }
        return run_child(&runner, &invocation.argv(""), &ChildEnv::unchanged());
    };

    let console = ConsoleClient::default();
    let Some(hosted) = hosted_access(&console) else {
        return 1;
    };
    let (result, mut models, error) =
        account_models_with(&console, &hosted.console_url, &hosted.key);
    match result {
        IdentityResult::Ok => {}
        IdentityResult::Unauthorized if hosted.session => {
            report_cloud_session_invalid("Shannon");
            return 1;
        }
        IdentityResult::Unauthorized => {
            out::error_line(&format!(
                "the console did not accept {CLOUD_KEY_VARIABLE}; check the key in the console"
            ));
            return 1;
        }
        _ => {
            out::error_line(&format!("could not read your model list: {error}"));
            return 1;
        }
    }
    // Decision models score text; they cannot run an agent.
    models.retain(|model| !super::is_decisions_model(&model.id));

    let preferred = crate::config::preferences::resolve_model("");
    let model = match choose_model(&models, requested_model, &preferred) {
        Ok(model) => model,
        Err(message) => {
            out::error_line(&message);
            return 1;
        }
    };
    if requested_model.is_none() {
        out::status_line(&format!("model not provided, using default model {model}"));
    }

    let Some(path) = models_config_path() else {
        out::error_line("no user profile directory is available");
        return 1;
    };
    let base_url = format!("{}/v1", hosted.console_url);
    let text = dump_pretty(&models_config(&base_url, &models), 2) + "\n";
    if let Err(error) = write_private_file(&path, &text) {
        out::error_line(&format!("could not write {}: {error}", path.display()));
        return 1;
    }

    out::status_line(&format!("Shannon will use {model} through {base_url}"));
    run_child(
        &runner,
        &invocation.argv(&path.to_string_lossy()),
        &ChildEnv::for_start(&model, &hosted.key),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    fn model(id: &str, context: i64, output: i64, image: bool) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            context_window: context,
            max_output: output,
            image_input: image,
            ..Default::default()
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wally-shannon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    // --- argv -------------------------------------------------------------

    #[test]
    fn no_command_means_start_with_the_models_config_first() {
        let invocation = classify(&strings(&["-u", "https://app.test", "-r", "./repo"])).unwrap();
        assert_eq!(
            invocation.argv("/p/models.json"),
            strings(&[
                "start",
                "--models-config",
                "/p/models.json",
                "-u",
                "https://app.test",
                "-r",
                "./repo"
            ])
        );
        assert!(matches!(invocation, Invocation::Start(_)));
    }

    #[test]
    fn an_explicit_start_is_not_doubled() {
        let invocation = classify(&strings(&["start", "-u", "https://app.test"])).unwrap();
        assert_eq!(
            invocation.argv("/p/models.json"),
            strings(&[
                "start",
                "--models-config",
                "/p/models.json",
                "-u",
                "https://app.test"
            ])
        );
    }

    #[test]
    fn a_bare_invocation_is_start_too() {
        assert_eq!(classify(&[]).unwrap(), Invocation::Start(Vec::new()));
    }

    #[test]
    fn a_models_config_of_the_users_is_refused_in_either_spelling() {
        for args in [
            strings(&["start", "-u", "x", "--models-config", "mine.json"]),
            strings(&["-u", "x", "--models-config=mine.json"]),
        ] {
            let message = classify(&args).unwrap_err();
            assert!(message.contains("--models-config"), "{message}");
        }
    }

    #[test]
    fn other_commands_pass_through_unchanged() {
        for args in [
            strings(&["version"]),
            strings(&["stop", "q1-audit", "--yes"]),
            strings(&["logs", "q1-audit", "--models-config", "x"]),
            strings(&["status", "--json"]),
            strings(&["scans"]),
        ] {
            let invocation = classify(&args).unwrap();
            assert_eq!(invocation, Invocation::Passthrough(args.clone()));
            assert_eq!(invocation.argv("/p/models.json"), args);
        }
    }

    #[test]
    fn shannons_own_help_and_version_flags_are_left_alone() {
        for args in [
            strings(&["--help"]),
            strings(&["-h"]),
            strings(&["--version"]),
            strings(&["start", "--help"]),
        ] {
            assert!(
                matches!(classify(&args).unwrap(), Invocation::Passthrough(_)),
                "{args:?}"
            );
        }
        assert_eq!(
            classify(&strings(&["start", "-h"])).unwrap().argv("/x"),
            strings(&["start", "-h"])
        );
    }

    // --- env --------------------------------------------------------------

    #[test]
    fn the_key_is_in_the_environment_and_nowhere_in_argv() {
        let key = "ra-secret-key-123";
        let invocation = classify(&strings(&["-u", "https://app.test", "-r", "./repo"])).unwrap();
        let argv = invocation.argv("/p/models.json");
        assert!(argv.iter().all(|arg| !arg.contains(key)));

        let env = ChildEnv::for_start("glm-5.3-flash", key);
        assert!(env.set.contains(&(KEY_VARIABLE, key.to_string())));
        assert!(env
            .set
            .contains(&(MODEL_VARIABLE, "runanywhere:glm-5.3-flash".to_string())));
        assert!(
            !models_config("https://api.test/v1", &[model("a", 1, 1, false)])
                .to_string()
                .contains(key)
        );
    }

    #[test]
    fn start_removes_the_variables_that_would_override_the_key() {
        let env = ChildEnv::for_start("m", "k");
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "XAI_API_KEY",
            "SHANNON_AI_BASE_URL",
        ] {
            assert!(env.remove.contains(&name), "{name}");
        }
        let unchanged = ChildEnv::unchanged();
        assert!(unchanged.set.is_empty() && unchanged.remove.is_empty());
    }

    #[test]
    fn the_child_sees_the_key_and_not_the_stray_ones() {
        let _guard = crate::util::env_lock::lock();
        let dir = scratch("env");
        let record = dir.join("record");
        let shannon = stub(
            &dir,
            "shannon",
            &format!(
                "echo \"$@\" > {r}\n\
                 echo \"SHANNON_AI_API_KEY=$SHANNON_AI_API_KEY\" >> {r}\n\
                 echo \"SHANNON_AI_MODEL=$SHANNON_AI_MODEL\" >> {r}\n\
                 echo \"OPENAI=${{OPENAI_API_KEY-unset}}\" >> {r}\n\
                 echo \"ANTHROPIC=${{ANTHROPIC_API_KEY-unset}}\" >> {r}\n\
                 echo \"BASE=${{SHANNON_AI_BASE_URL-unset}}\" >> {r}\n\
                 exit 7",
                r = record.display()
            ),
        );
        // SAFETY: serialised by the env lock; restored below.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "stray-openai");
            std::env::set_var("ANTHROPIC_API_KEY", "stray-anthropic");
            std::env::set_var("SHANNON_AI_BASE_URL", "https://elsewhere.test");
        }
        let code = run_child(
            &Runner::Installed(shannon),
            &strings(&["start", "-u", "https://app.test"]),
            &ChildEnv::for_start("glm-5.3-flash", "the-key"),
        );
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("SHANNON_AI_BASE_URL");
        }
        assert_eq!(code, 7);
        let seen = std::fs::read_to_string(&record).unwrap();
        assert!(seen.contains("start -u https://app.test"), "{seen}");
        assert!(seen.contains("SHANNON_AI_API_KEY=the-key"), "{seen}");
        assert!(
            seen.contains("SHANNON_AI_MODEL=runanywhere:glm-5.3-flash"),
            "{seen}"
        );
        assert!(seen.contains("OPENAI=unset"), "{seen}");
        assert!(seen.contains("ANTHROPIC=unset"), "{seen}");
        assert!(seen.contains("BASE=unset"), "{seen}");
        assert!(!seen.contains("stray"), "{seen}");
    }

    // --- models.json ------------------------------------------------------

    #[test]
    fn models_json_lists_every_model_with_its_real_limits() {
        let document = models_config(
            "https://api.test/v1",
            &[
                model("glm-5.3-flash", 202752, 131072, false),
                model("vision-model", 131072, 0, true),
                model("unknown-limits", 0, 0, false),
            ],
        );
        let provider = &document["providers"][PROVIDER_ID];
        assert_eq!(provider["baseUrl"], "https://api.test/v1");
        assert_eq!(provider["api"], "openai-completions");
        assert_eq!(provider["authHeader"], true);
        assert!(provider.get("apiKey").is_none());
        let models = provider["models"].as_array().unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(
            models[0],
            json!({"id": "glm-5.3-flash", "input": ["text"], "contextWindow": 202752, "maxTokens": 131072})
        );
        assert_eq!(
            models[1],
            json!({"id": "vision-model", "input": ["text", "image"], "contextWindow": 131072})
        );
        assert_eq!(
            models[2],
            json!({"id": "unknown-limits", "input": ["text"]})
        );
    }

    #[test]
    fn models_json_is_owner_only_and_replaces_an_earlier_file() {
        let dir = scratch("perm");
        let path = dir.join("shannon").join("models.json");
        write_private_file(&path, "first").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        // A file an earlier version or a person left at a looser mode.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private_file(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
    }

    // --- which Shannon ----------------------------------------------------

    #[test]
    fn version_output_is_read_from_its_last_word() {
        assert_eq!(parse_version("shannon 3.4.0\n").as_deref(), Some("3.4.0"));
        assert_eq!(
            parse_version("\n\nshannon 3.3.0"),
            Some("3.3.0".to_string())
        );
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn an_installed_shannon_at_the_pinned_version_is_run_directly() {
        let bin = scratch("installed");
        let shannon = stub(&bin, "shannon", &format!("echo 'shannon {PINNED_VERSION}'"));
        stub(&bin, "npx", "exit 0");
        let (runner, note) = select_runner(bin.as_os_str(), &probe_version).unwrap();
        assert_eq!(runner, Runner::Installed(shannon));
        assert_eq!(note, None);
        assert!(runner.prefix().is_empty());
    }

    #[test]
    fn a_different_installed_version_falls_back_to_npx_at_the_pin() {
        let bin = scratch("mismatch");
        stub(&bin, "shannon", "echo 'shannon 3.3.0'");
        let npx = stub(&bin, "npx", "exit 0");
        let (runner, note) = select_runner(bin.as_os_str(), &probe_version).unwrap();
        assert_eq!(runner, Runner::Npx(npx));
        assert_eq!(
            runner.prefix(),
            strings(&["--yes", &format!("@keygraph/shannon@{PINNED_VERSION}")])
        );
        let note = note.unwrap();
        assert!(
            note.contains("3.3.0") && note.contains(PINNED_VERSION),
            "{note}"
        );
    }

    #[test]
    fn a_shannon_that_will_not_say_its_version_is_not_trusted() {
        let bin = scratch("broken");
        stub(&bin, "shannon", "exit 3");
        stub(&bin, "npx", "exit 0");
        let (runner, note) = select_runner(bin.as_os_str(), &probe_version).unwrap();
        assert!(matches!(runner, Runner::Npx(_)));
        assert!(note.is_some());
    }

    #[test]
    fn without_shannon_npx_runs_the_pinned_package() {
        let bin = scratch("npx-only");
        let npx = stub(&bin, "npx", "exit 0");
        let (runner, note) = select_runner(bin.as_os_str(), &probe_version).unwrap();
        assert_eq!(runner, Runner::Npx(npx));
        assert_eq!(note, None);
    }

    #[test]
    fn with_neither_the_prerequisites_are_named() {
        let bin = scratch("empty");
        let message = select_runner(bin.as_os_str(), &probe_version).unwrap_err();
        assert!(message.contains("Node 18"), "{message}");
        assert!(message.contains("Docker"), "{message}");
    }

    #[test]
    fn a_mismatched_shannon_with_no_npx_says_both() {
        let bin = scratch("mismatch-no-npx");
        stub(&bin, "shannon", "echo 'shannon 9.9.9'");
        let message = select_runner(bin.as_os_str(), &probe_version).unwrap_err();
        assert!(
            message.contains("9.9.9") && message.contains("Node 18"),
            "{message}"
        );
    }

    #[test]
    fn a_file_that_is_not_executable_is_not_found() {
        let bin = scratch("not-exec");
        std::fs::write(bin.join("shannon"), "#!/bin/sh\n").unwrap();
        assert_eq!(find_on_path("shannon", bin.as_os_str()), None);
    }

    #[test]
    fn npx_is_run_with_yes_and_the_pinned_package_ahead_of_shannons_arguments() {
        let bin = scratch("npx-run");
        let record = bin.join("record");
        let npx = stub(&bin, "npx", &format!("echo \"$@\" > {}", record.display()));
        let code = run_child(
            &Runner::Npx(npx),
            &strings(&["version"]),
            &ChildEnv::unchanged(),
        );
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(&record).unwrap().trim(),
            format!("--yes @keygraph/shannon@{PINNED_VERSION} version")
        );
    }

    // --- the account's models ---------------------------------------------

    #[test]
    fn a_model_the_account_does_not_have_gets_the_approval_message() {
        let models = [model("glm-5.3-flash", 1, 1, false)];
        assert_eq!(
            choose_model(&models, Some("gated-model"), "glm-5.3-flash").unwrap_err(),
            "'gated-model' is not available on this account. If it needs approval, request \
             access in the console."
        );
        assert_eq!(
            choose_model(&models, Some("glm-5.3-flash"), "x").unwrap(),
            "glm-5.3-flash"
        );
    }

    #[test]
    fn without_m_the_default_is_used_when_the_account_has_it_else_the_first() {
        let models = [model("a", 1, 1, false), model("b", 1, 1, false)];
        assert_eq!(choose_model(&models, None, "b").unwrap(), "b");
        assert_eq!(choose_model(&models, None, "zzz").unwrap(), "a");
        assert!(choose_model(&[], None, "b").is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn native_windows_is_refused_with_the_wsl_message() {
        assert_eq!(launch(None, &[]), 1);
        assert_eq!(
            WINDOWS_REFUSAL,
            "Shannon runs on Linux, macOS, or Windows through WSL2. Run wally inside WSL2."
        );
    }
}
