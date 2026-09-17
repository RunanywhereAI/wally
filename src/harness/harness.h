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
/// it is the provider's own. That standard shape is why a harness needs no
/// plugin to work with us.
namespace wally::harness {

/// Where a model can be reached over HTTP, and whether we are serving it.
struct Endpoint {
    /// An OpenAI-compatible root, ending in `/v1`.
    std::string base_url;
    /// Empty for a local server, which ignores what is in the header.
    std::string api_key;
    /// True when `Resolve` started a server that `Release` has to stop.
    bool serving = false;
};

/// Whether `id` is safe to carry into a live editor/agent session: forwarded
/// into HTTP request bodies, environment variables, and config files built by
/// plain string concatenation with no escaping. Empty, over-long, control-character, and structurally dangerous
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
/// `unverified` is set true when the session could not be CHECKED (the console
/// is rate limiting or down) rather than found bad. A caller holding a
/// signed-in session may proceed on it in that case.
bool VerifyCloudSession(const account::ConsoleClient& console, account::Credentials* credentials,
                        std::string* email, std::string* error, bool* unverified = nullptr);

/// Points `endpoint` at `model`, starting a local server when the model is on
/// this machine and confirming the signed-in console session for real —
/// `VerifyCloudSession`, not just `Credentials::signed_in()` — when it is not.
///
/// Returns false having already explained why not: an unknown model, an
/// unsafe model id, a framework the local server cannot load, or an upstream
/// model with nobody signed in (or a session that does not check out against
/// the console). Every integration needs this same answer, so it is separate
/// from launching anything — and callers that go on to do something
/// destructive (quitting a running editor) must not do it until this returns
/// true.
bool Resolve(const std::string& model, Endpoint* endpoint);

/// Stops whatever `Resolve` started. Safe on an endpoint it did not serve.
void Release(const Endpoint& endpoint);

/// True when the CLI `tool` is on PATH. When it is not, prints the clean
/// "not installed" message and its accurate install command, then returns
/// false — so a caller can stop before resolving a model or printing anything
/// else, which is the only thing a person without the tool needs to see.
bool EnsureInstalled(const std::string& tool);

/// Prints the one shared "cloud session is no longer valid" error, in red, that
/// every harness shows when a hosted `model` cannot be used because the session
/// failed verification. One phrasing, one place, so it reads the same whichever
/// harness a person launched.
void ReportCloudSessionInvalid(const std::string& model);

/// The one shared "you are not signed in" error, in red with `wally login`
/// highlighted, for when no session is stored at all (as opposed to an expired
/// one). Same look as ReportCloudSessionInvalid.
void ReportNotSignedIn();

/// Recovery for a `-m <model>` that is not in the cached catalog: refresh the
/// catalog live (with coloured progress the reader can follow), then re-check.
/// Returns true when the model is now known so the caller can go on and launch
/// it, false to stop — the console was busy, or the model is genuinely unknown,
/// and the message is already printed.
bool RefreshAndRecheckModel(const account::Credentials& credentials, const std::string& model);

/// Runs `tool` against `model`, forwarding `args` to it, and returns the tool's
/// exit code. Blocks until the tool exits, then stops anything it started.
///
/// An empty `model` uses whatever the tool is already configured for, which
/// makes `wally opencode` a plain passthrough.
int Launch(const std::string& tool, const std::string& model,
           const std::vector<std::string>& args);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_HARNESS_H
