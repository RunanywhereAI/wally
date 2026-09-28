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
#[cfg(not(windows))]
use crate::util::getenv;

// The same script the install line in the README pipes to a shell. Kept as one
// constant so the update path and the documented install path cannot drift.
#[cfg(not(windows))]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh";

// Homebrew's own prefixes, plus whatever `$HOMEBREW_PREFIX` names (set by
// `brew shellenv` and by every formula's build environment). A binary
// resolved (symlinks followed) into one of these came from `brew install`,
// not install.sh -- piping install.sh at it would lay a second copy under
// ~/.local that `brew upgrade`/`brew uninstall` never sees again.
#[cfg(not(windows))]
const HOMEBREW_PREFIXES: [&str; 3] = [
    "/opt/homebrew",
    "/usr/local/Cellar",
    "/home/linuxbrew/.linuxbrew",
];

#[cfg(not(windows))]
pub(crate) fn is_homebrew_managed(exe: &str) -> bool {
    if exe.is_empty() {
        return false;
    }
    // Whole path components, so /opt/homebrew-old is not /opt/homebrew.
    let exe = std::path::Path::new(exe);
    if let Some(prefix) = getenv("HOMEBREW_PREFIX") {
        if !prefix.is_empty() && exe.starts_with(&prefix) {
            return true;
        }
    }
    HOMEBREW_PREFIXES
        .iter()
        .any(|prefix| exe.starts_with(prefix))
}

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
    let exe = crate::commands::cmd_maintenance::self_executable();
    if is_homebrew_managed(&exe) {
        // Not a failure: the command's job -- get the person to the newest
        // release -- is done by pointing them at the manager that actually
        // owns this binary. install.sh would "succeed" too, but by writing a
        // second, unmanaged copy under ~/.local that outlives every future
        // `brew upgrade`, which is a worse outcome than doing nothing here.
        out::status_line("wally was installed with Homebrew; run `brew upgrade wally` to update.");
        return 0;
    }

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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::util::env_lock::lock as env_lock;

    #[test]
    fn is_homebrew_managed_matches_the_apple_silicon_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(is_homebrew_managed(
            "/opt/homebrew/Cellar/wally/0.5.10/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_matches_the_intel_cellar_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(is_homebrew_managed(
            "/usr/local/Cellar/wally/0.5.10/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_matches_a_custom_homebrew_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body; `set_var`'s
        // only panic case is a name containing '=' or NUL, and this literal
        // has neither.
        unsafe { std::env::set_var("HOMEBREW_PREFIX", "/custom/brew") };
        let result = is_homebrew_managed("/custom/brew/Cellar/wally/0.5.10/bin/wally");
        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(result);
    }

    #[test]
    fn is_homebrew_managed_rejects_an_install_sh_path() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed(
            "/home/user/.local/lib/wally/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_rejects_a_sibling_of_a_homebrew_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed("/opt/homebrew-old/bin/wally"));
        assert!(!is_homebrew_managed(
            "/usr/local/Cellar-backup/wally/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_rejects_an_empty_path() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed(""));
    }
}
