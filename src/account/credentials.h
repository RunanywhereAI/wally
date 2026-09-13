#ifndef WALLY_ACCOUNT_CREDENTIALS_H
#define WALLY_ACCOUNT_CREDENTIALS_H

#include <string>
#include <vector>

namespace wally::account {

struct Credentials {
    std::string console_url;
    std::string email;
    std::string access_token;
    std::string refresh_token;
    long long expires_at = 0;

    bool signed_in() const { return !access_token.empty(); }
    bool access_token_expired(long long now, long long skew_seconds = 60) const {
        return expires_at > 0 && expires_at <= now + skew_seconds;
    }
};

/// The console API used when neither a login flag nor WALLY_CONSOLE_URL is set.
/// This is the control plane — `/v1/auth/cli/*`, `/v1/auth/me`, `/v1/cli/*` — and it is
/// not the host a person approves a sign-in on. See `TrustedBrowserOrigin`.
std::string DefaultConsoleUrl();

/// The browser origins allowed to host the approval page for `console_url`.
///
/// Never empty, which is the point. The version this replaces read one
/// environment variable and returned an empty string when it was unset — the
/// shipped case — so the origin check ran against nothing and the approval URL
/// the server sent was taken on trust.
///
/// Three answers, in order. `WALLY_CONSOLE_WEB_URL` alone when an operator
/// declared one. The deployed console's origins when `console_url` is the
/// deployed API, because those are different hosts from the API and demanding
/// the API's own origin refuses every real sign-in. Otherwise `console_url`
/// itself, so a console we know nothing about is trusted only at its own
/// origin.
std::vector<std::string> TrustedBrowserOrigins(const std::string& console_url);

/// Whether `url` sits on any of `origins`. Same origin rule as
/// `BrowserUrlMatchesConsole`, which is what it defers to.
bool BrowserUrlIsTrusted(const std::string& url, const std::vector<std::string>& origins);

/// Validate and canonicalize a console origin. HTTPS is required except for an
/// exact loopback host. Origins may not contain credentials, paths or queries.
bool NormalizeConsoleUrl(const std::string& input, std::string* normalized, std::string* error);

/// Browser URLs may include a path/query but obey the same HTTPS/loopback rule.
bool BrowserUrlIsSafe(const std::string& url);

/// Approval links must stay on the configured console origin so a compromised
/// response cannot silently open an unrelated sign-in page.
bool BrowserUrlMatchesConsole(const std::string& url, const std::string& console_url);

/// Bearer/refresh tokens are constrained to RFC 6750 b64token characters so
/// values loaded from disk or returned by a server cannot inject HTTP headers.
bool SessionTokenIsSafe(const std::string& token);

std::string ProfileDirectory();
std::string CredentialsPath();

/// Missing credentials are not an error; `out` receives the production default.
bool Load(Credentials* out, std::string* error);
// Compatibility overload for the editor/harness code introduced by PR #34.
// Errors are surfaced by the command-level pointer overload; callers using
// this legacy form receive an empty session on read failure.
Credentials Load();
bool Save(const Credentials& credentials, std::string* error);
bool Clear(std::string* error);

}  // namespace wally::account

#endif  // WALLY_ACCOUNT_CREDENTIALS_H
