#ifndef WALLY_IDE_OPENAI_PROXY_H
#define WALLY_IDE_OPENAI_PROXY_H

#include <string>

#include "harness/harness.h"

/// A loopback endpoint that carries the credential so the editor does not have
/// to.
///
/// An upstream model is reached through the signed-in console, which wants a
/// bearer token on every request. Handing that token to the IDE means putting
/// it in the IDE's credential store, and the IDE never reads what we write
/// there — the provider comes up with an empty key and the console answers 401.
///
/// So the token stays here. wally listens on loopback, adds the header, and
/// forwards. The IDE is configured exactly as it is for a local model, with no
/// key at all, which is the case already known to work. It also keeps the
/// reader's token out of a second store that neither of us controls.
namespace wally::ide {

struct Proxy {
    bool running = false;
    /// What to point the editor at. An OpenAI-compatible root ending in `/v1`.
    std::string base_url;
    /// Always empty for this proxy, and kept only so callers that wire a
    /// provider key through still compile.
    ///
    /// The session secret is the unguessable segment inside `base_url` instead.
    /// AI Assistant takes a provider key from its own settings dialog and
    /// nowhere else, so a header-based secret refused every request the proxy
    /// exists to serve; it does take a base URL, and it appends to whatever it
    /// is given.
    std::string auth_token;
};

/// Listens on `port` and forwards to `endpoint`, adding its credential.
///
/// `model` is the only model offered, and every request is answered by it
/// whatever it asked for. The console lists its provider's whole catalogue,
/// deprecated entries included, and an editor showing all of them invites a
/// choice that fails — which is how `models/gemini-2.5-pro`, retired for new
/// users, ended up being asked a question. wally was told which model to serve;
/// that is the one the editor gets.
///
/// Returns false having already said why. Nothing else is translated on the way
/// through: both sides speak OpenAI, so the bytes pass as they arrive.
bool StartProxy(const harness::Endpoint& endpoint, const std::string& model, int port,
                Proxy* proxy, bool verbose);

/// Stops the listener. Safe on a proxy that never started.
void StopProxy(Proxy* proxy);

}  // namespace wally::ide

#endif  // WALLY_IDE_OPENAI_PROXY_H
