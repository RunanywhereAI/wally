//! `wally vvah`: Visa Vulnerability Agentic Harness against account models.
//!
//! VVAH is a Python tool. This launcher installs a pinned release into the
//! Wally profile when asked, writes a `config.local.yaml` of OpenAI-compatible
//! roles, and sets `OPENAI_BASE_URL` / `OPENAI_API_KEY` for the child only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::account::{self, ConsoleClient};
use crate::io::output as out;

use super::harness::confirm;
use super::hosted::{self, CLOUD_KEY_VARIABLE};

/// The VVAH release this launcher installs and runs.
pub const PINNED_VERSION: &str = "1.5.1";

pub const MISSING_PYTHON: &str =
    "python 3.11 is not installed on this machine, Please install it first, Thank You";

pub const SOURCE: &str =
    "git+https://github.com/visa/visa-vulnerability-agentic-harness.git@v1.5.1";

pub const S10_WARNING: &str =
    "VVAH's remediate step can edit files in the target repo. Scan only code you are authorised to test.";

const MIN_PYTHON: (u32, u32) = (3, 11);
/// Inclusive. `arcadedb-embedded==26.10.1` (a VVAH pin) ships wheels for
/// 3.10–3.14 only, so 3.15's pip reports "from versions: none".
const MAX_PYTHON: (u32, u32) = (3, 14);

const PYTHON_CANDIDATES: &[&str] = &[
    "python3.12",
    "python3.11",
    "python3.13",
    "python3.14",
    "python3",
];

const KEY_VARIABLE: &str = "OPENAI_API_KEY";
const BASE_URL_VARIABLE: &str = "OPENAI_BASE_URL";

const ROUTE_COMMANDS: [&str; 3] = ["scan", "remediate", "doctor"];

const OVERRIDING_VARIABLES: [&str; 4] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_SDK_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    CLOUD_KEY_VARIABLE,
];

// ---------------------------------------------------------------------------
// The command line
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Routed { command: String, rest: Vec<String> },
    Passthrough(Vec<String>),
}

fn is_help_or_version_flag(arg: &str) -> bool {
    matches!(arg, "-h" | "--help" | "-v" | "--version")
}

fn names_config(arg: &str) -> bool {
    arg == "--config" || arg.starts_with("--config=")
}

fn config_error() -> String {
    "wally vvah writes the --config file itself, from your account's model. Remove --config, \
     or run VVAH directly to use your own file."
        .to_string()
}

/// No subcommand means `scan --repo . --remediate`. `scan`/`remediate`/`doctor`
/// get the Wally config; everything else is left alone.
pub fn classify(args: &[String]) -> Result<Invocation, String> {
    let first = args.first().map(String::as_str);
    let (command, rest): (String, Vec<String>) = match first {
        None => (
            "scan".to_string(),
            vec![
                "--repo".to_string(),
                ".".to_string(),
                "--remediate".to_string(),
            ],
        ),
        Some(flag) if is_help_or_version_flag(flag) => {
            return Ok(Invocation::Passthrough(args.to_vec()))
        }
        Some(flag) if flag.starts_with('-') => ("scan".to_string(), args.to_vec()),
        Some(command) if ROUTE_COMMANDS.contains(&command) => {
            (command.to_string(), args[1..].to_vec())
        }
        Some(_) => return Ok(Invocation::Passthrough(args.to_vec())),
    };
    if rest.iter().any(|arg| names_config(arg)) {
        return Err(config_error());
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

impl Invocation {
    pub fn argv(&self, config: &str) -> Vec<String> {
        match self {
            Invocation::Routed { command, rest } => {
                let mut argv = vec![command.clone(), "--config".to_string(), config.to_string()];
                argv.extend(rest.iter().cloned());
                argv
            }
            Invocation::Passthrough(args) => args.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

fn role_line(id: &str, model: &str) -> String {
    format!("  {id}: {{id: {model}, via: openai}}")
}

/// OpenAI-compatible overlay for every active VVAH role. No key.
pub fn config_yaml(model: &str) -> String {
    let mut out = String::from("models:\n");
    for id in [
        "autoexclude",
        "preprocess",
        "threatmodel",
        "decompose",
        "deepdive",
        "verify",
        "dedup",
        "chain",
        "remediate",
    ] {
        out.push_str(&role_line(id, model));
        out.push('\n');
    }
    out.push_str("  validate:\n");
    for id in [
        "orchestrator",
        "security_architect",
        "penetration_tester",
        "cross_repo_analyzer",
    ] {
        out.push_str(&format!("    {id}: {{id: {model}, via: openai}}\n"));
    }
    out.push_str("  exploit_verification:\n");
    for id in ["attacker", "judge", "classify", "mapper"] {
        out.push_str(&format!("    {id}: {{id: {model}, via: openai}}\n"));
    }
    out.push_str(
        "step1:\n  auto_exclude_effort: none\n\
         step2:\n  max_turns: 60\n\
         step4:\n  max_turns: 60\n\
         step6_verify:\n  max_turns: 60\n\
         step_remediate:\n  enabled: true\n\
         step_validate:\n  enabled: true\n",
    );
    out
}

pub fn config_path() -> Option<PathBuf> {
    let profile = account::profile_directory();
    if profile.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(profile)
            .join("vvah")
            .join("config.local.yaml"),
    )
}

pub fn repo_from_rest(rest: &[String]) -> &str {
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--repo" {
            return rest.get(i + 1).map(String::as_str).unwrap_or(".");
        }
        if let Some(value) = rest[i].strip_prefix("--repo=") {
            return value;
        }
        i += 1;
    }
    "."
}

fn existing_or_parent(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path)
        .ok()
        .or_else(|| path.parent().and_then(|parent| parent.canonicalize().ok()))
}

/// True when `path` is `root` or a file under it (VVAH's CONFIG_POLICY_REFUSED).
pub fn path_is_inside(path: &Path, root: &Path) -> bool {
    let Some(path) = existing_or_parent(path) else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    path == root || path.starts_with(&root)
}

/// Profile config sits under `$HOME/.config/wally`. A default scan of `.` from
/// home is that tree, so VVAH refuses the file. Put the config directory in
/// temp instead. `--config` must be `config.yaml`; VVAH then loads sibling
/// `config.local.yaml` as the overlay.
pub fn config_dir_for_target(
    preferred_dir: PathBuf,
    cwd: &Path,
    repo: &str,
) -> Result<PathBuf, String> {
    let root = {
        let repo = Path::new(repo);
        if repo.is_absolute() {
            repo.to_path_buf()
        } else {
            cwd.join(repo)
        }
    };
    if !path_is_inside(&preferred_dir, &root) {
        return Ok(preferred_dir);
    }
    let fallback = std::env::temp_dir().join("wally-vvah");
    if path_is_inside(&fallback, &root) {
        return Err(
            "the scan target contains Wally's VVAH config path; pass --repo at a project directory"
                .to_string(),
        );
    }
    Ok(fallback)
}

pub fn packaged_default_yaml(vvaharness: &Path) -> Option<PathBuf> {
    let venv = vvaharness.parent()?.parent()?;
    let relative = Path::new("vvaharness")
        .join("config")
        .join("profiles")
        .join("default.yaml");
    let windows = venv.join("Lib").join("site-packages").join(&relative);
    if windows.is_file() {
        return Some(windows);
    }
    let lib = venv.join("lib");
    let entries = std::fs::read_dir(&lib).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("site-packages").join(&relative);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn stage_vvah_config(dir: &Path, program: &Path, model: &str) -> Result<PathBuf, String> {
    if std::fs::create_dir_all(dir).is_err() {
        return Err(format!("could not create {}", dir.display()));
    }
    let overlay = dir.join("config.local.yaml");
    write_private_file(&overlay, &config_yaml(model))
        .map_err(|error| format!("could not write {}: {error}", overlay.display()))?;
    let primary = dir.join("config.yaml");
    match packaged_default_yaml(program) {
        Some(src) => {
            std::fs::copy(&src, &primary)
                .map_err(|error| format!("could not copy VVAH default.yaml: {error}"))?;
            Ok(primary)
        }
        None => Ok(overlay),
    }
}

pub fn refuse_home_workspace(args: &[String], cwd: &Path, home: Option<&Path>) -> bool {
    if !args.is_empty() {
        return false;
    }
    let Some(home) = home else {
        return false;
    };
    if cwd == home {
        return true;
    }
    match (std::fs::canonicalize(cwd), std::fs::canonicalize(home)) {
        (Ok(cwd), Ok(home)) => cwd == home,
        _ => false,
    }
}

pub fn write_private_file(path: &Path, text: &str) -> std::io::Result<()> {
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
    let mut temporary_name = std::ffi::OsString::from(".");
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
// Python / install
// ---------------------------------------------------------------------------

pub fn parse_python_version(output: &str) -> Option<(u32, u32)> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let version = line.split_whitespace().last()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

pub fn python_is_new_enough(major: u32, minor: u32) -> bool {
    (major, minor) >= MIN_PYTHON
}

pub fn python_is_supported(major: u32, minor: u32) -> bool {
    python_is_new_enough(major, minor) && (major, minor) <= MAX_PYTHON
}

pub fn too_new_python_message(major: u32, minor: u32) -> String {
    format!(
        "VVAH needs Python 3.11–3.14 (python3 is {major}.{minor}). Install Python 3.12, then run this again."
    )
}

pub fn install_dir() -> Option<PathBuf> {
    let profile = account::profile_directory();
    if profile.is_empty() {
        return None;
    }
    Some(PathBuf::from(profile).join("vvah"))
}

fn venv_bin() -> &'static str {
    if cfg!(windows) {
        "Scripts"
    } else {
        "bin"
    }
}

pub fn venv_vvaharness(dir: &Path) -> PathBuf {
    let mut name = dir.join("venv").join(venv_bin()).join("vvaharness");
    if cfg!(windows) {
        name.set_extension("exe");
    }
    name
}

pub fn install_command(dir: &Path, python: &Path) -> String {
    let venv = dir.join("venv");
    let pip = venv
        .join(venv_bin())
        .join(if cfg!(windows) { "pip.exe" } else { "pip" });
    format!(
        "{} -m venv {} && {} install {SOURCE}",
        python.display(),
        venv.display(),
        pip.display()
    )
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

pub fn probe_python_at(program: &Path) -> Option<(u32, u32)> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_python_version(&String::from_utf8_lossy(&output.stdout))
        .or_else(|| parse_python_version(&String::from_utf8_lossy(&output.stderr)))
}

pub fn probe_python() -> Option<(u32, u32)> {
    probe_python_at(Path::new("python3"))
}

/// First interpreter on PATH that VVAH's pinned ArcadeDB wheel will install on.
pub fn find_supported_python() -> Option<(PathBuf, u32, u32)> {
    for name in PYTHON_CANDIDATES {
        let Some(path) = find_on_path(name) else {
            continue;
        };
        let Some((major, minor)) = probe_python_at(&path) else {
            continue;
        };
        if python_is_supported(major, minor) {
            return Some((path, major, minor));
        }
    }
    None
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

fn find_on_path(name: &str) -> Option<PathBuf> {
    let search_path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{name}.exe"), name.to_string()]
    } else {
        vec![name.to_string()]
    };
    for dir in std::env::split_paths(&search_path) {
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

/// A `vvaharness` we can run: the profile venv first, then PATH.
pub fn resolve_vvaharness() -> Option<PathBuf> {
    if let Some(dir) = install_dir() {
        let path = venv_vvaharness(&dir);
        if is_executable(&path) {
            return Some(path);
        }
    }
    find_on_path("vvaharness")
}

fn run_status(mut command: Command) -> bool {
    retry_busy(|| command.status())
        .ok()
        .is_some_and(|status| status.success())
}

pub fn install_vvah(dir: &Path, python: &Path) -> Result<(), String> {
    if std::fs::create_dir_all(dir).is_err() {
        return Err(format!("could not create {}", dir.display()));
    }
    let venv = dir.join("venv");
    if !run_status({
        let mut command = Command::new(python);
        command.args(["-m", "venv", "--clear"]).arg(&venv);
        command
    }) {
        return Err("could not create a Python virtual environment for VVAH".to_string());
    }
    let pip = venv
        .join(venv_bin())
        .join(if cfg!(windows) { "pip.exe" } else { "pip" });
    if !run_status({
        let mut command = Command::new(&pip);
        command.args(["install", SOURCE]);
        command
    }) {
        return Err("could not install VVAH into the virtual environment".to_string());
    }
    if !is_executable(&venv_vvaharness(dir)) {
        return Err("VVAH installed but wally could not find vvaharness in the venv".to_string());
    }
    Ok(())
}

fn ensure_installed() -> Option<PathBuf> {
    if let Some(path) = resolve_vvaharness() {
        return Some(path);
    }
    let Some(dir) = install_dir() else {
        out::error_line("no user profile directory is available");
        return None;
    };
    let python = find_supported_python().map(|(path, _, _)| path);
    let shown = python.as_deref().unwrap_or(Path::new("python3"));
    let command = install_command(&dir, shown);
    out::error_line("vvaharness is not installed on this machine");
    out::status_line(&format!("install it with `{command}`"));
    if !confirm(&format!("Install it now with `{command}`? [y/N] ")) {
        return None;
    }
    let Some(python) = python.or_else(|| find_supported_python().map(|(path, _, _)| path)) else {
        match probe_python() {
            Some((major, minor)) if (major, minor) > MAX_PYTHON => {
                out::error_line(&too_new_python_message(major, minor));
            }
            Some(_) | None => out::error_line(MISSING_PYTHON),
        }
        return None;
    };
    if let Err(error) = install_vvah(&dir, &python) {
        out::error_line(&error);
        return None;
    }
    Some(venv_vvaharness(&dir))
}

// ---------------------------------------------------------------------------
// Child env and run
// ---------------------------------------------------------------------------

pub struct ChildEnv {
    pub set: Vec<(&'static str, String)>,
    pub remove: Vec<&'static str>,
}

impl ChildEnv {
    pub fn for_start(key: &str, base_url: &str) -> Self {
        ChildEnv {
            set: vec![
                (KEY_VARIABLE, key.to_string()),
                (BASE_URL_VARIABLE, base_url.to_string()),
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

pub fn run_child(program: &Path, argv: &[String], env: &ChildEnv) -> i32 {
    let interrupted = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&interrupted);
    let _interrupt = crate::util::interrupt::on_interrupt(move || {
        noted.store(true, Ordering::SeqCst);
    });

    let mut command = Command::new(program);
    command.args(argv);
    env.apply(&mut command);
    let status = match retry_busy(|| command.status()) {
        Ok(status) => status,
        Err(error) => {
            out::error_line(&format!("could not start VVAH: {error}"));
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

/// `wally vvah [-m <model>] [--] [<vvah command>] [args...]`.
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
            "wally vvah with no extra args scans the current directory. \
             That is your home directory. cd into a repo you are authorised to test, \
             or pass --repo after --.",
        );
        return 2;
    }
    let Some(program) = ensure_installed() else {
        return 127;
    };

    let Invocation::Routed { .. } = &invocation else {
        if requested_model.is_some() {
            out::status_line("-m only applies to scan, remediate and doctor; passing the command through unchanged");
        }
        return run_child(&program, &invocation.argv(""), &ChildEnv::unchanged());
    };

    let console = ConsoleClient::default();
    let Some(hosted) = hosted::hosted_access(&console, "VVAH") else {
        return 1;
    };
    let (model, base_url) = match hosted::pick_model(&console, &hosted, requested_model) {
        Ok(chosen) => chosen,
        Err(code) => return code,
    };

    let Some(preferred_dir) = install_dir() else {
        out::error_line("no user profile directory is available");
        return 1;
    };
    let repo = match &invocation {
        Invocation::Routed { rest, .. } => repo_from_rest(rest),
        Invocation::Passthrough(_) => ".",
    };
    let dir = match config_dir_for_target(preferred_dir, &cwd, repo) {
        Ok(dir) => dir,
        Err(message) => {
            out::error_line(&message);
            return 2;
        }
    };
    let path = match stage_vvah_config(&dir, &program, &model) {
        Ok(path) => path,
        Err(error) => {
            out::error_line(&error);
            return 1;
        }
    };

    out::status_line(S10_WARNING);
    out::status_line(&format!("VVAH will use {model} through {base_url}"));
    run_child(
        &program,
        &invocation.argv(&path.to_string_lossy()),
        &ChildEnv::for_start(&hosted.key, &base_url),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use crate::util::env_lock::lock as env_lock;

    fn strings(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn pinned_version_and_missing_python_message() {
        assert_eq!(PINNED_VERSION, "1.5.1");
        assert_eq!(
            MISSING_PYTHON,
            "python 3.11 is not installed on this machine, Please install it first, Thank You"
        );
    }

    #[test]
    fn repo_from_rest_reads_flag() {
        assert_eq!(
            repo_from_rest(&strings(&["--repo", ".", "--remediate"])),
            "."
        );
        assert_eq!(repo_from_rest(&strings(&["--repo=./x"])), "./x");
        assert_eq!(repo_from_rest(&[]), ".");
    }

    #[test]
    fn config_dir_for_target_leaves_profile_when_repo_is_elsewhere() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().join("proj");
        let profile = dir.path().join("profile");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&profile).unwrap();
        let chosen = config_dir_for_target(profile.clone(), &cwd, ".").unwrap();
        assert_eq!(chosen, profile);
    }

    #[test]
    fn config_dir_for_target_moves_out_when_repo_contains_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let profile = cwd.join(".config").join("wally");
        std::fs::create_dir_all(&profile).unwrap();
        let chosen = config_dir_for_target(profile, cwd, ".").unwrap();
        assert!(!path_is_inside(&chosen, cwd));
    }

    #[test]
    fn packaged_default_yaml_walks_venv_lib() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("venv").join("bin");
        let yaml = dir
            .path()
            .join("venv")
            .join("lib")
            .join("python3.12")
            .join("site-packages")
            .join("vvaharness")
            .join("config")
            .join("profiles")
            .join("default.yaml");
        std::fs::create_dir_all(yaml.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(&yaml, "models: {}\n").unwrap();
        let program = bin.join("vvaharness");
        std::fs::write(&program, "").unwrap();
        assert_eq!(
            packaged_default_yaml(&program).as_deref(),
            Some(yaml.as_path())
        );
    }

    #[test]
    fn refuse_home_workspace_only_for_bare_scan() {
        let home = Path::new("/Users/me");
        assert!(refuse_home_workspace(&[], home, Some(home)));
        assert!(!refuse_home_workspace(
            &strings(&["scan", "--repo", "./x"]),
            home,
            Some(home)
        ));
    }

    #[test]
    fn classify_default_is_routed_scan_with_remediate() {
        assert_eq!(
            classify(&[]).unwrap(),
            Invocation::Routed {
                command: "scan".to_string(),
                rest: strings(&["--repo", ".", "--remediate"]),
            }
        );
    }

    #[test]
    fn classify_help_passes_through_and_scan_is_routed() {
        let help = strings(&["--help"]);
        assert_eq!(classify(&help).unwrap(), Invocation::Passthrough(help));
        assert_eq!(
            classify(&strings(&["scan", "--repo", "./x"])).unwrap(),
            Invocation::Routed {
                command: "scan".to_string(),
                rest: strings(&["--repo", "./x"]),
            }
        );
        assert_eq!(
            classify(&strings(&["report"])).unwrap(),
            Invocation::Passthrough(strings(&["report"]))
        );
    }

    #[test]
    fn classify_refuses_user_config_on_routed_commands() {
        assert!(classify(&strings(&["scan", "--config", "mine.yaml"])).is_err());
        assert!(classify(&strings(&["doctor", "--config=mine.yaml"])).is_err());
    }

    #[test]
    fn python_version_parse_and_floor() {
        assert_eq!(parse_python_version("Python 3.11.9\n"), Some((3, 11)));
        assert!(python_is_new_enough(3, 11));
        assert!(python_is_new_enough(3, 12));
        assert!(!python_is_new_enough(3, 10));
        assert!(python_is_supported(3, 11));
        assert!(python_is_supported(3, 14));
        assert!(!python_is_supported(3, 10));
        assert!(!python_is_supported(3, 15));
        assert!(too_new_python_message(3, 15).contains("3.11–3.14"));
    }

    #[test]
    fn config_yaml_maps_roles_without_a_key() {
        let yaml = config_yaml("glm-5.3-flash");
        assert!(yaml.contains("autoexclude: {id: glm-5.3-flash, via: openai}"));
        assert!(yaml.contains("remediate: {id: glm-5.3-flash, via: openai}"));
        assert!(yaml.contains("    orchestrator: {id: glm-5.3-flash, via: openai}"));
        assert!(yaml.contains("max_turns: 60"));
        assert!(yaml.contains("auto_exclude_effort: none"));
        assert!(!yaml.contains("inject:"));
        assert!(!yaml.to_lowercase().contains("api_key"));
        assert!(!yaml.contains("sk-"));
    }

    #[test]
    fn install_command_shows_venv_and_pip_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let command = install_command(dir.path(), Path::new("python3.12"));
        assert!(command.contains("python3.12 -m venv"));
        assert!(command.contains("pip"));
        assert!(command.contains(SOURCE));
    }

    #[test]
    fn argv_routed_injects_config_before_rest() {
        let routed = Invocation::Routed {
            command: "scan".to_string(),
            rest: strings(&["--repo", "."]),
        };
        assert_eq!(
            routed.argv("/tmp/cfg.yaml"),
            strings(&["scan", "--config", "/tmp/cfg.yaml", "--repo", "."])
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_private_file_is_owner_only_and_contains_no_key() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.local.yaml");
        let yaml = config_yaml("glm-5.3-flash");
        write_private_file(&path, &yaml).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, yaml);
        assert!(!text.to_lowercase().contains("api_key"));
        assert!(!text.contains("sk-"));
    }

    #[test]
    fn child_env_sets_openai_vars_only_on_child() {
        let env = ChildEnv::for_start("key", "https://api.example/v1");
        assert_eq!(
            env.set,
            vec![
                ("OPENAI_API_KEY", "key".to_string()),
                ("OPENAI_BASE_URL", "https://api.example/v1".to_string()),
            ]
        );
        assert!(env.remove.contains(&CLOUD_KEY_VARIABLE));
    }

    #[cfg(unix)]
    #[test]
    fn run_child_applies_openai_env_on_child_only() {
        use std::os::unix::fs::PermissionsExt;

        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("vvah-stub");
        fs::write(
            &stub,
            "#!/bin/sh\nif test \"$OPENAI_API_KEY\" = child && test \"$OPENAI_BASE_URL\" = https://x/v1 && test -z \"$WALLY_CLOUD_KEY\"; then exit 43; fi\nexit 9\n",
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();

        let previous = std::env::var("WALLY_CLOUD_KEY").ok();
        std::env::set_var("WALLY_CLOUD_KEY", "parent");
        let code = run_child(
            &stub,
            &strings(&["scan", "--config", "c.yaml"]),
            &ChildEnv::for_start("child", "https://x/v1"),
        );
        let remaining = std::env::var("WALLY_CLOUD_KEY");
        match previous {
            Some(value) => std::env::set_var("WALLY_CLOUD_KEY", value),
            None => std::env::remove_var("WALLY_CLOUD_KEY"),
        }
        assert_eq!(code, 43);
        assert_eq!(remaining.as_deref(), Ok("parent"));
    }
}
