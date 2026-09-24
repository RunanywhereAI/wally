//! On-disk cache of the hosted model ids the console advertises (port of
//! src/account/model_cache.cpp). Read paths never touch the network; the
//! refresh is best-effort. Validation fails open. Lives in the profile dir.
//!

use std::io::Write;

use super::credentials::Credentials;

const FILE_NAME: &str = "models.json";
const MODELS_KEY: &str = "models";
const FETCHED_AT_KEY: &str = "fetched_at";

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
    let temp = parent.join(format!("{FILE_NAME}.tmp"));
    match std::fs::File::create(&temp) {
        Ok(mut file) => {
            if file.write_all(text.as_bytes()).is_err() {
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
