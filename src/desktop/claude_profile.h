#ifndef WALLY_DESKTOP_CLAUDE_PROFILE_H
#define WALLY_DESKTOP_CLAUDE_PROFILE_H

#include <string>
#include <utility>
#include <vector>

/// Claude Desktop's third-party inference mode.
///
/// The app ships two deployment modes: "1p", which talks to Anthropic, and
/// "3p", which talks to a gateway you name. The 3p side keeps its own profile
/// tree beside the normal one, and none of it is reachable from Settings —
/// which is why the model picker and ANTHROPIC_BASE_URL both look like dead
/// ends. Neither is the mechanism.
///
/// A gateway here speaks the Anthropic Messages API, which is exactly what
/// `wally::anthropic` already serves. So pointing Claude Desktop at a model we
/// serve is a matter of writing the profile and restarting the app.
namespace wally::desktop {

/// Writes the gateway profile, marks it applied, and switches both config
/// trees to third-party mode.
///
/// Takes effect on the app's next launch, never on a running one. Returns
/// false with `error` set. `models` is (Anthropic family name -> real id) pairs,
/// the first the default: each becomes a picker entry whose `name` the app maps
/// to a family and whose `labelOverride` shows the real id.
bool ApplyGateway(const std::string& base_url, const std::string& api_key,
                  const std::vector<std::pair<std::string, std::string>>& models,
                  const std::string& display_name, std::string* error);

/// Puts Claude Desktop back on Anthropic and strips the keys we wrote.
///
/// Safe to call when nothing was applied, and it only removes our own profile:
/// a gateway somebody else configured is left alone.
bool RestoreGateway(std::string* error);

/// True when our profile is the one Claude Desktop has applied.
bool GatewayApplied();

/// Where the app keeps its third-party profiles, for a message worth printing.
std::string ProfileDirectory();

}  // namespace wally::desktop

#endif  // WALLY_DESKTOP_CLAUDE_PROFILE_H
