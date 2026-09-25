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

/// Howard Hinnant's `civil_from_days` (public domain): the proleptic Gregorian
/// calendar date for `z` days since 1970-01-01, in UTC. Used instead of
/// gmtime_r/gmtime_s so formatting a timestamp has no platform-specific
/// calendar dependency.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}
