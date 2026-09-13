#ifndef WALLY_ACCOUNT_CONSOLE_H
#define WALLY_ACCOUNT_CONSOLE_H

#include <cstdint>

#include <functional>
#include <map>
#include <string>
#include <vector>

namespace wally::account {

struct HttpRequest {
    std::string method;
    std::string url;
    std::string body;
    std::string bearer_token;
    // Total time the call may take, in milliseconds; 0 keeps the transport's
    // defaults (10 s to connect, 30 s in all). A fire-and-forget call such as a
    // cancel sets it small, so a wrapper that is exiting is never held longer
    // than the call is worth.
    int timeout_ms = 0;
};

struct HttpResponse {
    int status = 0;
    std::string body;
    // Response headers, keys lowercased so a lookup does not have to guess the
    // server's casing. Populated by the real transport; a mock may leave it
    // empty.
    std::map<std::string, std::string> headers;

    // The value of a `Retry-After` header as whole seconds, or -1 when the
    // header is absent or not a plain delay. The hosted API answers overload
    // with 429 and this header; a client is expected to wait rather than retry
    // at once. Only the delta-seconds form is honored: an HTTP-date Retry-After
    // is valid but never sent by this API, so parsing one would be dead code.
    int retry_after_seconds() const;
};

using Transport = std::function<bool(const HttpRequest&, HttpResponse*, std::string*)>;

/// What the control plane said to a cancel: Cancelled (202, a node ended it),
/// NotFound (404: unknown, finished, or not this key's -- the server does not
/// say which, by design), or Failed (anything else, including no reply).
enum class CancelOutcome { Cancelled, NotFound, Failed };

struct Identity {
    std::string email;
    // Kept for the existing editor/proxy integrations from the parent PR.
    std::string plan;
    std::int64_t tokens_this_month = 0;
    std::int64_t monthly_token_limit = 0;
};

/// What the console meters. Money is integer micro-dollars everywhere: one
/// dollar is 1,000,000, and a single request routinely costs a few hundred.
struct UsageTotals {
    std::int64_t requests = 0;
    std::int64_t prompt_tokens = 0;
    std::int64_t completion_tokens = 0;
    std::int64_t cached_tokens = 0;
    std::int64_t cost_micros = 0;
};

struct UsageDay {
    std::string date;
    std::int64_t requests = 0;
    std::int64_t prompt_tokens = 0;
    std::int64_t completion_tokens = 0;
    std::int64_t cost_micros = 0;
};

struct UsageEvent {
    std::string request_id;
    std::string model;
    std::string harness;
    std::string started_at;
    std::string error_code;
    std::int64_t prompt_tokens = 0;
    std::int64_t completion_tokens = 0;
    std::int64_t cached_tokens = 0;
    std::int64_t cost_micros = 0;
    std::int64_t ttft_ms = 0;
    int status_code = 0;
};

struct UsageModel {
    std::string model;
    std::int64_t requests = 0;
    std::int64_t prompt_tokens = 0;
    std::int64_t completion_tokens = 0;
    std::int64_t cached_tokens = 0;
    std::int64_t cost_micros = 0;
};

struct Credits {
    std::int64_t balance_micros = 0;
    std::int64_t granted_micros = 0;
    std::int64_t spent_micros = 0;
};

/// Spend over a window ending now, totalled by the console.
///
/// Not derivable here. `timeline` is grouped by calendar date, so its finest
/// grain is a day, and summing the recent-request page instead would describe
/// the last N requests while claiming to describe the window — the two part
/// company the moment anyone is busy.
struct UsageWindow {
    /// "1h" or "24h" as the console labels it.
    std::string window;
    /// The span. Carried so nothing here has to parse `window`.
    std::int64_t seconds = 0;
    UsageTotals totals;
};

struct Usage {
    Credits credit;
    UsageTotals totals;
    /// Empty against a console that predates windowed totals, which is every
    /// deployed one until `/v1/cli/usage` ships. Callers render what is missing
    /// as missing rather than substituting a wider window's numbers.
    std::vector<UsageWindow> windows;
    std::vector<UsageDay> timeline;
    std::vector<UsageModel> models;
    std::vector<UsageEvent> events;
};

struct UsageQuery {
    int days = 30;
    std::string model;
    int limit = 20;
};

struct Authorization {
    std::string request_code;
    std::string poll_secret;
    std::string verification_url;
    int expires_in = 0;
    int interval = 2;
};

struct Grant {
    std::string access_token;
    std::string refresh_token;
    std::string email;
    std::string plan;
    long expires_in = 0;
};

enum class PollResult { Pending, Approved, Denied, Expired, Failed };
enum class IdentityResult { Ok, Unauthorized, Failed };

/// One served model as `/v1/models` advertises it. `context_window` is the
/// input-token ceiling a coding agent reads to decide when to compact; 0 means
/// the deployment declared none. `max_output_tokens` is 0 for self-hosted
/// shared-budget models and non-zero only where a separate cap exists.
struct ModelInfo {
    std::string id;
    std::int64_t context_window = 0;
    std::int64_t max_output_tokens = 0;
};

/// One model's price, straight from the catalog the credit gate charges against.
/// Micro-dollars per million tokens (1 USD = 1,000,000 micros).
struct CatalogPrice {
    std::string id;
    std::int64_t input_per_mtok = 0;
    std::int64_t output_per_mtok = 0;
};

/// Console client independent of SDK/bootstrap state.
///
/// The default transport uses WinHTTP on Windows and libcurl elsewhere. Tests
/// inject a hermetic transport so auth contracts never need an account or network.
class ConsoleClient {
   public:
    explicit ConsoleClient(Transport transport = {});

    bool BeginAuthorization(const std::string& console_url, const std::string& hostname,
                            Authorization* authorization, std::string* error) const;
    PollResult Poll(const std::string& console_url, const Authorization& authorization,
                    Grant* grant, std::string* error) const;
    bool Refresh(const std::string& console_url, const std::string& refresh_token, Grant* grant,
                 std::string* error) const;
    IdentityResult WhoAmI(const std::string& console_url, const std::string& access_token,
                          Identity* identity, std::string* error) const;
    bool Revoke(const std::string& console_url, const std::string& access_token,
                const std::string& refresh_token, std::string* error) const;
    IdentityResult FetchUsage(const std::string& console_url, const std::string& access_token,
                              const UsageQuery& query, Usage* usage, std::string* error) const;
    /// The served model catalog from `/v1/models`, used to feed a harness the
    /// real context window (so its auto-compaction fires at the right point).
    IdentityResult FetchModels(const std::string& console_url, const std::string& access_token,
                               std::vector<ModelInfo>* models, std::string* error) const;
    /// Per-model pricing from `/v1/models/catalog`, so a harness can show real
    /// spend instead of $0.00.
    IdentityResult FetchCatalog(const std::string& console_url, const std::string& access_token,
                                std::vector<CatalogPrice>* prices, std::string* error) const;

    /// Ask the control plane to end an in-flight request this session's key
    /// started (`POST /v1/requests/{request_id}/cancel`, InferenceInfra #440).
    /// `request_id` is the `x-request-id` the response being abandoned carried.
    /// Fire-and-forget by nature: the caller has already dropped the stream,
    /// so a short `timeout_ms` bounds how long an exiting wrapper waits.
    CancelOutcome CancelRequest(const std::string& console_url, const std::string& access_token,
                                const std::string& request_id, int timeout_ms,
                                std::string* error) const;

   private:
    Transport transport_;
};

// Compatibility facade for the editor/harness code introduced by PR #34.
// New code should prefer ConsoleClient so transports can be injected in tests.
bool BeginAuthorization(const std::string& console_url, const std::string& hostname,
                        Authorization* authorization, std::string* error);
PollResult Poll(const std::string& console_url, const Authorization& authorization, Grant* grant,
                std::string* error);
bool Refresh(const std::string& console_url, const std::string& refresh_token, Grant* grant,
             std::string* error);
bool WhoAmI(const std::string& console_url, const std::string& token, Identity* identity,
            std::string* error);

}  // namespace wally::account

#endif  // WALLY_ACCOUNT_CONSOLE_H
