/**
 * @file model_cache.h
 * @brief On-disk cache of the hosted model ids the console advertises.
 *
 * A harness launch validates a `-m <id>` against this cache so a typo is caught
 * before the tool starts, without a network round-trip. The read paths never
 * touch the network and never block; the refresh is the only thing that does,
 * and it is best-effort and fire-and-forget. Validation fails open: an empty
 * cache (never refreshed, or offline) blocks nothing.
 *
 * Lives in the profile dir beside the credential and preferences files, so it
 * follows `WALLY_PROFILE_DIR`.
 */

#ifndef WALLY_ACCOUNT_MODEL_CACHE_H
#define WALLY_ACCOUNT_MODEL_CACHE_H

#include <cstdint>
#include <string>
#include <vector>

#include "account/credentials.h"

namespace wally::account {

/// {ProfileDirectory}/models.json, or empty when no home resolves.
std::string ModelCachePath();

/// The hosted model ids last written to the cache. Empty when the cache is
/// missing, unreadable, or was never refreshed.
std::vector<std::string> CachedModelIds();

/// Whether the cache holds any ids. False is the "cannot validate, fail open"
/// signal for the launch path.
bool CacheHasModels();

/// Whether `id` is one the console advertised as of the last refresh.
bool ModelIsCached(const std::string& id);

/// Fetch the catalog now, with `credentials` already in hand, and overwrite the
/// cache. Blocking, best-effort: a failure (offline, expired token) leaves the
/// old cache untouched and is not reported. Use right after `wally login`.
void RefreshModelCache(const Credentials& credentials);

/// Fetch the catalog now with `credentials` and overwrite the cache, reporting
/// the outcome. Blocking. Returns true on success (cache written). On false,
/// `*busy` is set true when the console was rate-limited or unreachable (a "try
/// again" case) rather than a hard failure. `busy` may be null.
bool RefreshModelCacheNow(const Credentials& credentials, bool* busy);

/// If the cache is older than `ttl_seconds` (or absent), refresh it on a
/// detached thread and return immediately. Never blocks, never reports. A
/// no-op when the cache is fresh or no session is stored.
void RefreshModelCacheIfStale(std::int64_t ttl_seconds);

/// Delete the cache file. The catalog is account-scoped, so `wally logout`
/// clears it along with the credential. A no-op when there is nothing to remove.
void ClearModelCache();

/// The launch-path staleness threshold: a day. Exposed so callers share it.
constexpr std::int64_t kModelCacheTtlSeconds = 24LL * 60 * 60;

}  // namespace wally::account

#endif  // WALLY_ACCOUNT_MODEL_CACHE_H
