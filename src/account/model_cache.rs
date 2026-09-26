//! On-disk cache of the hosted model ids the console advertises (port of
//! src/account/model_cache.cpp). Read paths never touch the network; the
//! refresh is best-effort. Validation fails open. Lives in the profile dir.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use super::credentials::Credentials;

const FILE_NAME: &str = "models.json";
const MODELS_KEY: &str = "models";
const FETCHED_AT_KEY: &str = "fetched_at";

// Disambiguates the per-call temp file name below; only needs to be unique
// within this process, since the pid already disambiguates across processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn read_document() -> serde_json::Value {
    let path = model_cache_path();
    if path.is_empty() {
        return serde_json::Value::Object(serde_json::Map::new());
    }
    let Ok(bytes) = std::fs::read(&path) else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Atomic replace: a sibling temp then a rename, so a reader (or a launch
/// racing a refresh) never sees a half-written file, and a killed write leaves
/// the previous cache intact.
fn write_cache(ids: &[String]) {
    let path = model_cache_path();
    if path.is_empty() {
        return;
    }
    let target = std::path::Path::new(&path);
    let Some(parent) = target.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }

    let mut document = serde_json::Map::new();
    document.insert(
        FETCHED_AT_KEY.to_string(),
        serde_json::Value::from(now_seconds()),
    );
    document.insert(
        MODELS_KEY.to_string(),
        serde_json::Value::Array(
            ids.iter()
                .map(|id| serde_json::Value::String(id.clone()))
                .collect(),
        ),
    );
    let text = crate::io::json::dump_pretty(&serde_json::Value::Object(document), 2) + "\n";

    // C++ WriteCache only flushes the ofstream and checks `out.good()` -- no
    // fsync/fdatasync at all -- so a write succeeds here as soon as the
    // buffered write/flush succeeds. Calling sync_all() would fail (and skip
    // the rename, leaving the old cache stale) on a filesystem where fsync is
    // unsupported even though plain writes succeed, a failure mode the C++
    // side can never hit because it never asks the kernel to fsync.
    // Unique per call (pid + a monotonic counter), not a shared name: two
    // concurrent refreshes must never open the same temp file, or one writer's
    // create() can truncate the other's still-being-written contents out from
    // under it. `create_new` makes a name collision fail loudly instead of
    // silently truncating.
    let temp = parent.join(format!(
        "{FILE_NAME}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
    {
        Ok(mut file) => {
            if file.write_all(text.as_bytes()).is_err() {
                let _ = std::fs::remove_file(&temp);
                return;
            }
        }
        Err(_) => return,
    }
    if std::fs::rename(&temp, target).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

fn is_stale(ttl_seconds: i64) -> bool {
    let document = read_document();
    match document
        .get(FETCHED_AT_KEY)
        .and_then(|value| value.as_i64())
    {
        // Never written, or malformed.
        None => true,
        Some(fetched_at) => now_seconds() - fetched_at > ttl_seconds,
    }
}

/// {ProfileDirectory}/models.json, or empty when no home resolves.
pub fn model_cache_path() -> String {
    let directory = super::profile_directory();
    if directory.is_empty() {
        String::new()
    } else {
        format!("{directory}/{FILE_NAME}")
    }
}

/// The hosted model ids last written to the cache (empty when missing).
pub fn cached_model_ids() -> Vec<String> {
    let document = read_document();
    let Some(array) = document.get(MODELS_KEY).and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|entry| entry.as_str())
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
        .collect()
}

/// False is the "cannot validate, fail open" signal for the launch path.
pub fn cache_has_models() -> bool {
    !cached_model_ids().is_empty()
}

pub fn model_is_cached(id: &str) -> bool {
    cached_model_ids().iter().any(|cached| cached == id)
}

/// Fetch now and report. `Err(busy)`: busy is true when the console was
/// rate-limited or unreachable (a "try again" case) rather than a hard failure.
pub fn refresh_model_cache_now(credentials: &Credentials) -> Result<(), bool> {
    if !credentials.signed_in() {
        return Err(false);
    }
    let console = super::ConsoleClient::default();
    let (result, models, _error) =
        console.fetch_models(&credentials.console_url, &credentials.access_token);
    if result != super::IdentityResult::Ok {
        // Rate-limited or unreachable is a "try again", anything else a hard fail.
        return Err(result == super::IdentityResult::Unavailable);
    }
    let ids: Vec<String> = models
        .into_iter()
        .filter(|model| !model.id.is_empty())
        .map(|model| model.id)
        .collect();
    write_cache(&ids);
    Ok(())
}

/// Fetch the catalog now and overwrite the cache. Blocking, best-effort, silent.
pub fn refresh_model_cache(credentials: &Credentials) {
    let _ = refresh_model_cache_now(credentials); // best-effort, outcome ignored
}

/// If the cache is older than `ttl_seconds` (or absent), refresh it on a
/// detached thread and return immediately.
pub fn refresh_model_cache_if_stale(ttl_seconds: i64) {
    if !is_stale(ttl_seconds) {
        return;
    }
    // Detached and self-contained: it loads its own credentials and writes the
    // file atomically, so nothing it touches outlives its own stack. The
    // launch that spawned it blocks on the wrapped tool, so it has time to
    // finish; a fast-exiting caller just leaves the previous cache in place.
    // `spawn`'s handle is dropped without joining, the same as C++'s `.detach()`.
    std::thread::spawn(|| {
        if let Ok(credentials) = super::load() {
            if credentials.signed_in() {
                refresh_model_cache(&credentials);
            }
        }
    });
}

/// Delete the cache file. The catalog is account-scoped, so `wally logout`
/// clears it along with the credential. A no-op when there is nothing to remove.
pub fn clear_model_cache() {
    let path = model_cache_path();
    if path.is_empty() {
        return;
    }
    let _ = std::fs::remove_file(path);
}

/// The launch-path staleness threshold: a day.
pub const MODEL_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // Serializes every test in this module that touches WALLY_PROFILE_DIR, the
    // same pattern src/commands/cmd_account.rs and src/commands/cmd_update.rs
    // use for cargo test's shared-process env.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_profile_dir<T>(run: impl FnOnce(&std::path::Path) -> T) -> T {
        let _lock = env_lock();
        let dir = tempfile::tempdir().expect("temp dir");
        let saved = std::env::var_os("WALLY_PROFILE_DIR");
        // SAFETY: `_lock` serializes every test in this module that touches
        // WALLY_PROFILE_DIR.
        unsafe { std::env::set_var("WALLY_PROFILE_DIR", dir.path()) };
        let result = run(dir.path());
        match saved {
            // SAFETY: still under `_lock`.
            Some(value) => unsafe { std::env::set_var("WALLY_PROFILE_DIR", value) },
            None => unsafe { std::env::remove_var("WALLY_PROFILE_DIR") },
        }
        result
    }

    // #133 comment 11: two concurrent refreshes used to share one temp file
    // name, so one writer's create() could truncate the other's still-being-
    // written contents before either renamed onto the real cache. Each writer
    // now gets its own temp file, so a read never observes a spliced or
    // truncated document -- always exactly one writer's whole payload.
    #[test]
    fn concurrent_writers_never_produce_a_corrupt_cache() {
        with_profile_dir(|_dir| {
            let ids_a: Vec<String> = vec!["model-a".to_string()];
            let ids_b: Vec<String> = vec!["model-b1".to_string(), "model-b2".to_string()];

            let spawn_writer = |ids: Vec<String>| {
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        write_cache(&ids);
                    }
                })
            };
            let a = spawn_writer(ids_a.clone());
            let b = spawn_writer(ids_b.clone());
            a.join().expect("writer a");
            b.join().expect("writer b");

            let cached = cached_model_ids();
            assert!(
                cached == ids_a || cached == ids_b,
                "cache must be exactly one writer's whole payload, got {cached:?}"
            );
        });
    }

}
