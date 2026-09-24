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
#[cfg(target_os = "macos")]
fn self_executable() -> String {
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
fn self_executable() -> String {
    std::fs::read_link("/proc/self/exe")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn self_executable() -> String {
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
// or {base}/RunAnywhere/Models. Every base it can derive is checked -- the env
// override, the kit's answer, and the platform default -- and the first store
// that actually exists wins, so a missing bootstrap cannot hide it.
fn models_directory() -> String {
    let mut bases: Vec<String> = Vec::new();
    if let Some(env) = getenv("RUNANYWHERE_HOME") {
        bases.push(env);
    }
    let home = cli_paths::resolve_home("");
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

/// Shared by `wally uninstall` and the whole-argv `-U/--uninstall` shortcut.
pub fn run_uninstall(yes: bool) -> i32 {
    let mut targets: Vec<Target> = Vec::new();

    let models = models_directory();
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
        let exe_path = PathBuf::from(&exe);
        targets.push(Target {
            label: "binary",
            path: exe_path.clone(),
        });
        // The Metal shader bundles the installer placed beside the binary. Only
        // *.bundle next to wally, never anything else on PATH.
        if let Some(parent) = exe_path.parent() {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) == Some("bundle") {
                        targets.push(Target {
                            label: "bundle",
                            path,
                        });
                    }
                }
            }
        }
    }

    let present: Vec<Target> = targets.into_iter().filter(|t| t.path.exists()).collect();
    if present.is_empty() {
        out::status_line("nothing to uninstall; wally is already gone.");
        return 0;
    }

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
    out::status_line("your coding tools (claude-code, opencode, ...) are left untouched.");

    if !yes {
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

    // Everything but the running binary first; the binary last, because on a
    // unix filesystem deleting the file the process is executing is safe -- the
    // inode lives until the process exits.
    let mut failures = 0;
    let mut binary: Option<PathBuf> = None;
    for target in &present {
        if target.label == "binary" {
            binary = Some(target.path.clone());
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
    if let Some(binary) = binary {
        if let Err(e) = std::fs::remove_file(&binary) {
            out::error_line(&format!(
                "could not delete {}: {}",
                binary.display(),
                os_error_message(&e)
            ));
            failures += 1;
        }
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
    cmd.callback(|parsed, _options| run_uninstall(parsed.flag("--yes")));
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
}
