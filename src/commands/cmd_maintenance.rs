//! Port of src/commands/cmd_maintenance.cpp.
//!
//! `wally help` (a plain-word mirror of `--help`) and `wally uninstall` (remove
//! wally, its on-device models, and its config -- never the coding tools a
//! person installed themselves).

use std::cell::RefCell;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::account::credentials;
use crate::cli::{App, ValueType};
use crate::cli_formatter::color_output_enabled;
use crate::config::cli_paths;
use crate::io::output as out;
use crate::util::{getenv, term};

// The path of the running wally binary, symlinks resolved. Empty when the
// platform gives no answer; uninstall then just skips deleting the binary.
// `pub(crate)`: `wally update` also needs it, to tell a Homebrew-managed
// binary apart from one install.sh or install.ps1 put down.
#[cfg(target_os = "macos")]
pub(crate) fn self_executable() -> String {
    extern "C" {
        fn _NSGetExecutablePath(buf: *mut std::os::raw::c_char, bufsize: *mut u32) -> i32;
    }
    let mut raw = vec![0u8; libc::PATH_MAX as usize];
    let mut size = raw.len() as u32;
    // SAFETY: `raw` is a valid, writable buffer of `size` bytes for the call.
    let ok = unsafe {
        _NSGetExecutablePath(raw.as_mut_ptr() as *mut std::os::raw::c_char, &mut size) == 0
    };
    if !ok {
        return String::new();
    }
    // SAFETY: on success _NSGetExecutablePath NUL-terminates `raw`.
    let raw_path = unsafe { std::ffi::CStr::from_ptr(raw.as_ptr() as *const std::os::raw::c_char) }
        .to_string_lossy()
        .into_owned();
    let Ok(c_raw) = std::ffi::CString::new(raw_path.clone()) else {
        return raw_path;
    };
    let mut resolved = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `resolved` has room for PATH_MAX bytes (realpath's documented
    // maximum output here); `c_raw` is a NUL-terminated input path.
    let resolved_ptr = unsafe {
        libc::realpath(
            c_raw.as_ptr(),
            resolved.as_mut_ptr() as *mut std::os::raw::c_char,
        )
    };
    if resolved_ptr.is_null() {
        return raw_path;
    }
    // SAFETY: realpath NUL-terminates `resolved` on success.
    unsafe { std::ffi::CStr::from_ptr(resolved.as_ptr() as *const std::os::raw::c_char) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(target_os = "linux")]
pub(crate) fn self_executable() -> String {
    std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// `std::env::current_exe()` wraps `GetModuleFileNameW`, giving the exact
// Unicode path with no ANSI round trip -- unlike the C++ build, which had no
// Windows branch here at all (self_executable() there returns {} on every
// platform but macOS and Linux) and so never found its own binary to remove
// or to warn about on `wally uninstall`.
#[cfg(windows)]
pub(crate) fn self_executable() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(crate) fn self_executable() -> String {
    String::new()
}

// The documented default RunAnywhere base for this platform. Needs no rac_init,
// so uninstall can find the model store even though it never boots the kit.
fn platform_base() -> String {
    let Some(home) = getenv("HOME") else {
        return String::new();
    };
    #[cfg(target_os = "macos")]
    {
        format!("{home}/Library/Application Support/RunAnywhere")
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Some(xdg) = getenv("XDG_DATA_HOME") {
            format!("{xdg}/RunAnywhere")
        } else {
            format!("{home}/.local/share/RunAnywhere")
        }
    }
}

// The on-device model store, matching harness/local_models.rs: {base}/Models
// or {base}/RunAnywhere/Models. Every base it can derive is checked -- the
// global --home override, the env override, the kit's answer, and the
// platform default -- and the first store that actually exists wins, so a
// missing bootstrap cannot hide it.
fn models_directory(home_override: &str) -> String {
    let mut bases: Vec<String> = Vec::new();
    if let Some(env) = getenv("RUNANYWHERE_HOME") {
        bases.push(env);
    }
    let home = cli_paths::resolve_home(home_override);
    if !home.is_empty() {
        bases.push(home);
    }
    let fallback = platform_base();
    if !fallback.is_empty() {
        bases.push(fallback);
    }

    for base in &bases {
        for sub in ["RunAnywhere/Models", "Models"] {
            let candidate = Path::new(base).join(sub);
            if candidate.is_dir() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    bases
        .first()
        .map(|b| Path::new(b).join("Models").to_string_lossy().into_owned())
        .unwrap_or_default()
}

// Best-effort recursive size, directory symlinks not followed while walking
// (avoids a symlink cycle; the C++ recursive_directory_iterator default has
// the same effect for directory symlinks). C++'s is_regular_file(ec) /
// file_size(ec) call status(), which DOES follow a symlink to a regular file,
// so a symlinked file's target size counts here too — as in `models state`
// and `rm`.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            total += dir_size(&entry.path());
        } else if file_type.is_file() {
            total += std::fs::metadata(entry.path())
                .map(|m| m.len())
                .unwrap_or(0);
        } else if file_type.is_symlink() {
            if let Ok(target_metadata) = std::fs::metadata(entry.path()) {
                if target_metadata.is_file() {
                    total += target_metadata.len();
                }
            }
        }
    }
    total
}

fn human_size(target: &Path) -> String {
    if !target.exists() {
        return String::new();
    }
    let bytes = if target.is_dir() {
        dir_size(target)
    } else if target.is_file() {
        std::fs::metadata(target).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    let units = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < 3 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

/// Strips the "(os error N)" suffix Rust's `io::Error` Display always
/// appends after the OS message for an OS-sourced error, so the printed text
/// matches C++'s `std::error_code::message()`, which is the OS message alone
/// (e.g. "Permission denied"), with no error-number suffix. Only a trailing
/// " (os error <digits>)" is removed; a non-OS io::Error's Display (which
/// never has that exact suffix) passes through unchanged.
fn os_error_message(error: &std::io::Error) -> String {
    let full = error.to_string();
    if let Some(open) = full.rfind(" (os error ") {
        let tail = &full[open + " (os error ".len()..];
        if let Some(digits) = tail.strip_suffix(')') {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                return full[..open].to_string();
            }
        }
    }
    full
}

fn confirm(question: &str) -> bool {
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.as_bytes().first(), Some(b'y') | Some(b'Y'))
}

#[derive(Debug)]
struct Target {
    label: &'static str,
    path: PathBuf,
}

fn remove_all(path: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

// The whole tree install.sh put down: LIB_DIR (`~/.local/lib/wally`) plus its
// launcher symlink at `~/.local/bin/wally`. `None` for a Homebrew install, a
// from-source build, or anything else uninstall must leave alone -- matching
// `windows_install_directory` below for install.ps1's layout.
#[cfg(not(windows))]
fn install_sh_layout(exe: &str) -> Option<(PathBuf, PathBuf)> {
    if exe.is_empty() {
        return None;
    }
    let home = getenv("HOME")?;
    if home.is_empty() {
        return None;
    }
    let lib_dir = Path::new(&home).join(".local").join("lib").join("wally");
    let launcher = Path::new(&home).join(".local").join("bin").join("wally");
    // Resolved the same way `exe` already was (self_executable() realpaths
    // it), so a HOME whose own path has a symlinked component cannot defeat
    // the prefix check on either side.
    let canonical_lib_dir = std::fs::canonicalize(&lib_dir).unwrap_or(lib_dir);
    Path::new(exe)
        .starts_with(&canonical_lib_dir)
        .then_some((canonical_lib_dir, launcher))
}

// `launcher` only when it is really install.sh's own symlink into
// `lib_dir` -- never a same-named file or symlink somebody else put on
// ~/.local/bin, and never a dangling link uninstall cannot verify.
#[cfg(not(windows))]
fn launcher_target(launcher: &Path, lib_dir: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(launcher).ok()?;
    if !metadata.file_type().is_symlink() {
        return None;
    }
    let resolved = std::fs::canonicalize(launcher).ok()?;
    resolved
        .starts_with(lib_dir)
        .then(|| launcher.to_path_buf())
}

// The whole tree install.ps1 put down (`%LOCALAPPDATA%\Programs\wally`),
// when `exe` lives inside it -- the Windows analogue of `install_sh_layout`.
// `None` for a dev build placed anywhere else.
#[cfg(windows)]
fn windows_install_directory(exe: &str) -> Option<PathBuf> {
    if exe.is_empty() {
        return None;
    }
    let local_app_data = getenv("LOCALAPPDATA")?;
    if local_app_data.is_empty() {
        return None;
    }
    let expected = Path::new(&local_app_data).join("Programs").join("wally");
    // Both sides canonical: on Windows `canonicalize` returns the verbatim
    // `\\?\C:\...` form and `current_exe()` does not, so comparing one of
    // each never matched and uninstall tried to delete its own running exe.
    // The plain path is what the person is told to delete.
    let canonical_exe = std::fs::canonicalize(exe).unwrap_or_else(|_| PathBuf::from(exe));
    let canonical_expected = std::fs::canonicalize(&expected).ok()?;
    canonical_exe
        .starts_with(&canonical_expected)
        .then_some(expected)
}

// The exe-related targets uninstall may delete: only install.sh's own tree
// (verified by `install_sh_layout`), never a Homebrew install or a source
// build -- uninstall cannot prove it laid either of those down, so it must
// only report them (see the call site), never delete. A pure function so the
// decision is unit-testable without touching the real filesystem outside a
// fixture.
#[cfg(not(windows))]
fn unix_exe_targets(exe: &str) -> Vec<Target> {
    let mut targets = Vec::new();
    let Some((lib_dir, launcher)) = install_sh_layout(exe) else {
        return targets;
    };
    if let Some(link) = launcher_target(&launcher, &lib_dir) {
        targets.push(Target {
            label: "launcher",
            path: link,
        });
    }
    targets.push(Target {
        label: "install",
        path: lib_dir,
    });
    targets
}

/// Shared by `wally uninstall` and the whole-argv `-U/--uninstall` shortcut.
/// `home_override` is the global `--home` flag, so uninstall removes the same
/// model store the other model commands are pointed at, not just the default
/// one.
pub fn run_uninstall(yes: bool, home_override: &str) -> i32 {
    let mut targets: Vec<Target> = Vec::new();
    // Windows cannot delete the program file backing its own running
    // process (unlike a Unix inode, which stays live under an unlinked
    // name until the process exits) -- see the note below, where this is
    // reported instead of attempted.
    #[cfg(windows)]
    let mut manual_removal: Option<PathBuf> = None;

    let models = models_directory(home_override);
    if !models.is_empty() {
        targets.push(Target {
            label: "models",
            path: PathBuf::from(models),
        });
    }
    let config = credentials::profile_directory();
    if !config.is_empty() {
        targets.push(Target {
            label: "config",
            path: PathBuf::from(config),
        });
    }

    let exe = self_executable();
    if !exe.is_empty() {
        // A running exe cannot be deleted on Windows. One that install.ps1
        // put down is named for the person to remove; any other (a source
        // build) is left alone, as the C++ build always did there.
        #[cfg(windows)]
        {
            if let Some(dir) = windows_install_directory(&exe) {
                manual_removal = Some(dir);
            }
        }
        #[cfg(not(windows))]
        {
            targets.extend(unix_exe_targets(&exe));
            // Only install.sh's own tree is ever deleted (unix_exe_targets
            // above); a Homebrew install or an unverified (e.g. source-build)
            // location is reported instead, the same way the Windows branch
            // only reports manual_removal rather than deleting an unverified
            // binary.
            if install_sh_layout(&exe).is_none() {
                if crate::commands::cmd_update::is_homebrew_managed(&exe) {
                    out::status_line(
                        "wally was installed with Homebrew; run `brew uninstall wally` to \
                         remove the binary.",
                    );
                } else {
                    out::status_line(&format!(
                        "{exe} could not be verified as wally's own install; leaving it in place."
                    ));
                }
            }
        }
    }

    let present: Vec<Target> = targets.into_iter().filter(|t| t.path.exists()).collect();
    #[cfg(windows)]
    let has_manual_removal = manual_removal.is_some();
    #[cfg(not(windows))]
    let has_manual_removal = false;

    if present.is_empty() && !has_manual_removal {
        out::status_line("nothing to uninstall; wally is already gone.");
        return 0;
    }

    if !present.is_empty() {
        out::status_line("wally uninstall will delete:");
        for target in &present {
            let size = human_size(&target.path);
            let size_suffix = if size.is_empty() {
                String::new()
            } else {
                format!("  ({size})")
            };
            out::status_line(&format!(
                "  {}  {}{size_suffix}",
                target.label,
                target.path.display()
            ));
        }
    }
    #[cfg(windows)]
    if let Some(dir) = &manual_removal {
        // Windows will not let a running process delete (or even rename) its
        // own program file, so there is no safe way to have this process
        // finish that part of the job itself. Spawning a detached helper
        // that waits on our pid and deletes behind us is the other option
        // the C++ never had either, but it trades one guaranteed, honest
        // message for a background process that can be killed, blocked by
        // antivirus, or race a relaunch of wally -- worse failure modes than
        // telling the person the one folder left to remove by hand.
        out::status_line(&format!(
            "wally cannot delete its own running program on Windows; close this window, \
             then delete {} yourself to finish uninstalling.",
            dir.display()
        ));
    }
    out::status_line("your coding tools (claude-code, opencode, ...) are left untouched.");

    if !present.is_empty() && !yes {
        // Never delete without a real confirmation. A non-interactive shell
        // (piped or redirected stdin) cannot answer, so it must pass --yes on
        // purpose rather than have the prompt silently skipped.
        if !term::stdin_is_tty() {
            out::error_line(
                "uninstall needs a terminal to confirm; re-run with --yes to delete \
                 non-interactively.",
            );
            return 1;
        }
        if !confirm("delete all of the above?") {
            out::status_line("aborted; nothing was deleted.");
            return 1;
        }
    }

    // Everything but the running binary (or the tree containing it) first;
    // that one last, because on a unix filesystem deleting the file the
    // process is executing is safe -- the inode lives until the process
    // exits -- and the same now holds for deleting the whole install.sh
    // tree out from under it.
    let mut failures = 0;
    let mut deferred: Option<PathBuf> = None;
    for target in &present {
        if target.label == "binary" || target.label == "install" {
            deferred = Some(target.path.clone());
            continue;
        }
        if let Err(e) = remove_all(&target.path) {
            out::error_line(&format!(
                "could not delete {}: {}",
                target.path.display(),
                os_error_message(&e)
            ));
            failures += 1;
        }
    }
    if let Some(deferred) = deferred {
        if let Err(e) = remove_all(&deferred) {
            out::error_line(&format!(
                "could not delete {}: {}",
                deferred.display(),
                os_error_message(&e)
            ));
            failures += 1;
        }
    }
    // Reported above already; counted here so a script checking the exit
    // code sees an incomplete uninstall rather than a clean 0.
    #[cfg(windows)]
    if has_manual_removal {
        failures += 1;
    }

    if failures != 0 {
        return 1;
    }
    out::status_line("wally is uninstalled. thanks for trying it.");
    0
}

/// `wally help [command]`. Renders the same text `app.get_subcommand(topic)
/// ->help(app.get_name())` would in C++ for a known `topic`; with no topic,
/// or an unknown one, C++ falls back to `app.help()` -- which CLI11
/// delegates to whichever subcommand was actually parsed (`help` itself
/// here), i.e. it prints the `help` subcommand's OWN help, never the
/// top-level help.
///
/// The Callback type has no access to the App tree at call time, and this
/// callback is bound at *registration* time -- before bench/backends/
/// telemetry (and everything registered after `register_help`) exist. A
/// plain clone of `app` here would miss them. Instead this returns a
/// handle `configure_app` fills with a clone of the COMPLETE tree after the
/// last `register_*` call, so topic lookups made through the handle at call
/// time see every subcommand.
/// Which subcommand's help `wally help <topic>` should render: `topic`
/// itself when it names a real subcommand of `root`, otherwise the `help`
/// subcommand's OWN help -- matching C++'s `app.help()`, which CLI11
/// delegates to whichever subcommand was actually parsed ("help" itself)
/// rather than rendering the top level.
fn help_render_path(root: &App, topic: &str) -> Vec<String> {
    if !topic.is_empty() && root.get_subcommand(topic).is_some() {
        vec![topic.to_string()]
    } else {
        vec!["help".to_string()]
    }
}

pub fn register_help(app: &mut App) -> Rc<RefCell<Option<App>>> {
    let tree: Rc<RefCell<Option<App>>> = Rc::new(RefCell::new(None));
    let tree_for_closure = Rc::clone(&tree);
    let cmd = app.add_subcommand("help", "Show help for a command");
    cmd.add_option("command", ValueType::Text, "Command to describe");
    cmd.callback(move |parsed, options| {
        let topic = parsed.get_str("command").unwrap_or_default();
        let color = color_output_enabled(options.no_color);
        let borrowed = tree_for_closure.borrow();
        // Only unpopulated if configure_app never filled the handle (a
        // wiring bug, not a runtime condition) -- degrade to nothing printed
        // rather than panic.
        let Some(root) = borrowed.as_ref() else {
            return 0;
        };
        let path = help_render_path(root, &topic);
        let text = root.render_help(&path, color);
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        0
    });
    tree
}

pub fn register_uninstall(app: &mut App) {
    let cmd = app.add_subcommand("uninstall", "Remove wally, its models and its config");
    cmd.add_flag("-y,--yes", "Skip the confirmation prompt");
    cmd.callback(|parsed, options| run_uninstall(parsed.flag("--yes"), &options.home_override));
}

#[cfg(test)]
mod tests {
    use super::*;

    // Dir_size (uninstall's pre-delete size preview) must follow a
    // symlink to a regular file, matching C++'s is_regular_file(ec)/
    // file_size(ec) (which call status(), following symlinks).
    #[test]
    #[cfg(unix)]
    fn dir_size_follows_symlinked_regular_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob.bin");
        std::fs::write(&blob, b"uninstall preview bytes").expect("write blob");

        let scan_dir = dir.path().join("scan");
        std::fs::create_dir(&scan_dir).expect("mkdir");
        std::os::unix::fs::symlink(&blob, scan_dir.join("blob.bin")).expect("symlink");

        assert_eq!(dir_size(&scan_dir), 23);
    }

    // The printed message must be the bare OS message, with no
    // "(os error N)" suffix (that suffix is Rust io::Error::Display-only and
    // has no C++ equivalent — std::error_code::message() never appends it).
    #[test]
    fn os_error_message_strips_the_os_error_number_suffix() {
        let error = std::io::Error::from_raw_os_error(13); // EACCES
        let message = os_error_message(&error);
        assert!(!message.contains("os error"));
        assert!(!message.is_empty());
    }

    #[test]
    fn os_error_message_leaves_a_non_os_error_display_unchanged() {
        let error = std::io::Error::other("custom failure text");
        assert_eq!(os_error_message(&error), "custom failure text");
    }

    // `wally help <topic>` reached through a leading global flag runs
    // the `help` subcommand's own registered callback, not app::run's
    // pre-parse shortcut. help_render_path is what that callback uses to
    // pick which subcommand's help to render.
    fn app_with_help_and_bench() -> App {
        let mut app = App::new("root description", "wally");
        app.add_subcommand("help", "Show help for a command");
        app.add_subcommand("bench", "Measure throughput and load time");
        app
    }

    #[test]
    fn help_render_path_resolves_a_real_topic() {
        let app = app_with_help_and_bench();
        assert_eq!(help_render_path(&app, "bench"), vec!["bench".to_string()]);
    }

    #[test]
    fn help_render_path_falls_back_to_helps_own_help_for_an_empty_topic() {
        let app = app_with_help_and_bench();
        assert_eq!(help_render_path(&app, ""), vec!["help".to_string()]);
    }

    #[test]
    fn help_render_path_falls_back_to_helps_own_help_for_an_unknown_topic() {
        let app = app_with_help_and_bench();
        // "pull" is never a bare top-level name (only "models pull"), so this
        // is the same "unknown topic" case as a typo.
        assert_eq!(help_render_path(&app, "pull"), vec!["help".to_string()]);
    }

    // End-to-end coverage of the stale-snapshot case (a tree cloned at
    // register_help's registration time, before bench/backends/telemetry
    // exist) lives in tests/test_wally_help_routing.rs, which drives the real
    // built binary through app::run's non-shortcut parse path.

    // Serializes every test below that sets HOME -- the same pattern
    // src/harness/agents.rs uses for its own env-touching tests.
    #[cfg(unix)]
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    // `wally uninstall` used to ignore the global `--home` override and
    // always resolve against HOME/RUNANYWHERE_HOME, so it could remove a
    // different model store than the one `wally models` was just pointed at.
    #[test]
    #[cfg(unix)]
    fn models_directory_prefers_the_home_override_over_home() {
        let _lock = env_lock();
        let home_a = tempfile::tempdir().expect("tempdir A");
        let home_b = tempfile::tempdir().expect("tempdir B");
        std::fs::create_dir_all(home_b.path().join("Models")).expect("mkdir B/Models");
        // SAFETY: env_lock() is held for this whole test body.
        unsafe {
            std::env::set_var("HOME", home_a.path());
            std::env::remove_var("RUNANYWHERE_HOME");
        }

        let result = models_directory(&home_b.path().to_string_lossy());

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };

        assert!(
            result.starts_with(home_b.path().to_str().expect("utf8 path")),
            "models_directory({home_b:?}) must resolve against the override, not HOME \
             ({home_a:?}): got {result}"
        );
    }

    // A real install.sh tree in a temp HOME: LIB_DIR with a `bin/wally`
    // file, and the launcher symlink install.sh creates at
    // ~/.local/bin/wally pointing at it.
    #[cfg(unix)]
    struct InstallShLayout {
        _home: tempfile::TempDir,
        home_path: PathBuf,
        exe: PathBuf,
        lib_dir: PathBuf,
        launcher: PathBuf,
    }

    #[cfg(unix)]
    fn install_sh_layout_fixture() -> InstallShLayout {
        let home = tempfile::tempdir().expect("tempdir");
        let home_path = home.path().to_path_buf();
        let lib_dir = home_path.join(".local/lib/wally");
        let bin_dir = lib_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("mkdir lib/bin");
        let exe = bin_dir.join("wally");
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write exe");

        let bin_link_dir = home_path.join(".local/bin");
        std::fs::create_dir_all(&bin_link_dir).expect("mkdir bin");
        let launcher = bin_link_dir.join("wally");
        std::os::unix::fs::symlink(&exe, &launcher).expect("symlink launcher");

        InstallShLayout {
            _home: home,
            home_path,
            exe,
            lib_dir,
            launcher,
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_sh_layout_finds_lib_dir_and_launcher_from_the_running_exe() {
        let _lock = env_lock();
        let fixture = install_sh_layout_fixture();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::set_var("HOME", &fixture.home_path) };

        let resolved_exe = std::fs::canonicalize(&fixture.exe).expect("canonicalize exe");
        let (lib_dir, launcher) = install_sh_layout(&resolved_exe.to_string_lossy())
            .expect("install.sh layout should be recognised");
        assert_eq!(
            lib_dir,
            std::fs::canonicalize(&fixture.lib_dir).expect("canonicalize lib_dir")
        );
        assert_eq!(launcher, fixture.launcher);

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };
    }

    #[test]
    #[cfg(unix)]
    fn install_sh_layout_rejects_a_binary_outside_the_tree() {
        let _lock = env_lock();
        let fixture = install_sh_layout_fixture();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::set_var("HOME", &fixture.home_path) };

        let elsewhere = fixture.home_path.join("elsewhere-wally");
        std::fs::write(&elsewhere, b"#!/bin/sh\n").expect("write elsewhere");
        assert!(install_sh_layout(&elsewhere.to_string_lossy()).is_none());

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };
    }

    // `wally uninstall` must never delete a binary it cannot prove came from
    // install.sh -- a Homebrew Cellar install is the case this comment fixed.
    #[test]
    #[cfg(unix)]
    fn unix_exe_targets_skips_a_homebrew_binary() {
        let _lock = env_lock();
        let fixture = install_sh_layout_fixture();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::set_var("HOME", &fixture.home_path) };

        let targets = unix_exe_targets("/opt/homebrew/Cellar/wally/0.5.10/bin/wally");
        assert!(
            targets.is_empty(),
            "a Homebrew-managed binary is never install.sh's own tree, so there is nothing to \
             delete: {targets:?}"
        );

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };
    }

    // A source build (arbitrary path, no install.sh tree behind it) must be
    // left alone too -- uninstall has no way to verify it owns the binary.
    #[test]
    #[cfg(unix)]
    fn unix_exe_targets_skips_an_unverified_source_build() {
        let _lock = env_lock();
        let fixture = install_sh_layout_fixture();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::set_var("HOME", &fixture.home_path) };

        let targets = unix_exe_targets("/home/dev/wally/target/release/wally");
        assert!(
            targets.is_empty(),
            "an unverified binary location is never deleted: {targets:?}"
        );

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };
    }

    // The one case that IS deleted -- install.sh's own tree -- must still
    // produce the launcher + install targets, unaffected by the two skips
    // above.
    #[test]
    #[cfg(unix)]
    fn unix_exe_targets_still_covers_the_install_sh_layout() {
        let _lock = env_lock();
        let fixture = install_sh_layout_fixture();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::set_var("HOME", &fixture.home_path) };

        let resolved_exe = std::fs::canonicalize(&fixture.exe).expect("canonicalize exe");
        let targets = unix_exe_targets(&resolved_exe.to_string_lossy());
        let labels: Vec<&str> = targets.iter().map(|t| t.label).collect();
        assert!(labels.contains(&"launcher"), "targets: {labels:?}");
        assert!(labels.contains(&"install"), "targets: {labels:?}");

        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOME") };
    }

    #[test]
    #[cfg(unix)]
    fn launcher_target_accepts_a_symlink_that_resolves_into_lib_dir() {
        let fixture = install_sh_layout_fixture();
        let lib_dir = std::fs::canonicalize(&fixture.lib_dir).expect("canonicalize lib_dir");
        assert_eq!(
            launcher_target(&fixture.launcher, &lib_dir),
            Some(fixture.launcher.clone())
        );
    }

    #[test]
    #[cfg(unix)]
    fn launcher_target_rejects_a_plain_file() {
        let fixture = install_sh_layout_fixture();
        let lib_dir = std::fs::canonicalize(&fixture.lib_dir).expect("canonicalize lib_dir");
        let not_a_symlink = fixture.home_path.join(".local/bin/not-a-symlink");
        std::fs::write(&not_a_symlink, b"plain file").expect("write plain file");
        assert_eq!(launcher_target(&not_a_symlink, &lib_dir), None);
    }

    #[test]
    #[cfg(unix)]
    fn launcher_target_rejects_a_symlink_pointing_outside_lib_dir() {
        let fixture = install_sh_layout_fixture();
        let lib_dir = std::fs::canonicalize(&fixture.lib_dir).expect("canonicalize lib_dir");
        let outside_target = fixture.home_path.join("someone-elses-wally");
        std::fs::write(&outside_target, b"not ours").expect("write outside target");
        let rogue_link = fixture.home_path.join(".local/bin/rogue");
        std::os::unix::fs::symlink(&outside_target, &rogue_link).expect("symlink rogue");
        assert_eq!(launcher_target(&rogue_link, &lib_dir), None);
    }

    #[test]
    #[cfg(unix)]
    fn launcher_target_rejects_a_dangling_symlink() {
        let fixture = install_sh_layout_fixture();
        let lib_dir = std::fs::canonicalize(&fixture.lib_dir).expect("canonicalize lib_dir");
        let missing_target = fixture.lib_dir.join("bin/gone");
        let dangling = fixture.home_path.join(".local/bin/dangling");
        std::os::unix::fs::symlink(&missing_target, &dangling).expect("symlink dangling");
        assert_eq!(launcher_target(&dangling, &lib_dir), None);
    }

    // Serializes every test below that sets LOCALAPPDATA -- same pattern as
    // the Unix env_lock() above, kept separate since it guards a different
    // variable and only ever runs on Windows.
    #[cfg(windows)]
    fn windows_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    #[test]
    #[cfg(windows)]
    fn windows_install_directory_finds_the_directory_from_the_running_exe() {
        let _lock = windows_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let local_app_data = temp.path();
        // SAFETY: windows_env_lock() is held for this whole test body.
        unsafe { std::env::set_var("LOCALAPPDATA", local_app_data) };

        let install_dir = local_app_data.join("Programs").join("wally");
        std::fs::create_dir_all(&install_dir).expect("mkdir install dir");
        let exe = install_dir.join("wally.exe");
        std::fs::write(&exe, b"stub").expect("write exe");

        // The plain path, as `current_exe()` reports it -- not canonicalized,
        // which on Windows would add the `\\?\` prefix and hide the mismatch.
        let resolved = windows_install_directory(&exe.to_string_lossy())
            .expect("windows install directory should be recognised");
        assert_eq!(resolved, install_dir);
        let verbatim_exe = std::fs::canonicalize(&exe).expect("canonicalize exe");
        assert_eq!(
            windows_install_directory(&verbatim_exe.to_string_lossy()),
            Some(install_dir.clone()),
            "a verbatim exe path must match too"
        );

        // SAFETY: still holding windows_env_lock().
        unsafe { std::env::remove_var("LOCALAPPDATA") };
    }

    #[test]
    #[cfg(windows)]
    fn windows_install_directory_rejects_a_binary_outside_the_programs_tree() {
        let _lock = windows_env_lock();
        let temp = tempfile::tempdir().expect("tempdir");
        let local_app_data = temp.path();
        // SAFETY: windows_env_lock() is held for this whole test body.
        unsafe { std::env::set_var("LOCALAPPDATA", local_app_data) };

        let elsewhere = local_app_data.join("elsewhere-wally.exe");
        std::fs::write(&elsewhere, b"stub").expect("write elsewhere");
        assert!(windows_install_directory(&elsewhere.to_string_lossy()).is_none());

        // SAFETY: still holding windows_env_lock().
        unsafe { std::env::remove_var("LOCALAPPDATA") };
    }
}
