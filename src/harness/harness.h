#ifndef WALLY_HARNESS_HARNESS_H
#define WALLY_HARNESS_HARNESS_H

#include <string>
#include <vector>

#include "account/console.h"
#include "account/credentials.h"

/// Launching a coding tool against a model, whether that model runs here or
/// upstream.
///
/// The harness never learns which it got. It is handed one OpenAI-compatible
/// base URL and talks to that, exactly as it would to any provider. For a local
/// model the URL is a server this process starts and stops; for an upstream one
/// it is the provider's own. That is the same shape Ollama uses, and it is why
/// a harness needs no plugin to work with us.
namespace wally::harness {

/// Where a model can be reached over HTTP, and whether we are serving it.
struct Endpoint {
    /// An OpenAI-compatible root, ending in `/v1`.
    std::string base_url;
    /// Empty for a local server, which ignores what is in the header.
    std::string api_key;
    /// The control plane the session belongs to (`credentials.console_url`,
    /// base path included), where a request started against `base_url` can
    /// be cancelled by name. Empty for a local server: nothing to cancel.
    std::string console_url;
    /// True when `Resolve` started a server that `Release` has to stop.
    bool serving = false;
};

/// Whether `id` is safe to carry into a live editor/agent session: forwarded
/// into HTTP request bodies, environment variables, and — for the JetBrains
/// wiring — an XML settings file built by plain string concatenation with no
/// escaping. Empty, over-long, control-character, and structurally dangerous
/// (`< > " ' & / \`) ids are rejected; the last four have no legitimate local
/// or upstream model name anyway (`LocalModels()` only ever yields a bare
/// directory name). Exposed so the contract can be tested directly.
bool ModelIdIsSafe(const std::string& id);

/// Confirms a cloud session is real before it is used to route a live editor
/// or agent session: refreshes an expired token first (the same dance `wally
/// usage` uses), then calls the console's identity endpoint the way `wally
/// whoami` does. A non-empty `access_token` alone — `Credentials::signed_in()`
/// — proves nothing: it is a local, offline check that a hand-written
/// credentials.json satisfies trivially.
///
/// On success `credentials` holds the (possibly refreshed, and already saved)
/// session and `email` names who it verified as. `error` is set on failure.
/// Exposed so the contract can be tested without a real console.
bool VerifyCloudSession(const account::ConsoleClient& console, account::Credentials* credentials,
                        std::string* email, std::string* error);

/// Points `endpoint` at `model`, starting a local server when the model is on
/// this machine and confirming the signed-in console session for real —
/// `VerifyCloudSession`, not just `Credentials::signed_in()` — when it is not.
///
/// `preferred_port` asks the local server for one particular port, and is
/// ignored when something else already holds it or the model is upstream.
/// An integration that writes the port into a file the tool reads at startup
/// wants this: the same port every run is what keeps that file true.
///
/// Returns false having already explained why not: an unknown model, an
/// unsafe model id, a framework the local server cannot load, or an upstream
/// model with nobody signed in (or a session that does not check out against
/// the console). Every integration needs this same answer, so it is separate
/// from launching anything — and callers that go on to do something
/// destructive (quitting a running editor) must not do it until this returns
/// true.
bool Resolve(const std::string& model, Endpoint* endpoint, int preferred_port = 0);

/// Stops whatever `Resolve` started. Safe on an endpoint it did not serve.
void Release(const Endpoint& endpoint);

/// Runs `tool` against `model`, forwarding `args` to it, and returns the tool's
/// exit code. Blocks until the tool exits, then stops anything it started.
///
/// An empty `model` uses whatever the tool is already configured for, which
/// makes `wally opencode` a plain passthrough.
int Launch(const std::string& tool, const std::string& model,
           const std::vector<std::string>& args);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_HARNESS_H
