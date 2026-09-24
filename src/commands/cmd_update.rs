//! Port of src/commands/cmd_update.cpp.
//!
//! `wally update` — re-run the installer, telling it our version.
//!
//! The installer already knows how to fetch the latest release; the only thing
//! it cannot know on its own is what the caller is running. So update hands it
//! this build's version, and the script decides: pull the newer release, or
//! report that this machine is already current and download nothing.

use crate::cli::App;
use crate::io::output as out;

// The same script the install line in the README pipes to a shell. Kept as one
// constant so the update path and the documented install path cannot drift.
#[cfg(not(windows))]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh";

pub fn register_update(app: &mut App) {
    let cmd = app.add_subcommand("update", "Update wally to the latest release");
    cmd.add_flag("--nightly", "Track the development channel");
    cmd.callback(|parsed, _options| {
        if run_update(parsed.flag("--nightly")) != 0 {
            return 1;
        }
        0
    });
}

/// Shared by `wally update` and the whole-argv `-u/--update` shortcut.
#[cfg(windows)]
pub fn run_update(nightly: bool) -> i32 {
    let _ = nightly;
    // The installer is a POSIX shell script; the Windows bottle updates
    // through its own channel, not this command.
    out::error_line("wally update is not available on Windows; reinstall from the release page");
    1
}

/// Shared by `wally update` and the whole-argv `-u/--update` shortcut.
#[cfg(not(windows))]
pub fn run_update(nightly: bool) -> i32 {
    // WALLY_VERSION is a compile-time constant and the flag is a fixed token,
    // so the command line carries nothing a caller could inject.
    let mut command = format!("curl -fsSL {INSTALL_URL} | sh -s --");
    if nightly {
        command.push_str(" --nightly");
    }
    command.push_str(" --version=");
    command.push_str(env!("WALLY_VERSION"));

    out::status_line("checking for a newer wally...");
    // A non-zero exit means the installer already explained why on its own
    // stderr; a failure to even spawn `sh` counts the same way.
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        0
    } else {
        1
    }
}
