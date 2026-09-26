#ifndef WALLY_ANTHROPIC_MESSAGES_H
#define WALLY_ANTHROPIC_MESSAGES_H

#include <string>
#include <utility>
#include <vector>

#include "harness/declared_harness.h"
#include "harness/harness.h"

/// An Anthropic-shaped front door onto an OpenAI-shaped model.
///
/// Claude Code, Claude Desktop and Cowork all talk the Anthropic Messages API
/// and are pointed elsewhere with ANTHROPIC_BASE_URL. Our server speaks
/// OpenAI: /v1/models, /v1/chat/completions, /health, and nothing else. The two
/// never meet, which is why `wally opencode` works today and `wally claude-code`
/// could not.
///
/// This is the translator between them. It serves POST /v1/messages on
/// loopback, rewrites each request into a chat completion, forwards it to
/// whichever endpoint `harness::Resolve` produced, and rewrites the reply back.
/// Streaming is translated event by event, because Claude Code streams and a
/// buffered answer would arrive as one block minutes later.
///
/// It lives here rather than in commons on purpose: it implements another
/// vendor's wire format, which is an integration detail of this CLI, not
/// inference logic every SDK consumer needs. If a second consumer ever wants
/// it, that is the moment to move it down a layer.
namespace wally::anthropic {

/// (Anthropic family name -> real model id) pairs. Claude Desktop's picker is
/// family-based, so each catalog model is advertised to it under a family name
/// and a request naming that family is routed back to the real id.
using ModelAliases = std::vector<std::pair<std::string, std::string>>;

/// A running translator.
struct Shim {
    /// Where Claude Code should be pointed. Empty when nothing started.
    std::string base_url;
    /// The value ANTHROPIC_AUTH_TOKEN should carry. Never the upstream key.
    std::string auth_token;
    bool running = false;
};

/// Starts a translator in front of `upstream` and fills `shim`.
///
/// `model` is the id sent on to the upstream endpoint, whatever name the
/// caller asked Claude Code for: Claude Code sends its own model strings, and
/// forwarding those to a local GGUF would ask for a model that is not there.
///
/// `declared` is the tool this translator was started for (Claude Code or
/// Claude Desktop). Every upstream request declares it in `X-RA-Harness` and
/// names it in the User-Agent (see harness/declared_harness.h), because the
/// endpoint otherwise sees only httplib's default agent string and attributes
/// the traffic to nobody.
///
/// Returns false having said why. The caller owns stopping it.
/// `verbose` narrates each request to stderr. Off by default: the body carries
/// the reader's prompt, and a translator that logs conversations unasked is one
/// nobody should point at their editor.
/// `advertised`, when set, is the id this gateway claims to serve. Requests
/// are still forwarded as `model`; only the name on the wire changes.
///
/// Claude Desktop drops any gateway model it cannot map to an Anthropic family,
/// so a gateway serving something else has to answer under a name it accepts.
/// The profile carries a labelOverride so the picker still shows what is really
/// answering.
/// `aliases`, when set, advertises each family name to the app and routes a
/// request naming one back to its real model id. Empty for the CLI path.
bool Start(const harness::Endpoint& upstream, const std::string& model,
           harness::DeclaredHarness declared, Shim* shim, bool verbose = false,
           const std::string& advertised = {}, const ModelAliases& aliases = {});

/// Stops the translator and waits for its thread. Safe on a stopped shim.
void Stop(Shim* shim);

}  // namespace wally::anthropic

#endif  // WALLY_ANTHROPIC_MESSAGES_H
