//! Small process-wide helpers.

pub mod interrupt;
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

/// One lock for every unit test in this library that touches a real
/// environment variable another unit test also touches (HOME,
/// XDG_STATE_HOME, LOCALAPPDATA, USERPROFILE, WALLY_*, RUNANYWHERE_HOME,
/// HOMEBREW_PREFIX, PATH, TMPDIR/TMP/TEMP/TEMPDIR, ...). All of the `[lib]`
/// target's `#[cfg(test)]` modules compile into one test binary and run as
/// threads of one process, so a lock declared inside a single module only
/// serializes the tests in that module — a test in a different file that
/// sets the same variable can still interleave with it and observe (or
/// clobber) a value meant only for the test that set it. Declared once here,
/// crate-visible, so every env-touching unit test shares the same lock.
///
/// Its reach is that one binary. Each integration test under `tests/` is its
/// own binary and process, with its own locks, and every `[[bin]]` target in
/// Cargo.toml sets `test = false`, so none has unit tests to share it with.
#[cfg(test)]
pub(crate) mod env_lock {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Hold this for the whole body of any test that reads or writes a real
    /// environment variable. Recovers from a poisoned lock — one earlier env
    /// test panicking while it held this — instead of poisoning every later
    /// env test in the crate.
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Howard Hinnant's `civil_from_days` (public domain): the proleptic Gregorian
/// calendar date for `z` days since 1970-01-01, in UTC. Used instead of
/// gmtime_r/gmtime_s so formatting a timestamp has no platform-specific
/// calendar dependency.
///
/// Total over `i64`: the arithmetic runs in `i128`, where shifting the epoch
/// and scaling by eras cannot overflow, and a year is always smaller in
/// magnitude than the day count it came from, so it fits back in `i64`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = i128::from(z) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year as i64, m, d)
}

/// The inverse of `civil_from_days`: days since 1970-01-01 for a proleptic
/// Gregorian date. The caller supplies a real date (month 1-12, a day the
/// month has); `year` is limited to what a timestamp can carry.
pub fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400); // [0, 399]
    let mp = i64::from((month + 9) % 12); // March is 0
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// `seconds` since the epoch as `%Y-%m-%dT%H:%M:%SZ`. Every UTC timestamp
/// wally writes goes through here, so there is one calendar conversion to get
/// right. Callers decide what a non-positive clock means for them.
pub fn format_utc(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let secs_of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 2000-02-29 and the day after: the leap day a century rule keeps,
        // and the March 1 the algorithm's years start on.
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
        assert_eq!(civil_from_days(-135_080), (1600, 3, 1));
        assert_eq!(civil_from_days(2_932_896), (9999, 12, 31));
    }

    #[test]
    fn civil_from_days_holds_at_the_ends_of_i64() {
        // 2^63 days is about 2.5e16 years either way.
        let (year, month, day) = civil_from_days(i64::MAX);
        assert!(year > 25_000_000_000_000_000, "{year}");
        assert!((1..=12).contains(&month) && (1..=31).contains(&day));
        let (year, month, day) = civil_from_days(i64::MIN);
        assert!(year < -25_000_000_000_000_000, "{year}");
        assert!((1..=12).contains(&month) && (1..=31).contains(&day));
    }

    #[test]
    fn days_from_civil_inverts_civil_from_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 2, 29), 11016);
        assert_eq!(days_from_civil(9999, 12, 31), 2_932_896);
        for days in (-800_000..800_000).step_by(997) {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year as i32, month, day), days);
        }
    }

    #[test]
    fn format_utc_writes_rfc3339_zulu() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(-1), "1969-12-31T23:59:59Z");
        assert_eq!(format_utc(1_790_000_000), "2026-09-21T14:13:20Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn format_utc_never_panics() {
        for seconds in [i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1] {
            assert!(format_utc(seconds).ends_with('Z'), "{seconds}");
        }
    }

    // The whole point of env_lock is that every env-touching test in the
    // crate serializes on the *same* mutex, not just the tests in its own
    // module. This does not exercise another module's test, but it does
    // pin down the primitive they all now share: two threads racing to set
    // "the same variable" must never both be inside the critical section at
    // once.
    #[test]
    fn env_lock_serializes_concurrent_callers() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let busy = Arc::new(AtomicBool::new(false));
        let overlapped = Arc::new(AtomicBool::new(false));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let busy = Arc::clone(&busy);
                let overlapped = Arc::clone(&overlapped);
                std::thread::spawn(move || {
                    let _lock = env_lock::lock();
                    if busy.swap(true, Ordering::SeqCst) {
                        overlapped.store(true, Ordering::SeqCst);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    busy.store(false, Ordering::SeqCst);
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread should not panic");
        }
        assert!(
            !overlapped.load(Ordering::SeqCst),
            "env_lock() let two callers into the critical section at once"
        );
    }

    // A test elsewhere in the crate that panics while holding env_lock must
    // not take every later env-touching test down with it -- the crate has
    // dozens of them, spread across modules that no longer know about each
    // other's lock at all. Without `unwrap_or_else(|e| e.into_inner())` this
    // panics on the second call instead of recovering.
    #[test]
    fn env_lock_recovers_from_a_poisoned_guard() {
        let poisoned = std::panic::catch_unwind(|| {
            let _lock = env_lock::lock();
            panic!("simulated panic while holding env_lock");
        });
        assert!(poisoned.is_err());

        // Must return a guard, not panic or deadlock.
        let _lock = env_lock::lock();
    }
}
