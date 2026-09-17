#include "desktop/claude_profile.h"

#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

#include <nlohmann/json.hpp>

namespace wally::desktop {
namespace {

using Json = nlohmann::json;
namespace fs = std::filesystem;

/// Our profile's identity inside Claude Desktop's library. Fixed, so a second
/// run replaces the first rather than stacking entries up.
///
/// Shaped like the app's own ids; the tail is "RunAny" in hex, which makes it
/// recognisable in a file somebody is reading by hand.
constexpr const char* kProfileID = "00000000-0000-4000-8000-52756e416e79";

std::string Home() {
    const char* home = std::getenv("HOME");
    return home != nullptr ? home : std::string();
}

std::string SupportRoot(bool third_party) {
    const std::string home = Home();
    if (home.empty()) {
        return {};
    }
    return home + "/Library/Application Support/" + (third_party ? "Claude-3p" : "Claude");
}

/// Reads a JSON object, treating "not there" and "empty" as an empty object.
///
/// A malformed file is reported rather than overwritten: it is the reader's
/// Claude Desktop configuration, and silently replacing it would lose whatever
/// else they had in there.
bool ReadObject(const std::string& path, Json* out, std::string* error) {
    *out = Json::object();
    std::ifstream file(path);
    if (!file.good()) {
        return true;
    }
    const std::string text((std::istreambuf_iterator<char>(file)),
                           std::istreambuf_iterator<char>());
    if (text.find_first_not_of(" \t\r\n") == std::string::npos) {
        return true;
    }
    try {
        Json parsed = Json::parse(text);
        if (parsed.is_object()) {
            *out = std::move(parsed);
        }
        return true;
    } catch (const Json::exception& failure) {
        if (error != nullptr) {
            *error = "could not read " + path + ": " + failure.what();
        }
        return false;
    }
}

bool WriteObject(const std::string& path, const Json& value, std::string* error) {
    std::error_code code;
    fs::create_directories(fs::path(path).parent_path(), code);
    std::ofstream file(path, std::ios::trunc);
    if (!file.good()) {
        if (error != nullptr) {
            *error = "could not write " + path;
        }
        return false;
    }
    file << value.dump(2) << "\n";
    // Closed before it is judged. A short write on a full disk surfaces during
    // the flush that close() performs, so testing while the stream is still
    // open reports success and leaves the reader a truncated profile.
    file.close();
    if (!file.good() && error != nullptr) {
        *error = "could not write " + path;
    }
    return file.good();
}

bool SetDeploymentMode(const std::string& path, const std::string& mode, std::string* error) {
    Json config;
    if (!ReadObject(path, &config, error)) {
        return false;
    }
    config["deploymentMode"] = mode;
    return WriteObject(path, config, error);
}

std::string ProfilePath() { return SupportRoot(true) + "/configLibrary/" + kProfileID + ".json"; }
std::string MetaPath() { return SupportRoot(true) + "/configLibrary/_meta.json"; }

}  // namespace

std::string ProfileDirectory() { return SupportRoot(true); }

bool ApplyGateway(const std::string& base_url, const std::string& api_key,
                  const std::string& advertised, const std::string& label,
                  const std::string& display_name, std::string* error) {
    if (Home().empty()) {
        if (error != nullptr) {
            *error = "no home directory to write the profile into";
        }
        return false;
    }

    Json profile;
    if (!ReadObject(ProfilePath(), &profile, error)) {
        return false;
    }
    profile["inferenceProvider"] = "gateway";
    profile["inferenceGatewayBaseUrl"] = base_url;
    profile["inferenceGatewayApiKey"] = api_key;
    profile["inferenceGatewayAuthScheme"] = "bearer";
    profile["deploymentDisplayName"] = display_name;
    // Skip the first-run deployment-mode chooser: the gateway is already wired,
    // so a fresh install should go straight in rather than open the wizard.
    profile["disableDeploymentModeChooser"] = true;
    profile["chatTabEnabled"] = true;
    // Cowork reaches plugins and MCP servers over the network, and a profile
    // that does not say so leaves it unable to use them.
    profile["coworkEgressAllowedHosts"] = Json::array({"*"});
    profile["autoModeEnabled"] = false;
    // Cowork is the surface being asked for, and the app disables surfaces it
    // is not told to keep.
    profile["coworkTabEnabled"] = true;
    // Named rather than discovered. Discovery works when a gateway advertises
    // a model the app recognises as usable; ours advertises one id and the app
    // answers "Gateway returned no usable models", which is exactly the case
    // its own error message says to solve by listing the model here. A plain
    // id string is a valid entry, and the first entry is the default.
    // `name` has to be an id the app can map onto an Anthropic family or it
    // drops the entry: "expected a gateway model that maps to an Anthropic
    // model". `labelOverride` is what the picker actually shows, so the row
    // names the model that really answers rather than the one we route under.
    profile["inferenceModels"] =
        Json::array({Json{{"name", advertised}, {"labelOverride", label}}});
    // Asked for explicitly: with inferenceModels set the app would otherwise
    // skip discovery, and the picker then has nothing to reconcile the served
    // model against.
    profile["modelDiscoveryEnabled"] = true;
    if (!WriteObject(ProfilePath(), profile, error)) {
        return false;
    }

    Json meta;
    if (!ReadObject(MetaPath(), &meta, error)) {
        return false;
    }
    meta["appliedId"] = kProfileID;
    Json entries = Json::array();
    if (meta.contains("entries") && meta["entries"].is_array()) {
        for (const Json& entry : meta["entries"]) {
            // Drop any previous version of ours; keep everybody else's.
            if (entry.is_object() && entry.value("id", std::string()) == kProfileID) {
                continue;
            }
            entries.push_back(entry);
        }
    }
    entries.push_back(Json{{"id", kProfileID}, {"name", display_name}});
    meta["entries"] = std::move(entries);
    if (!WriteObject(MetaPath(), meta, error)) {
        return false;
    }

    // Both trees: the app reads the normal one to decide which mode it is in.
    return SetDeploymentMode(SupportRoot(true) + "/claude_desktop_config.json", "3p", error) &&
           SetDeploymentMode(SupportRoot(false) + "/claude_desktop_config.json", "3p", error);
}

bool RestoreGateway(std::string* error) {
    if (Home().empty()) {
        return true;
    }
    if (!SetDeploymentMode(SupportRoot(false) + "/claude_desktop_config.json", "1p", error) ||
        !SetDeploymentMode(SupportRoot(true) + "/claude_desktop_config.json", "1p", error)) {
        return false;
    }

    Json meta;
    if (!ReadObject(MetaPath(), &meta, error)) {
        return false;
    }
    if (!meta.empty()) {
        if (meta.value("appliedId", std::string()) == kProfileID) {
            meta.erase("appliedId");
        }
        if (meta.contains("entries") && meta["entries"].is_array()) {
            Json entries = Json::array();
            for (const Json& entry : meta["entries"]) {
                if (entry.is_object() && entry.value("id", std::string()) == kProfileID) {
                    continue;
                }
                entries.push_back(entry);
            }
            meta["entries"] = std::move(entries);
        }
        if (!WriteObject(MetaPath(), meta, error)) {
            return false;
        }
    }

    Json profile;
    if (!ReadObject(ProfilePath(), &profile, error)) {
        return false;
    }
    if (profile.empty()) {
        return true;
    }
    for (const char* key :
         {"inferenceProvider", "inferenceGatewayBaseUrl", "inferenceGatewayApiKey",
          "inferenceGatewayAuthScheme", "deploymentDisplayName", "inferenceModels",
          "coworkEgressAllowedHosts", "autoModeEnabled", "coworkTabEnabled",
          "modelDiscoveryEnabled"}) {
        profile.erase(key);
    }
    return WriteObject(ProfilePath(), profile, error);
}

bool GatewayApplied() {
    Json meta;
    if (!ReadObject(MetaPath(), &meta, nullptr)) {
        return false;
    }
    return meta.value("appliedId", std::string()) == kProfileID;
}

}  // namespace wally::desktop
