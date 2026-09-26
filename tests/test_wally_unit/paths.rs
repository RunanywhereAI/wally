//! test_wally_unit.cpp: directory normalisation and home/state resolution.

use wally::config::cli_paths as paths;
use wally::config::cli_paths::resolve_home;

use super::common::{env_lock, EnvGuard};

#[test]
fn normalize_dir() {
    assert_eq!(paths::normalize_dir("/a/b/"), "/a/b");
    assert_eq!(paths::normalize_dir("/a/b///"), "/a/b");
    assert_eq!(paths::normalize_dir("/"), "/");
    assert_eq!(paths::normalize_dir(""), "");
}

#[test]
fn resolve_home_precedence() {
    let _lock = env_lock();
    {
        // Flag override wins over env.
        let mut env = EnvGuard::new();
        env.set("RUNANYWHERE_HOME", "/from-env/runanywhere");
        assert_eq!(
            resolve_home("/from-flag/runanywhere/"),
            "/from-flag/runanywhere"
        );
        assert_eq!(resolve_home(""), "/from-env/runanywhere");
    }
    #[cfg(windows)]
    {
        let mut env = EnvGuard::new();
        env.unset("RUNANYWHERE_HOME")
            .set("LOCALAPPDATA", "C:/wally-local");
        assert_eq!(resolve_home(""), "C:/wally-local/RunAnywhere");
        // A real LOCALAPPDATA is backslash-separated, and the SDK's default base
        // dir appends "/RunAnywhere" to it as-is.
        env.set("LOCALAPPDATA", "C:\\wally-local");
        assert_eq!(resolve_home(""), "C:/wally-local/RunAnywhere");
    }
    #[cfg(not(windows))]
    {
        // Default: XDG data dir under runanywhere.
        let mut env = EnvGuard::new();
        env.unset("RUNANYWHERE_HOME")
            .set("XDG_DATA_HOME", "/xdg-data");
        assert_eq!(resolve_home(""), "/xdg-data/runanywhere");
    }
}

#[test]
fn state_dir() {
    let _lock = env_lock();
    {
        let mut env = EnvGuard::new();
        env.set("XDG_STATE_HOME", "/xdg-state");
        assert_eq!(paths::state_dir(), "/xdg-state/runanywhere");
    }
    #[cfg(windows)]
    {
        // A real LOCALAPPDATA is backslash-separated while the suffix appended
        // to it is not, so the join must not leave mixed separators behind.
        let mut env = EnvGuard::new();
        env.unset("XDG_STATE_HOME")
            .unset("HOME")
            .set("LOCALAPPDATA", "C:\\wally-local");
        assert_eq!(paths::state_dir(), "C:/wally-local/RunAnywhere/state");
    }
    #[cfg(windows)]
    {
        // MSYS2 / Git Bash set HOME on Windows. It must not win over
        // LOCALAPPDATA, or state lands in a POSIX-shaped directory under the
        // user profile.
        let mut env = EnvGuard::new();
        env.unset("XDG_STATE_HOME")
            .set("HOME", "C:/msys-home")
            .set("LOCALAPPDATA", "C:/wally-local");
        assert_eq!(paths::state_dir(), "C:/wally-local/RunAnywhere/state");
    }
    {
        // With every input unset there is nowhere to resolve state under, so
        // callers (e.g. cmd_editors's prepare_claude_config_dir) must see the
        // empty string rather than a plausible-looking but bogus path.
        let mut env = EnvGuard::new();
        env.unset("XDG_STATE_HOME").unset("HOME");
        #[cfg(windows)]
        env.unset("LOCALAPPDATA").unset("USERPROFILE");
        assert_eq!(paths::state_dir(), "");
    }
}
