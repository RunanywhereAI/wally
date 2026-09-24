//! Small process-wide helpers.

pub mod term;

/// Read an environment variable as UTF-8. Returns `None` when unset or empty —
/// the C++ code treated an empty value as unset throughout.
///
/// On Windows the C++ port had to read the wide value and convert with CP_UTF8,
/// because `getenv` decodes through the ANSI code page and corrupts Unicode
/// paths (an international LOCALAPPDATA/USERPROFILE/RUNANYWHERE_HOME). Rust's
/// `var_os` reads the wide value already; lossy conversion only affects values
/// that are not valid Unicode at all.
pub fn getenv(name: &str) -> Option<String> {
    std::env::var_os(name)
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty())
}

/// `getenv`, or the empty string — the shape most C++ call sites used.
pub fn getenv_or_empty(name: &str) -> String {
    getenv(name).unwrap_or_default()
}
