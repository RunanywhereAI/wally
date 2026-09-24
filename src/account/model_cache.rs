//! On-disk cache of the hosted model ids the console advertises (port of
//! src/account/model_cache.cpp). Read paths never touch the network; the
//! refresh is best-effort. Validation fails open. Lives in the profile dir.
//! Owner: the account port.

use super::credentials::Credentials;

/// {ProfileDirectory}/models.json, or empty when no home resolves.
pub fn model_cache_path() -> String {
    todo!("account port: ModelCachePath")
}

/// The hosted model ids last written to the cache (empty when missing).
pub fn cached_model_ids() -> Vec<String> {
    todo!("account port: CachedModelIds")
}

/// False is the "cannot validate, fail open" signal for the launch path.
pub fn cache_has_models() -> bool {
    todo!("account port: CacheHasModels")
}

pub fn model_is_cached(id: &str) -> bool {
    todo!("account port: ModelIsCached ({id})")
}

/// Fetch the catalog now and overwrite the cache. Blocking, best-effort, silent.
pub fn refresh_model_cache(credentials: &Credentials) {
    let _ = credentials;
    todo!("account port: RefreshModelCache")
}

/// Fetch now and report. `Err(busy)`: busy is true when the console was
/// rate-limited or unreachable (a "try again" case) rather than a hard failure.
pub fn refresh_model_cache_now(credentials: &Credentials) -> Result<(), bool> {
    let _ = credentials;
    todo!("account port: RefreshModelCacheNow")
}

/// If the cache is older than `ttl_seconds` (or absent), refresh it on a
/// detached thread and return immediately.
pub fn refresh_model_cache_if_stale(ttl_seconds: i64) {
    todo!("account port: RefreshModelCacheIfStale ({ttl_seconds})")
}

/// Delete the cache file (logout clears it with the credential).
pub fn clear_model_cache() {
    todo!("account port: ClearModelCache")
}

/// The launch-path staleness threshold: a day.
pub const MODEL_CACHE_TTL_SECONDS: i64 = 24 * 60 * 60;
