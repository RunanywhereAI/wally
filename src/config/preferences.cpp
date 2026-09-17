#include "config/preferences.h"

#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <string>
#include <system_error>

#include <nlohmann/json.hpp>

#include "account/credentials.h"
#include "harness/harness.h"
#include "io/output.h"

// The built-in default model id, set at build time from the CMake cache variable
// WALLY_DEFAULT_MODEL_ID (defaults to glm-5.3-flash; override with
// -DWALLY_DEFAULT_MODEL_ID=<id>). The id lives in the build, not in this file.
// Empty means a build left it undefined, i.e. no built-in default.
#ifndef WALLY_DEFAULT_MODEL_ID
#define WALLY_DEFAULT_MODEL_ID ""
#endif

namespace wally::prefs {
namespace {

namespace fs = std::filesystem;
using Json = nlohmann::json;

constexpr const char* kFileName = "preferences.json";
constexpr const char* kDefaultModelKey = "default_model";

// The model a harness launch falls back to when the reader passes no -m and set
// no default (neither the env override nor the file). Its value is whatever the
// build baked into WALLY_DEFAULT_MODEL_ID above.
constexpr const char* kBuiltInDefaultModel = WALLY_DEFAULT_MODEL_ID;

std::string EnvValue(const char* name) {
    const char* value = std::getenv(name);
    return value == nullptr ? std::string() : std::string(value);
}

/// Reads and parses the preferences file. Returns an empty object for a missing
/// file, and — on a parse error — an empty object plus a stderr warning, so a
/// corrupt file degrades to "no preferences" rather than breaking a launch.
Json ReadFile() {
    const std::string path = PreferencesPath();
    if (path.empty()) {
        return Json::object();
    }
    std::ifstream file(path, std::ios::binary);
    if (!file.good()) {
        return Json::object();
    }
    Json parsed = Json::parse(file, nullptr, /*allow_exceptions=*/false);
    if (parsed.is_discarded() || !parsed.is_object()) {
        out::status_line("ignoring unreadable preferences file at " + path);
        return Json::object();
    }
    return parsed;
}

/// Writes `document` to the preferences file atomically: a sibling temp file
/// then a rename, so a reader never sees a half-written file.
bool WriteFile(const Json& document, std::string* error) {
    const std::string path = PreferencesPath();
    if (path.empty()) {
        if (error != nullptr) {
            *error = "cannot resolve a home directory for preferences";
        }
        return false;
    }

    const fs::path target(path);
    std::error_code ec;
    fs::create_directories(target.parent_path(), ec);
    if (ec) {
        if (error != nullptr) {
            *error = "cannot create " + target.parent_path().string() + ": " + ec.message();
        }
        return false;
    }

    const fs::path temp = target.parent_path() / (std::string(kFileName) + ".tmp");
    {
        std::ofstream out(temp, std::ios::binary | std::ios::trunc);
        if (!out.good()) {
            if (error != nullptr) {
                *error = "cannot write " + temp.string();
            }
            return false;
        }
        out << document.dump(2) << '\n';
        out.flush();
        if (!out.good()) {
            if (error != nullptr) {
                *error = "failed writing " + temp.string();
            }
            return false;
        }
    }

    fs::rename(temp, target, ec);
    if (ec) {
        fs::remove(temp, ec);
        if (error != nullptr) {
            *error = "cannot replace " + target.string() + ": " + ec.message();
        }
        return false;
    }
    return true;
}

}  // namespace

std::string PreferencesPath() {
    const std::string directory = account::ProfileDirectory();
    if (directory.empty()) {
        return {};
    }
    return (fs::path(directory) / kFileName).string();
}

std::optional<std::string> FileDefaultModel() {
    const Json document = ReadFile();
    const auto it = document.find(kDefaultModelKey);
    if (it == document.end() || !it->is_string()) {
        return std::nullopt;
    }
    const auto value = it->get<std::string>();
    if (value.empty()) {
        return std::nullopt;
    }
    return value;
}

DefaultModel EffectiveDefaultModel() {
    const std::string env = EnvValue("WALLY_DEFAULT_MODEL");
    if (!env.empty()) {
        return {env, DefaultModelSource::Environment};
    }
    if (const std::optional<std::string> file = FileDefaultModel()) {
        return {*file, DefaultModelSource::File};
    }
    if (kBuiltInDefaultModel[0] != '\0') {
        return {kBuiltInDefaultModel, DefaultModelSource::BuiltIn};
    }
    return {};
}

std::string ResolveModel(const std::string& explicit_model) {
    if (!explicit_model.empty()) {
        return explicit_model;
    }
    return EffectiveDefaultModel().id;
}

bool SetDefaultModel(const std::string& id, std::string* error) {
    if (!harness::ModelIdIsSafe(id)) {
        if (error != nullptr) {
            *error = "not a usable model id: " + id;
        }
        return false;
    }
    Json document = ReadFile();
    document[kDefaultModelKey] = id;
    return WriteFile(document, error);
}

bool ClearDefaultModel(std::string* error) {
    Json document = ReadFile();
    if (!document.contains(kDefaultModelKey)) {
        return true;
    }
    document.erase(kDefaultModelKey);
    return WriteFile(document, error);
}

}  // namespace wally::prefs
