//! wally directory resolution.
//!
//! One knob controls where models live: the RunAnywhere HOME directory.
//!   resolution: --home flag → $RUNANYWHERE_HOME → ${XDG_DATA_HOME:-~/.local/share}/runanywhere
//! Models are derived BY COMMONS from that home via rac_model_paths_*
//! (home named "runanywhere" → <home>/Models/{framework}/<id>, the same layout
//! the Linux test rig and Playground tooling use).
//!
//! Config (secure store) stays under ${XDG_CONFIG_HOME:-~/.config}/runanywhere;
//! REPL history under ${XDG_STATE_HOME:-~/.local/state}/runanywhere.

use crate::sys;
use crate::util::getenv;

/// Strip trailing '/' (keeps root "/"). On Windows also folds '\\' to '/'.
pub fn normalize_dir(dir: &str) -> String {
    // Windows environment values (LOCALAPPDATA, USERPROFILE) come back
    // backslash-separated, while every path built from them here appends
    // '/'-joined segments -- leaving `wally info` printing a mixed
    // C:\Users\...\AppData\Local/RunAnywhere. Fold to '/' so a single style
    // survives into the output; Win32 accepts either separator.
    #[cfg(windows)]
    let mut dir = dir.replace('\\', "/");
    #[cfg(not(windows))]
    let mut dir = dir.to_string();
    while dir.len() > 1 && (dir.ends_with('/') || dir.ends_with('\\')) {
        dir.pop();
    }
    dir
}

/// Resolve the RunAnywhere home (storage base dir) — see the module header for
/// the precedence. Returns the empty string only when $HOME is unresolvable.
pub fn resolve_home(override_dir: &str) -> String {
    if !override_dir.is_empty() {
        return normalize_dir(override_dir);
    }
    if let Some(env) = getenv("RUNANYWHERE_HOME") {
        return normalize_dir(&env);
    }
    let mut buffer = [0 as std::ffi::c_char; 1024];
    // SAFETY: the kit writes at most `len` bytes, NUL-terminated, into buffer.
    let rc = unsafe { sys::rac_desktop_default_base_dir(buffer.as_mut_ptr(), buffer.len()) };
    if rc == sys::SUCCESS {
        // SAFETY: NUL-terminated by the kit on success.
        let dir = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
        return normalize_dir(&dir.to_string_lossy());
    }
    String::new()
}

/// ${XDG_STATE_HOME:-~/.local/state}/runanywhere (not created). On Windows,
/// with XDG_STATE_HOME unset, %LOCALAPPDATA%/RunAnywhere/state even when HOME
/// is set (MSYS2 / Git Bash).
pub fn state_dir() -> String {
    if let Some(env) = getenv("XDG_STATE_HOME") {
        return normalize_dir(&env) + "/runanywhere";
    }
    // Checked ahead of HOME: MSYS2 / Git Bash set HOME on Windows, which would
    // otherwise divert state into %USERPROFILE%/.local/state/runanywhere and
    // leave the two branches below permanently unreachable.
    #[cfg(windows)]
    {
        if let Some(local) = getenv("LOCALAPPDATA") {
            return normalize_dir(&local) + "/RunAnywhere/state";
        }
        if let Some(profile) = getenv("USERPROFILE") {
            return normalize_dir(&profile) + "/AppData/Local/RunAnywhere/state";
        }
    }
    if let Some(home) = getenv("HOME") {
        return normalize_dir(&home) + "/.local/state/runanywhere";
    }
    String::new()
}
