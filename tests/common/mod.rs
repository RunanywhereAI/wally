//! Shared helpers for the Rust test suites (port of the useful parts of
//! tests/test_common.h; the suite/ASSERT machinery is `#[test]` now). Areas that
//! need more (WAV fixtures, fake servers) add their own `tests/common/<area>.rs`
//! and declare it from their test file with `#[path]`, so no two ports edit this
//! file.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Tests in one binary run on parallel threads; anything that reads or writes
/// process environment holds this lock for its whole body.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets environment variables for its lifetime and restores the previous values
/// on drop. Hold `env_lock()` while it lives.
pub struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub fn new() -> Self {
        EnvGuard { saved: Vec::new() }
    }

    fn remember(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_string(), std::env::var_os(key)));
        }
    }

    pub fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) -> &mut Self {
        self.remember(key);
        // SAFETY: callers hold env_lock(), so no other test thread touches env.
        unsafe { std::env::set_var(key, value) };
        self
    }

    pub fn unset(&mut self, key: &str) -> &mut Self {
        self.remember(key);
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
        self
    }
}

impl Default for EnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..).rev() {
            // SAFETY: as above.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

/// An isolated home for one test: every directory wally reads or writes lives
/// under it. `env()` is what to hand a spawned wally.
pub struct TempHome {
    pub dir: tempfile::TempDir,
}

impl TempHome {
    pub fn new() -> Self {
        TempHome {
            dir: tempfile::tempdir().expect("temp dir"),
        }
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn join(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// The same isolation tests/golden was captured with.
    pub fn env(&self) -> BTreeMap<String, String> {
        let home = self.path().to_string_lossy().into_owned();
        let mut env = BTreeMap::new();
        env.insert("HOME".into(), home.clone());
        env.insert("USERPROFILE".into(), home.clone());
        env.insert("RUNANYWHERE_HOME".into(), format!("{home}/ra"));
        env.insert("WALLY_PROFILE_DIR".into(), format!("{home}/profile"));
        env.insert("XDG_STATE_HOME".into(), format!("{home}/state"));
        env.insert("XDG_CONFIG_HOME".into(), format!("{home}/config"));
        env.insert("XDG_DATA_HOME".into(), format!("{home}/data"));
        env.insert("RUNANYWHERE_BASE_URL".into(), "http://127.0.0.1:9".into());
        env.insert("TERM".into(), "dumb".into());
        env.insert("LANG".into(), "C".into());
        env.insert("LC_ALL".into(), "C".into());
        env
    }
}

impl Default for TempHome {
    fn default() -> Self {
        Self::new()
    }
}

/// The cargo-built wally binary.
pub fn wally_bin() -> &'static str {
    env!("CARGO_BIN_EXE_wally")
}

/// Run the wally binary with `args` in `home`'s isolated environment (clean env,
/// stdin closed). Returns (exit code, stdout, stderr).
pub fn run_wally(home: &TempHome, args: &[&str]) -> (i32, String, String) {
    run_wally_env(home, args, &[])
}

pub fn run_wally_env(
    home: &TempHome,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String, String) {
    let mut cmd = Command::new(wally_bin());
    cmd.args(args).env_clear().stdin(Stdio::null());
    for (k, v) in home.env() {
        cmd.env(k, v);
    }
    // PATH without user tool dirs, so no coding tool is ever found or launched.
    #[cfg(windows)]
    cmd.env(
        "PATH",
        std::env::var_os("SystemRoot")
            .map(|r| format!("{}\\System32", r.to_string_lossy()))
            .unwrap_or_default(),
    );
    #[cfg(not(windows))]
    cmd.env("PATH", "/usr/bin:/bin");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out: Output = cmd.output().expect("spawn wally");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Drive the production entry point in-process, as the C++ tests did with
/// `wally::run(argc, argv)`. `args` excludes the program name. Output goes to
/// the test's own stdout/stderr; only the exit code comes back.
pub fn run_in_process(args: &[&str]) -> i32 {
    let mut argv = vec!["wally".to_string()];
    argv.extend(args.iter().map(|a| a.to_string()));
    wally::app::run(&argv)
}

/// Case-insensitive substring test (test_common.h contains_ci).
pub fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}
