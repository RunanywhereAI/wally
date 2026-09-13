#pragma once

/// The environment an editor launch hands to the wrapped tool.
///
/// Split out of `cmd_editors.cpp` so it can be tested. The wiring is the whole
/// of what `wally claude-code` does -- get one variable wrong and the tool talks
/// to the wrong endpoint, or reports the wrong model as the one that answered --
/// and it was previously a file-private helper that no test could reach.

#include <string>
#include <vector>

#include "anthropic/messages.h"

namespace wally::commands {

/// `open(1)` arguments that launch a macOS bundle against `shim`, carrying the
/// model as well as the endpoint.
///
/// `open --env` is the only way in: launchd starts a bundle from the reader's
/// login session, so nothing the parent process exports reaches it. Anything the
/// terminal path sets with a scoped environment variable has to be listed here
/// too, or a desktop launch silently runs unwired.
///
/// `model` empty means no wiring was asked for, and the model variables are then
/// omitted rather than exported empty -- an empty value reads as "unset this" to
/// the wrapped tool, which is not the same as leaving its own configuration
/// alone.
std::vector<std::string> OpenArgs(const std::string& bundle, const anthropic::Shim& shim,
                                  const std::vector<std::string>& passthrough,
                                  const std::string& model);

}  // namespace wally::commands
