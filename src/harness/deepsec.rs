//! `wally deepsec`: DeepSec pointed at the models on a RunAnywhere account.
//!
//! DeepSec's custom OpenAI-compatible route is the Pi backend. This launcher
//! injects that route, sets the key for the child only, and otherwise gets
//! out of the way.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::account::{self, ConsoleClient};
use crate::io::output as out;

use super::harness::confirm;
use super::hosted::{self, CLOUD_KEY_VARIABLE};

/// The DeepSec release this launcher was written against.
pub const PINNED_VERSION: &str = "2.3.10";

/// The npm package that ships the `deepsec` command.
pub const PACKAGE: &str = "deepsec";

/// The provider name passed as `--ai-provider`.
pub const PROVIDER_ID: &str = "runanywhere";

/// Where DeepSec reads the key. Only the name is stored in its config.
pub const KEY_VARIABLE: &str = "WALLY_DEEPSEC_API_KEY";

/// Pi reads `models.json` from this directory. Isolated under the Wally profile
/// so we do not rewrite `~/.pi/agent`.
pub const PI_AGENT_DIR_VARIABLE: &str = "PI_CODING_AGENT_DIR";

pub const PREREQUISITES: &str =
    "DeepSec needs Node 18 or later (it provides npx). Install Node from https://nodejs.org, \
then run this again.";

pub fn npx_invoke() -> String {
    format!("npx --yes {PACKAGE}@{PINNED_VERSION}")
}

/// Bare `wally deepsec` runs DeepSec `init` in the current directory. Home is
/// not a repo to scan: DeepSec will treat it as the workspace (and may walk
/// `~/.Trash`).
pub fn refuse_home_workspace(args: &[String], cwd: &Path, home: Option<&Path>) -> bool {
    if !args.is_empty() {
        return false;
    }
    let Some(home) = home else {
        return false;
    };
    paths_are_same_dir(cwd, home)
}

fn paths_are_same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

const VERSION_PROBE_SECONDS: u64 = 20;

const ROUTE_COMMANDS: [&str; 4] = ["init", "setup", "process", "revalidate"];

const OWNED_FLAGS: [&str; 3] = ["--model-auth", "--ai-base-url", "--ai-api-key-env"];

const OVERRIDING_VARIABLES: [&str; 4] = [
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "AI_GATEWAY_API_KEY",
    CLOUD_KEY_VARIABLE,
];

// ---------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------

/// What the person asked DeepSec to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// A command that takes Wally's Pi custom route.
    Routed { command: String, rest: Vec<String> },
    /// Anything else, forwarded untouched.
    Passthrough(Vec<String>),
}

fn is_help_or_version_flag(arg: &str) -> bool {
    matches!(arg, "-h" | "--help" | "-v" | "--version")
}

fn names_owned_flag(arg: &str) -> bool {
    OWNED_FLAGS
        .iter()
        .any(|flag| arg == *flag || arg.starts_with(&format!("{flag}=")))
}

fn route_error() -> String {
    "wally deepsec sets the model route itself, from your account. Remove --model-auth, \
     --ai-base-url and --ai-api-key-env, or run DeepSec directly to use your own route."
        .to_string()
}

/// Reads DeepSec's arguments. No subcommand, or a first argument that is an
/// option, means `init`; `setup`/`process`/`revalidate` also get the route.
pub fn classify(args: &[String]) -> Result<Invocation, String> {
    let first = args.first().map(String::as_str);
    let (command, rest): (String, Vec<String>) = match first {
        None => ("init".to_string(), Vec::new()),
        Some(flag) if is_help_or_version_flag(flag) => {
            return Ok(Invocation::Passthrough(args.to_vec()))
        }
        Some(flag) if flag.starts_with('-') => ("init".to_string(), args.to_vec()),
        Some(command) if ROUTE_COMMANDS.contains(&command) => {
            (command.to_string(), args[1..].to_vec())
        }
        Some(_) => return Ok(Invocation::Passthrough(args.to_vec())),
    };
    if rest.iter().any(|arg| names_owned_flag(arg)) {
        return Err(route_error());
    }
    if rest
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        let mut argv = vec![command];
        argv.extend(rest);
        return Ok(Invocation::Passthrough(argv));
    }
    Ok(Invocation::Routed { command, rest })
}

/// Pi requires `provider/model` and looks that pair up in `models.json`.
/// Custom OpenAI-compatible routes are registered as `runanywhere/<id>`.
pub fn pi_model_id(model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        format!("{PROVIDER_ID}/{model}")
    }
}

pub fn models_json_model_id(model: &str) -> &str {
    model.rsplit_once('/').map(|(_, id)| id).unwrap_or(model)
}

/// Pi's registry for the hosted provider. No key; auth is the child env var.
pub fn models_json(base_url: &str, model: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": base_url,
                "api": "openai-completions",
                "authHeader": true,
                "compat": { "supportsDeveloperRole": false },
                "models": [{
                    "id": models_json_model_id(model),
                    "name": models_json_model_id(model),
                    "input": ["text"]
                }]
            }
        }
    }))
    .expect("models.json is a closed object")
}

pub fn pi_agent_directory() -> Option<PathBuf> {
    let profile = account::profile_directory();
    if profile.is_empty() {
        return None;
    }
    Some(PathBuf::from(profile).join("deepsec").join("agent"))
}

pub fn write_pi_models_json(dir: &Path, base_url: &str, model: &str) -> Result<PathBuf, String> {
    if std::fs::create_dir_all(dir).is_err() {
        return Err(format!("could not create {}", dir.display()));
    }
    let path = dir.join("models.json");
    std::fs::write(&path, models_json(base_url, model))
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(path)
}

/// The Pi custom-route flags, then the person's own options.
pub fn route_flags(model: &str, base_url: &str) -> Vec<String> {
    vec![
        "--agent".to_string(),
        "pi".to_string(),
        "--model".to_string(),
        pi_model_id(model),
        "--model-auth".to_string(),
        "custom".to_string(),
        "--ai-provider".to_string(),
        PROVIDER_ID.to_string(),
        "--ai-base-url".to_string(),
        base_url.to_string(),
        "--ai-api-key-env".to_string(),
        KEY_VARIABLE.to_string(),
        "--ai-credential-header".to_string(),
        "authorization:bearer".to_string(),
    ]
}

impl Invocation {
    pub fn argv(&self, model: &str, base_url: &str) -> Vec<String> {
        match self {
            Invocation::Routed { command, rest } => {
                let mut argv = vec![command.clone()];
                argv.extend(route_flags(model, base_url));
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

/// The environment changes for the DeepSec child. Holds the key, so it has no
/// `Debug`.
pub struct ChildEnv {
    pub set: Vec<(&'static str, String)>,
    pub remove: Vec<&'static str>,
}

impl ChildEnv {
    pub fn for_start(key: &str, agent_dir: &Path) -> Self {
        ChildEnv {
            set: vec![
                (KEY_VARIABLE, key.to_string()),
                (
                    PI_AGENT_DIR_VARIABLE,
                    agent_dir.to_string_lossy().into_owned(),
                ),
            ],
            remove: OVERRIDING_VARIABLES.to_vec(),
        }
    }

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
// Which DeepSec runs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    Installed(PathBuf),
    Npx(PathBuf),
}

impl Runner {
    fn program(&self) -> &Path {
        match self {
            Runner::Installed(path) | Runner::Npx(path) => path,
        }
    }

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

pub fn parse_version(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| {
            line.split_whitespace().find_map(|word| {
                let word = word.strip_prefix('v').unwrap_or(word);
                word.chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
                    .then(|| word.to_string())
            })
        })
}

fn retry_busy<T>(mut start: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempts = 0;
    loop {
        match start() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 40 =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

pub fn probe_version(deepsec: &Path) -> Option<String> {
    let mut child = retry_busy(|| {
        Command::new(deepsec)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    })
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

pub fn select_runner(
    search_path: &OsStr,
    probe: &dyn Fn(&Path) -> Option<String>,
) -> Result<(Runner, Option<String>), String> {
    let mut note = None;
    if let Some(installed) = find_on_path("deepsec", search_path) {
        match probe(&installed) {
            Some(version)
                if version == PINNED_VERSION
                    || version.strip_prefix('v') == Some(PINNED_VERSION) =>
            {
                return Ok((Runner::Installed(installed), None));
            }
            Some(version) => {
                note = Some(format!(
                    "the deepsec on PATH is {version}, not {PINNED_VERSION}; using npx \
                     {PACKAGE}@{PINNED_VERSION} instead"
                ));
            }
            None => {
                note = Some(format!(
                    "could not read the version of the deepsec on PATH; using npx \
                     {PACKAGE}@{PINNED_VERSION} instead"
                ));
            }
        }
    }
    match find_on_path("npx", search_path) {
        Some(npx) => Ok((Runner::Npx(npx), note)),
        None => Err(match note {
            Some(note) => format!("{note}, but npx was not found. {PREREQUISITES}"),
            None => format!("deepsec and npx were not found. {PREREQUISITES}"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

pub fn run_child(runner: &Runner, argv: &[String], env: &ChildEnv) -> i32 {
    let interrupted = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&interrupted);
    let _interrupt = crate::util::interrupt::on_interrupt(move || {
        noted.store(true, Ordering::SeqCst);
    });

    let mut command = Command::new(runner.program());
    command.args(runner.prefix()).args(argv);
    env.apply(&mut command);
    let status = match retry_busy(|| command.status()) {
        Ok(status) => status,
        Err(error) => {
            out::error_line(&format!("could not start DeepSec: {error}"));
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
// The command
// ---------------------------------------------------------------------------

/// `wally deepsec [-m <model>] [--] [<deepsec command>] [args...]`.
pub fn launch(requested_model: Option<&str>, args: &[String]) -> i32 {
    if let Err(code) = hosted::require_safe_model(requested_model) {
        return code;
    }
    let invocation = match classify(args) {
        Ok(invocation) => invocation,
        Err(message) => {
            out::error_line(&message);
            return 2;
        }
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if refuse_home_workspace(args, &cwd, home.as_deref()) {
        out::error_line(
            "wally deepsec with no extra args runs DeepSec init in the current directory. \
             That is your home directory, which DeepSec would treat as the workspace. \
             cd into a repo you are authorised to test.",
        );
        return 2;
    }

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
    if matches!(runner, Runner::Npx(_)) {
        let command = npx_invoke();
        out::error_line("deepsec is not installed on this machine");
        out::status_line(&format!("run it with `{command}`"));
        out::status_line(
            "DeepSec init also links a Vercel project for Sandbox identity; models still use your RunAnywhere account.",
        );
        if !confirm(&format!("Run it now with `{command}`? [y/N] ")) {
            return 127;
        }
    }

    let Invocation::Routed { .. } = &invocation else {
        if requested_model.is_some() {
            out::status_line("-m only applies to init, setup, process and revalidate; passing the command through unchanged");
        }
        return run_child(&runner, &invocation.argv("", ""), &ChildEnv::unchanged());
    };

    let console = ConsoleClient::default();
    let Some(hosted) = hosted::hosted_access(&console, "DeepSec") else {
        return 1;
    };
    let (model, base_url) = match hosted::pick_model(&console, &hosted, requested_model) {
        Ok(chosen) => chosen,
        Err(code) => return code,
    };

    out::status_line(&format!("DeepSec will use {model} through {base_url}"));
    let Some(agent_dir) = pi_agent_directory() else {
        out::error_line("no user profile directory is available");
        return 1;
    };
    if let Err(message) = write_pi_models_json(&agent_dir, &base_url, &model) {
        out::error_line(&message);
        return 1;
    }
    run_child(
        &runner,
        &invocation.argv(&model, &base_url),
        &ChildEnv::for_start(&hosted.key, &agent_dir),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::fs;

    use crate::util::env_lock::lock as env_lock;

    fn strings(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn pinned_package_and_provider_constants() {
        assert_eq!(PINNED_VERSION, "2.3.10");
        assert_eq!(PACKAGE, "deepsec");
        assert_eq!(PROVIDER_ID, "runanywhere");
        assert_eq!(KEY_VARIABLE, "WALLY_DEEPSEC_API_KEY");
        assert_eq!(npx_invoke(), "npx --yes deepsec@2.3.10");
    }

    #[test]
    fn refuse_home_workspace_only_for_bare_init() {
        let home = Path::new("/Users/me");
        assert!(refuse_home_workspace(&[], home, Some(home)));
        assert!(!refuse_home_workspace(
            &strings(&["init", "--repo", "./x"]),
            home,
            Some(home)
        ));
        assert!(!refuse_home_workspace(
            &[],
            Path::new("/Users/me/src"),
            Some(home)
        ));
    }

    #[test]
    fn classify_empty_or_option_starts_routed_init() {
        assert_eq!(
            classify(&[]).unwrap(),
            Invocation::Routed {
                command: "init".to_string(),
                rest: vec![],
            }
        );
        assert_eq!(
            classify(&strings(&["--repo", "./x"])).unwrap(),
            Invocation::Routed {
                command: "init".to_string(),
                rest: strings(&["--repo", "./x"]),
            }
        );
    }

    #[test]
    fn classify_help_and_version_pass_through() {
        for flag in ["-h", "--help", "-v", "--version"] {
            let args = strings(&[flag]);
            assert_eq!(classify(&args).unwrap(), Invocation::Passthrough(args));
        }
    }

    #[test]
    fn classify_routed_subcommands_and_scan_passthrough() {
        assert_eq!(
            classify(&strings(&["process", "--all"])).unwrap(),
            Invocation::Routed {
                command: "process".to_string(),
                rest: strings(&["--all"]),
            }
        );
        assert_eq!(
            classify(&strings(&["status"])).unwrap(),
            Invocation::Passthrough(strings(&["status"]))
        );
    }

    #[test]
    fn classify_refuses_owned_route_flags_on_routed_commands() {
        for flag in ["--model-auth", "--ai-base-url", "--ai-api-key-env"] {
            assert!(classify(&strings(&["init", flag])).is_err());
            assert!(classify(&strings(&["revalidate", &format!("{flag}=x")])).is_err());
        }
    }

    #[test]
    fn classify_help_on_routed_command_passes_through() {
        assert_eq!(
            classify(&strings(&["init", "--help"])).unwrap(),
            Invocation::Passthrough(strings(&["init", "--help"]))
        );
    }

    #[test]
    fn pi_model_id_prefixes_runanywhere_when_bare() {
        assert_eq!(pi_model_id("glm-5.3-flash"), "runanywhere/glm-5.3-flash");
        assert_eq!(pi_model_id("zai/glm-5.2"), "zai/glm-5.2");
    }

    #[test]
    fn route_flags_inject_pi_custom_route_before_rest() {
        assert_eq!(
            route_flags("glm-5.3-flash", "https://api.example/v1"),
            strings(&[
                "--agent",
                "pi",
                "--model",
                "runanywhere/glm-5.3-flash",
                "--model-auth",
                "custom",
                "--ai-provider",
                "runanywhere",
                "--ai-base-url",
                "https://api.example/v1",
                "--ai-api-key-env",
                "WALLY_DEEPSEC_API_KEY",
                "--ai-credential-header",
                "authorization:bearer",
            ])
        );
        let routed = Invocation::Routed {
            command: "init".to_string(),
            rest: strings(&["--repo", "."]),
        };
        assert_eq!(
            routed.argv("m", "https://x/v1"),
            strings(&[
                "init",
                "--agent",
                "pi",
                "--model",
                "runanywhere/m",
                "--model-auth",
                "custom",
                "--ai-provider",
                "runanywhere",
                "--ai-base-url",
                "https://x/v1",
                "--ai-api-key-env",
                "WALLY_DEEPSEC_API_KEY",
                "--ai-credential-header",
                "authorization:bearer",
                "--repo",
                ".",
            ])
        );
    }

    #[test]
    fn models_json_registers_runanywhere_provider_without_a_key() {
        let text = models_json("https://api.example/v1", "glm-5.3-flash");
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed["providers"]["runanywhere"]["baseUrl"],
            "https://api.example/v1"
        );
        assert_eq!(
            parsed["providers"]["runanywhere"]["api"],
            "openai-completions"
        );
        assert_eq!(
            parsed["providers"]["runanywhere"]["models"][0]["id"],
            "glm-5.3-flash"
        );
        assert!(!text.contains("apiKey"));
        assert!(!text.contains("sk-"));
        assert_eq!(models_json_model_id("runanywhere/m"), "m");
    }

    #[test]
    fn child_env_for_start_sets_key_and_strips_overrides() {
        let env = ChildEnv::for_start("secret", Path::new("/tmp/wally-deepsec-agent"));
        assert_eq!(
            env.set,
            vec![
                (KEY_VARIABLE, "secret".to_string()),
                (
                    PI_AGENT_DIR_VARIABLE,
                    "/tmp/wally-deepsec-agent".to_string()
                ),
            ]
        );
        assert_eq!(
            env.remove,
            vec![
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "AI_GATEWAY_API_KEY",
                CLOUD_KEY_VARIABLE,
            ]
        );
        assert!(ChildEnv::unchanged().set.is_empty());
        assert!(ChildEnv::unchanged().remove.is_empty());
    }

    #[test]
    fn parse_version_reads_first_numeric_token() {
        assert_eq!(
            parse_version("deepsec 2.3.10\n"),
            Some("2.3.10".to_string())
        );
        assert_eq!(parse_version("version 2.3.10"), Some("2.3.10".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn select_runner_prefers_pinned_install() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let deepsec = dir.path().join("deepsec");
        fs::write(&deepsec, "#!/bin/sh\necho deepsec 2.3.10\n").unwrap();
        fs::set_permissions(&deepsec, fs::Permissions::from_mode(0o700)).unwrap();
        let (runner, note) = select_runner(dir.path().as_os_str(), &|_| {
            Some(PINNED_VERSION.to_string())
        })
        .unwrap();
        assert!(note.is_none());
        assert_eq!(runner, Runner::Installed(deepsec));
    }

    #[test]
    fn select_runner_without_deepsec_or_npx_names_node() {
        let err = select_runner(OsStr::new("/empty-wally-path"), &|_| None).unwrap_err();
        assert!(err.contains("npx were not found"));
        assert!(err.contains("Node 18"));
    }

    #[cfg(unix)]
    #[test]
    fn run_child_applies_env_only_on_the_child() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("deepsec-stub");
        fs::write(
            &stub,
            "#!/bin/sh\nif test -n \"$WALLY_DEEPSEC_API_KEY\" && test -z \"$OPENAI_API_KEY\"; then exit 42; fi\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();

        let previous = std::env::var("OPENAI_API_KEY").ok();
        std::env::set_var("OPENAI_API_KEY", "parent");
        let code = run_child(
            &Runner::Installed(stub),
            &strings(&["status"]),
            &ChildEnv::for_start("child-key", Path::new("/tmp/wally-deepsec-agent")),
        );
        let remaining = std::env::var("OPENAI_API_KEY");
        match previous {
            Some(value) => std::env::set_var("OPENAI_API_KEY", value),
            None => std::env::remove_var("OPENAI_API_KEY"),
        }
        assert_eq!(code, 42);
        assert_eq!(remaining.as_deref(), Ok("parent"));
    }
}
