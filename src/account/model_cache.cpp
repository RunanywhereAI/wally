#include "account/model_cache.h"

#include <chrono>
#include <filesystem>
#include <fstream>
#include <string>
#include <system_error>
#include <thread>
#include <vector>

#include <nlohmann/json.hpp>

#include "account/console.h"

namespace wally::account {
namespace {

namespace fs = std::filesystem;
using Json = nlohmann::json;

constexpr const char* kFileName = "models.json";
constexpr const char* kModelsKey = "models";
constexpr const char* kFetchedAtKey = "fetched_at";

std::int64_t NowSeconds() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

Json ReadDocument() {
    const std::string path = ModelCachePath();
    if (path.empty()) {
        return Json::object();
    }
    std::ifstream file(path, std::ios::binary);
    if (!file.good()) {
        return Json::object();
    }
    Json parsed = Json::parse(file, nullptr, /*allow_exceptions=*/false);
    return parsed.is_object() ? parsed : Json::object();
}

/// Atomic replace: a sibling temp then a rename, so a reader (or a launch
/// racing a refresh) never sees a half-written file, and a killed write leaves
/// the previous cache intact.
void WriteCache(const std::vector<std::string>& ids) {
    const std::string path = ModelCachePath();
    if (path.empty()) {
        return;
    }
    const fs::path target(path);
    std::error_code ec;
    fs::create_directories(target.parent_path(), ec);
    if (ec) {
        return;
    }
    Json document;
    document[kFetchedAtKey] = NowSeconds();
    document[kModelsKey] = ids;

    const fs::path temp = target.parent_path() / (std::string(kFileName) + ".tmp");
    {
        std::ofstream out(temp, std::ios::binary | std::ios::trunc);
        if (!out.good()) {
            return;
        }
        out << document.dump(2) << '\n';
        out.flush();
        if (!out.good()) {
            return;
        }
    }
    fs::rename(temp, target, ec);
    if (ec) {
        fs::remove(temp, ec);
    }
}

bool IsStale(std::int64_t ttl_seconds) {
    const Json document = ReadDocument();
    const auto it = document.find(kFetchedAtKey);
    if (it == document.end() || !it->is_number_integer()) {
        return true;  // never written, or malformed
    }
    return NowSeconds() - it->get<std::int64_t>() > ttl_seconds;
}

}  // namespace

std::string ModelCachePath() {
    const std::string directory = ProfileDirectory();
    if (directory.empty()) {
        return {};
    }
    return (fs::path(directory) / kFileName).string();
}

std::vector<std::string> CachedModelIds() {
    const Json document = ReadDocument();
    const auto it = document.find(kModelsKey);
    if (it == document.end() || !it->is_array()) {
        return {};
    }
    std::vector<std::string> ids;
    for (const Json& entry : *it) {
        if (entry.is_string()) {
            std::string id = entry.get<std::string>();
            if (!id.empty()) {
                ids.push_back(std::move(id));
            }
        }
    }
    return ids;
}

bool CacheHasModels() {
    return !CachedModelIds().empty();
}

bool ModelIsCached(const std::string& id) {
    for (const std::string& cached : CachedModelIds()) {
        if (cached == id) {
            return true;
        }
    }
    return false;
}

bool RefreshModelCacheNow(const Credentials& credentials, bool* busy) {
    if (busy != nullptr) {
        *busy = false;
    }
    if (!credentials.signed_in()) {
        return false;
    }
    const ConsoleClient console;
    std::vector<ModelInfo> models;
    std::string error;
    const IdentityResult result =
        console.FetchModels(credentials.console_url, credentials.access_token, &models, &error);
    if (result != IdentityResult::Ok) {
        // Rate-limited or unreachable is a "try again", anything else a hard fail.
        if (busy != nullptr) {
            *busy = result == IdentityResult::Unavailable;
        }
        return false;
    }
    std::vector<std::string> ids;
    ids.reserve(models.size());
    for (const ModelInfo& model : models) {
        if (!model.id.empty()) {
            ids.push_back(model.id);
        }
    }
    WriteCache(ids);
    return true;
}

void RefreshModelCache(const Credentials& credentials) {
    RefreshModelCacheNow(credentials, nullptr);  // best-effort, outcome ignored
}

void ClearModelCache() {
    const std::string path = ModelCachePath();
    if (path.empty()) {
        return;
    }
    std::error_code ec;
    fs::remove(path, ec);
}

void RefreshModelCacheIfStale(std::int64_t ttl_seconds) {
    if (!IsStale(ttl_seconds)) {
        return;
    }
    // Detached and self-contained: it loads its own credentials and writes the
    // file atomically, so nothing it touches outlives its own stack. The launch
    // that spawned it blocks on the wrapped tool, so it has time to finish; a
    // fast-exiting caller just leaves the previous cache in place.
    std::thread([] {
        Credentials credentials;
        std::string error;
        if (Load(&credentials, &error) && credentials.signed_in()) {
            RefreshModelCache(credentials);
        }
    }).detach();
}

}  // namespace wally::account
