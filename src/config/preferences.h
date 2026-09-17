/**
 * @file preferences.h
 * @brief Non-secret per-profile CLI preferences.
 *
 * Distinct from the credential store (`account::`), which is the secret file at
 * mode 0600. Preferences are plain configuration: what model a harness launch
 * should use when the reader gives no `-m`. They live in the same profile dir as
 * credentials so they follow `WALLY_PROFILE_DIR`, but in their own file.
 *
 * The only key today is the default model. Resolution precedence, highest first:
 *   1. an explicit model the reader passed
 *   2. the `WALLY_DEFAULT_MODEL` environment variable (a one-off shell override)
 *   3. `default_model` in the preferences file
 *   4. the id compiled into the binary, so a build always has a working default
 */

#ifndef WALLY_CONFIG_PREFERENCES_H
#define WALLY_CONFIG_PREFERENCES_H

#include <cstdint>
#include <optional>
#include <string>

namespace wally::prefs {

/// {ProfileDirectory}/preferences.json, or empty when $HOME is unresolvable.
std::string PreferencesPath();

/// Where the default model came from, so a command can tell the reader which
/// value is winning and why clearing the file might not change anything.
enum class DefaultModelSource : std::uint8_t {
    None,         ///< no default anywhere (only if no built-in id is compiled in)
    Environment,  ///< WALLY_DEFAULT_MODEL is set
    File,         ///< default_model in the preferences file
    BuiltIn,      ///< the id compiled into the binary
};

struct DefaultModel {
    std::string id;
    DefaultModelSource source = DefaultModelSource::None;
    bool set() const { return source != DefaultModelSource::None; }
};

/// The default model in effect: the env override if present, else the file.
/// A malformed file is treated as no default (a warning goes to stderr); it
/// never throws, so it cannot break a launch.
DefaultModel EffectiveDefaultModel();

/// The default model recorded in the file, ignoring the environment override.
/// This is what a `--clear` acts on and what "set" reports back.
std::optional<std::string> FileDefaultModel();

/// The model a launch should use given what the reader typed. Returns
/// `explicit_model` unchanged when it is non-empty, otherwise the effective
/// default, otherwise empty.
std::string ResolveModel(const std::string& explicit_model);

/// Persist `id` as the default model. Rejects an id that fails
/// `harness::ModelIdIsSafe`. Does not check any catalog: a hosted id such as
/// `glm-5.3-flash` is not in the local table, so real validity is left to the
/// launch path. Returns false with `*error` set on a bad id or a write failure.
bool SetDefaultModel(const std::string& id, std::string* error);

/// Remove the default model from the file. Succeeds when no default was set.
bool ClearDefaultModel(std::string* error);

}  // namespace wally::prefs

#endif  // WALLY_CONFIG_PREFERENCES_H
