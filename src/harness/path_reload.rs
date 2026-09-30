//! Finding a harness wally has just installed, in the same terminal.
//!
//! An installer edits the person's shell startup files or the user PATH, and
//! neither reaches a process that is already running, wally included. wally
//! cannot change its parent shell either, but it can ask for the PATH a new
//! terminal would get and use that for the launch it is about to make.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use super::harness::PATH_SEPARATOR;

const MARK: &str = "__WALLY_PATH__";

/// How long a shell or npm gets to answer. A startup file that prompts must
/// not hold the launch up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The name of a shell executable without its directory or extension.
fn shell_name(shell: &str) -> String {
    let file = shell.rsplit(['/', '\\']).next().unwrap_or(shell);
    file.strip_suffix(".exe").unwrap_or(file).to_string()
}

/// The command that prints PATH between two marks, so whatever else a startup
/// file writes to stdout is ignored. None for a shell wally does not know how
/// to ask.
fn probe_script(shell: &str) -> Option<String> {
    match shell_name(shell).as_str() {
        "bash" | "zsh" | "sh" | "dash" | "ksh" | "mksh" | "ash" => {
            Some(format!("printf '{MARK}%s{MARK}' \"$PATH\""))
        }
        // fish keeps PATH as a list, so it is joined by hand.
        "fish" => Some(format!("printf '{MARK}%s{MARK}' (string join : $PATH)")),
        _ => None,
    }
}

/// The text between the two marks, if both are there.
fn parse_probe(output: &str) -> Option<String> {
    let start = output.find(MARK)? + MARK.len();
    let end = start + output[start..].find(MARK)?;
    let path = &output[start..end];
    (!path.is_empty()).then(|| path.to_string())
}

/// `current` followed by the directories of `fresh` it does not have yet.
/// Existing entries keep their order, so nothing already resolving changes.
pub fn merge_path(current: &str, fresh: &str) -> String {
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    let mut merged: Vec<&str> = current.split(PATH_SEPARATOR).collect();
    for dir in fresh.split(PATH_SEPARATOR) {
        if !dir.is_empty() && !merged.iter().any(|have| same(have, dir)) {
            merged.push(dir);
        }
    }
    merged.join(&PATH_SEPARATOR.to_string())
}

/// How long output may keep arriving after the process has exited. Whatever the
/// shell printed is already in the pipe by then; only a leftover background
/// child holding the pipe open can make the reader wait longer.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// Ends the process and everything it started. On Unix it leads its own
/// process group (see `run_for_output`), so one signal reaches a startup
/// file's background jobs too, which would otherwise keep the pipe open.
fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: killpg only signals the group this child leads.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Runs `command`, and returns its stdout if it exits cleanly within the
/// timeout. Stdin is closed and stderr dropped: a shell started without a
/// terminal complains about job control, and none of that is wanted. Output is
/// read on its own thread and collected as it arrives, so a child that outlives
/// the shell with the pipe still open costs at most `DRAIN_GRACE`, not its own
/// lifetime.
fn run_for_output(mut command: Command) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let collected = Arc::new(Mutex::new(Vec::new()));
    let (ended, end_of_output) = mpsc::channel();
    {
        let collected = Arc::clone(&collected);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(read) = stdout.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                if let Ok(mut bytes) = collected.lock() {
                    bytes.extend_from_slice(&chunk[..read]);
                }
            }
            let _ = ended.send(());
        });
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                kill_tree(&mut child);
                break false;
            }
        }
    };
    if !finished {
        return None;
    }
    let grace = DRAIN_GRACE.min(deadline.saturating_duration_since(Instant::now()));
    if end_of_output.recv_timeout(grace).is_err() {
        kill_tree(&mut child);
    }
    let bytes = collected.lock().ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The flag sets to run `shell` with. bash reads `~/.bashrc` only as a
/// non-login interactive shell, and `~/.bash_profile` (which need not source
/// it) only as a login one, so a PATH set in either place needs both runs.
/// The other shells read what they need in the one.
#[cfg(not(windows))]
fn probe_modes(shell: &str) -> &'static [&'static [&'static str]] {
    match shell_name(shell).as_str() {
        "bash" => &[&["-l", "-i", "-c"], &["-i", "-c"]],
        _ => &[&["-l", "-i", "-c"]],
    }
}

/// The PATH `shell` reports in each of its probe modes, merged. `home`
/// replaces HOME for the probe, which is how a test gives it its own
/// startup files.
#[cfg(not(windows))]
fn shell_path(shell: &str, home: Option<&std::path::Path>) -> Option<String> {
    let script = probe_script(shell)?;
    let mut merged: Option<String> = None;
    for flags in probe_modes(shell) {
        let mut command = Command::new(shell);
        command.args(*flags).arg(&script);
        if let Some(home) = home {
            command.env("HOME", home);
        }
        if let Some(path) = run_for_output(command).and_then(|output| parse_probe(&output)) {
            merged = Some(match merged {
                Some(have) => merge_path(&have, &path),
                None => path,
            });
        }
    }
    merged
}

/// The PATH a new terminal would start with, as far as it can be told.
///
/// POSIX: the person's own shell, run as a login shell and an interactive one
/// so it reads the profile files and the rc files an installer might have
/// appended to (bash is also run as a plain interactive shell, see
/// `probe_modes`). Windows: the machine and user PATH from the registry, which
/// is where an installer writes.
pub fn fresh_path() -> Option<String> {
    #[cfg(windows)]
    {
        let mut command = Command::new("powershell");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Environment]::ExpandEnvironmentVariables([Environment]::GetEnvironmentVariable('Path','Machine') + ';' + [Environment]::GetEnvironmentVariable('Path','User'))",
        ]);
        let path = run_for_output(command)?.trim().to_string();
        (!path.is_empty()).then_some(path)
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
        shell_path(&shell, None)
    }
}

/// npm's global bin directory, which is on no PATH until the person puts it
/// there (a custom prefix, a fresh Node install).
pub fn npm_global_bin() -> Option<PathBuf> {
    let mut command = Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" });
    command.args(["config", "get", "prefix"]);
    let output = run_for_output(command)?;
    let prefix = output.trim();
    if prefix.is_empty() {
        return None;
    }
    // Windows keeps the shims in the prefix itself; everywhere else in bin.
    Some(if cfg!(windows) {
        PathBuf::from(prefix)
    } else {
        PathBuf::from(prefix).join("bin")
    })
}

/// What to type to get this terminal to see a new install, or None where
/// there is nothing short of opening a new window.
pub fn reload_hint() -> String {
    if cfg!(windows) {
        return "open a new terminal window".to_string();
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    match shell_name(&shell).as_str() {
        name @ ("bash" | "zsh" | "fish" | "ksh" | "dash" | "sh") => {
            format!("run `exec {name}` or open a new terminal")
        }
        _ => "open a new terminal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_name_drops_the_directory_and_extension() {
        assert_eq!(shell_name("/bin/zsh"), "zsh");
        assert_eq!(shell_name("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_name("C:\\Program Files\\Git\\bin\\bash.exe"), "bash");
    }

    #[test]
    fn posix_shells_print_path_between_marks() {
        for shell in [
            "/bin/bash",
            "/bin/zsh",
            "/bin/sh",
            "/usr/bin/dash",
            "/bin/ksh",
        ] {
            let script = probe_script(shell).unwrap();
            assert!(script.contains("\"$PATH\""), "{shell}: {script}");
            assert!(script.starts_with("printf '__WALLY_PATH__"), "{shell}");
        }
    }

    #[test]
    fn fish_joins_its_path_list() {
        assert!(probe_script("/usr/bin/fish")
            .unwrap()
            .contains("string join : $PATH"));
    }

    #[test]
    fn an_unknown_shell_is_not_probed() {
        assert_eq!(probe_script("/usr/bin/nu"), None);
        assert_eq!(probe_script("/bin/tcsh"), None);
    }

    #[test]
    fn startup_file_noise_around_the_marks_is_ignored() {
        let output = "Welcome back!\n__WALLY_PATH__/a/bin:/b/bin__WALLY_PATH__\nbye\n";
        assert_eq!(parse_probe(output).as_deref(), Some("/a/bin:/b/bin"));
    }

    #[test]
    fn output_without_both_marks_is_rejected() {
        assert_eq!(parse_probe("no marks here"), None);
        assert_eq!(parse_probe("__WALLY_PATH__/a/bin"), None);
        assert_eq!(parse_probe("__WALLY_PATH____WALLY_PATH__"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn merge_adds_only_the_missing_directories_after_the_existing_ones() {
        assert_eq!(
            merge_path("/usr/bin:/bin", "/home/me/.local/bin:/usr/bin:/opt/x/bin"),
            "/usr/bin:/bin:/home/me/.local/bin:/opt/x/bin"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn merge_skips_empty_entries_in_the_fresh_path() {
        assert_eq!(
            merge_path("/usr/bin", "::/opt/x/bin:"),
            "/usr/bin:/opt/x/bin"
        );
    }

    #[cfg(windows)]
    #[test]
    fn merge_compares_windows_directories_without_case() {
        assert_eq!(
            merge_path(
                "C:\\Windows",
                "c:\\windows;C:\\Users\\me\\AppData\\Roaming\\npm"
            ),
            "C:\\Windows;C:\\Users\\me\\AppData\\Roaming\\npm"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_shell_that_prints_a_path_is_read_back() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "printf 'noise__WALLY_PATH__/x/bin:/y/bin__WALLY_PATH__'",
        ]);
        assert_eq!(
            parse_probe(&run_for_output(command).unwrap()).as_deref(),
            Some("/x/bin:/y/bin")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_probe_that_hangs_is_given_up_on() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let started = Instant::now();
        assert_eq!(run_for_output(command), None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_background_child_holding_the_pipe_does_not_hold_the_probe_up() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30 & printf '__WALLY_PATH__/x__WALLY_PATH__'"]);
        let started = Instant::now();
        let output = run_for_output(command).expect("the shell itself exited cleanly");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(parse_probe(&output).as_deref(), Some("/x"));
    }

    #[cfg(not(windows))]
    #[test]
    fn a_slow_probe_is_bounded_and_takes_its_children_with_it() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30; printf '__WALLY_PATH__/x__WALLY_PATH__'"]);
        let started = Instant::now();
        assert_eq!(run_for_output(command), None);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_is_probed_as_a_login_shell_and_as_a_plain_interactive_one() {
        assert_eq!(probe_modes("/bin/bash").len(), 2);
        assert_eq!(probe_modes("/bin/zsh").len(), 1);
        assert_eq!(probe_modes("/usr/bin/fish").len(), 1);
    }

    #[cfg(not(windows))]
    #[test]
    fn bash_finds_a_path_entry_set_only_in_bashrc() {
        let home = std::env::temp_dir().join(format!("wally-path-reload-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // The profile does not source .bashrc, which is the case a login-only
        // probe misses. Each file adds one entry the other cannot see.
        std::fs::write(
            home.join(".bash_profile"),
            "export PATH=\"/only/in/profile:$PATH\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join(".bashrc"),
            "export PATH=\"/only/in/bashrc:$PATH\"\n",
        )
        .unwrap();
        let found = shell_path("bash", Some(&home));
        let _ = std::fs::remove_dir_all(&home);
        let found = found.expect("bash answered");
        let dirs: Vec<&str> = found.split(':').collect();
        assert!(dirs.contains(&"/only/in/bashrc"), "{found}");
        assert!(dirs.contains(&"/only/in/profile"), "{found}");
    }

    #[cfg(not(windows))]
    #[test]
    fn a_failing_probe_returns_nothing() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf out; exit 3"]);
        assert_eq!(run_for_output(command), None);
    }
}
