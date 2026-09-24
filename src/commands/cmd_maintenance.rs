//! Port of src/commands/cmd_maintenance.cpp. Owner: the maintenance/diagnostics port.
//!
//! `wally help` (a plain-word mirror of `--help`) and `wally uninstall` (remove
//! wally, its on-device models, and its config -- never the coding tools a
//! person installed themselves).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::account::credentials;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{self, color_output_enabled};
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

// Best-effort recursive size, symlinks not followed while walking (avoids a
// symlink cycle; the C++ recursive_directory_iterator default has the same
// effect for directory symlinks).
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
            out::error_line(&format!("could not delete {}: {e}", target.path.display()));
            failures += 1;
        }
    }
    if let Some(binary) = binary {
        if let Err(e) = std::fs::remove_file(&binary) {
            out::error_line(&format!("could not delete {}: {e}", binary.display()));
            failures += 1;
        }
    }

    if failures != 0 {
        return 1;
    }
    out::status_line("wally is uninstalled. thanks for trying it.");
    0
}

/// `wally help [command]`. Renders the same text `--help` would, either for
/// `command` or (with no argument, or an unknown one) the top level.
///
/// Caveat: the Callback type has no access to the App tree at call time, so
/// this clones the root `App` at *registration* time instead. That snapshot
/// only contains subcommands registered on `app` before `register_help` runs
/// -- `configure_app` must call this after every other `register_*`.
pub fn register_help(app: &mut App) {
    let root = app.clone();
    let cmd = app.add_subcommand("help", "Show help for a command");
    cmd.add_option("command", ValueType::Text, "Command to describe");
    cmd.callback(move |parsed, options| {
        let topic = parsed.get_str("command").unwrap_or_default();
        let color = color_output_enabled(options.no_color);
        let text = if !topic.is_empty() {
            root.get_subcommand(&topic)
                .map(|sub| cli_formatter::make_help(sub, &root.name, color))
        } else {
            None
        };
        let text = text.unwrap_or_else(|| cli_formatter::make_help(&root, "", color));
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(text.as_bytes());
        let _ = stdout.flush();
        0
    });
}

pub fn register_uninstall(app: &mut App) {
    let cmd = app.add_subcommand("uninstall", "Remove wally, its models and its config");
    cmd.add_flag("-y,--yes", "Skip the confirmation prompt");
    cmd.callback(|parsed, _options| run_uninstall(parsed.flag("--yes")));
}
