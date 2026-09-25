#ifndef WALLY_HARNESS_OPENCODE_H
#define WALLY_HARNESS_OPENCODE_H

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

#include "account/console.h"
#include "harness/catalog_models.h"

namespace wally::harness {

using SpawnFunction =
    std::function<int(const std::string& executable, const std::vector<std::string>& arguments)>;

/// OpenCode's complete, ephemeral provider configuration for a local or hosted model.
/// Every model in `models` becomes a selectable entry with its real limits
/// (context/output) and price so OpenCode's compaction and usage display are
/// correct; a 0 for any of them omits that field. `primary` is the default
/// selection. Exposed so the contract can be tested without launching a child.
std::string BuildOpenCodeConfig(const std::string& primary, const std::string& base_url,
                                const std::string& api_key,
                                const std::vector<CatalogModel>& models);

/// Compatibility name for callers configuring a hosted catalog.
std::string BuildOpenCodeCloudConfig(const std::string& primary, const std::string& base_url,
                                     const std::string& access_token,
                                     const std::vector<CatalogModel>& models);

/// Launch OpenCode against the signed-in RunAnywhere cloud session.
///
/// Only OPENCODE_CONFIG_CONTENT is changed, only for the duration of the child.
/// No OpenCode or project configuration file is read or written. The default
/// overload starts `opencode` directly (never through a shell).
int LaunchOpenCodeCloud(const std::string& model, const std::vector<std::string>& arguments);

/// Test seam for the console refresh transport and child process.
int LaunchOpenCodeCloud(const std::string& model, const std::vector<std::string>& arguments,
                        const account::ConsoleClient& console, const SpawnFunction& spawn);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_OPENCODE_H
