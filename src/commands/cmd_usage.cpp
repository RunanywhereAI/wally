#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <utility>

#include "account/console.h"
#include "account/credentials.h"
#include "cli_formatter.h"
#include "commands/commands.h"
#include "io/output.h"

namespace wally::commands {
namespace {

void fail(int status) {
    if (status != 0) {
        throw CLI::RuntimeError(status);
    }
}

long long EpochSeconds() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

/// Money is integer micro-dollars: one dollar is 1,000,000.
std::string Money(std::int64_t micros) {
    const int places = micros != 0 && micros < 1'000'000 ? 4 : 2;
    char text[48];
    std::snprintf(text, sizeof(text), "$%.*f", places, static_cast<double>(micros) / 1'000'000.0);
    return text;
}

std::string Grouped(std::int64_t value) {
    std::string digits = std::to_string(value < 0 ? -value : value);
    for (std::size_t at = digits.size(); at > 3;) {
        at -= 3;
        digits.insert(at, ",");
    }
    return value < 0 ? "-" + digits : digits;
}

bool RefreshSession(const account::ConsoleClient& client, account::Credentials* credentials,
                    std::string* error) {
    if (credentials->refresh_token.empty()) {
        if (error != nullptr) {
            *error = "the cloud session cannot be refreshed; run `wally account login`";
        }
        return false;
    }
    account::Grant grant;
    if (!client.Refresh(credentials->console_url, credentials->refresh_token, &grant, error)) {
        return false;
    }
    credentials->access_token = grant.access_token;
    if (!grant.refresh_token.empty()) {
        credentials->refresh_token = grant.refresh_token;
    }
    credentials->expires_at = EpochSeconds() + (grant.expires_in > 0 ? grant.expires_in : 3600);
    return account::Save(*credentials, error);
}

/// The two rows, in the order they are printed, keyed by the label the console
/// puts on the window it totalled.
constexpr struct {
    const char* id;
    const char* label;
} kRows[] = {{"1h", "past 1h"}, {"24h", "past 24h"}};

/// The window the console labelled `id`, or nullptr when it sent none.
///
/// A console deployed before windowed totals sends no `windows` at all. Nothing
/// substitutes for a missing one: `totals` covers `days`, so showing it under
/// an hour's heading would be a month's spend wearing an hour's label.
const account::UsageWindow* Window(const account::Usage& usage, const char* id) {
    for (const account::UsageWindow& window : usage.windows) {
        if (window.window == id) {
            return &window;
        }
    }
    return nullptr;
}

void PrintJson(const account::Usage& usage) {
    out::JsonWriter json;
    json.begin_object();
    json.field("balance_micros", static_cast<int64_t>(usage.credit.balance_micros));
    json.field("granted_micros", static_cast<int64_t>(usage.credit.granted_micros));
    json.field("spent_micros", static_cast<int64_t>(usage.credit.spent_micros));

    json.begin_array("windows");
    for (const auto& row : kRows) {
        const account::UsageWindow* window = Window(usage, row.id);
        json.begin_array_object();
        json.field("window", row.id);
        json.field("available", window != nullptr);
        if (window != nullptr) {
            json.field("input_tokens", static_cast<int64_t>(window->totals.prompt_tokens));
            json.field("output_tokens", static_cast<int64_t>(window->totals.completion_tokens));
            json.field("cached_tokens", static_cast<int64_t>(window->totals.cached_tokens));
            json.field("cost_micros", static_cast<int64_t>(window->totals.cost_micros));
        }
        json.end_object();
    }
    json.end_array();

    json.end_object();
    out::result_line(json.str());
}

void PrintReport(const account::Usage& usage) {
    char line[200];
    std::snprintf(line, sizeof(line), "credit     %s left of %s granted",
                  Money(usage.credit.balance_micros).c_str(),
                  Money(usage.credit.granted_micros).c_str());
    out::result_line(line);
    out::result_line("");

    std::snprintf(line, sizeof(line), "%-10s %11s %11s %11s %11s", "window", "input", "output",
                  "cache", "spend");
    out::result_line(line);

    bool missing = false;
    for (const auto& row : kRows) {
        const account::UsageWindow* window = Window(usage, row.id);
        if (window == nullptr) {
            missing = true;
            std::snprintf(line, sizeof(line), "%-10s %11s %11s %11s %11s", row.label, "-", "-", "-",
                          "-");
        } else {
            const account::UsageTotals& totals = window->totals;
            std::snprintf(line, sizeof(line), "%-10s %11s %11s %11s %11s", row.label,
                          Grouped(totals.prompt_tokens).c_str(),
                          Grouped(totals.completion_tokens).c_str(),
                          Grouped(totals.cached_tokens).c_str(),
                          Money(totals.cost_micros).c_str());
        }
        out::result_line(line);
    }

    if (missing) {
        out::result_line("");
        out::status_line("this console does not total by window yet; a dash is a number it did "
                         "not send, not a zero");
    }
}

int Usage(bool as_json) {
    account::Credentials credentials;
    std::string failure;
    if (!account::Load(&credentials, &failure)) {
        out::error_line(failure);
        return 1;
    }
    if (!credentials.signed_in()) {
        out::error_line("not signed in — run `wally account login`");
        return 1;
    }

    account::ConsoleClient client;
    if (credentials.access_token_expired(EpochSeconds()) &&
        !RefreshSession(client, &credentials, &failure)) {
        out::error_line(failure);
        return 1;
    }

    // One day back, and the shortest recent-request page the route accepts:
    // nothing here renders those, and `by_model`/`totals` group in SQL, so the
    // page size cannot move the numbers above.
    account::UsageQuery query;
    query.days = 1;
    query.limit = 1;

    account::Usage usage;
    account::IdentityResult result = client.FetchUsage(credentials.console_url,
                                                       credentials.access_token, query, &usage,
                                                       &failure);
    if (result == account::IdentityResult::Unauthorized) {
        std::string refresh_failure;
        if (!RefreshSession(client, &credentials, &refresh_failure)) {
            // Not "expired". The console answers unknown, revoked, expired and
            // malformed with the same 401 on purpose, so which of the four this
            // was is not something we know — and sending someone to re-login
            // over a revoked key wastes the trip.
            out::error_line("the console rejected this session (" + refresh_failure +
                            "); run `wally account login`");
            return 1;
        }
        usage = account::Usage{};
        result = client.FetchUsage(credentials.console_url, credentials.access_token, query, &usage,
                                   &failure);
    }
    if (result != account::IdentityResult::Ok) {
        out::error_line(failure);
        return 1;
    }

    if (as_json) {
        PrintJson(usage);
    } else {
        PrintReport(usage);
    }
    return 0;
}

/// The export's default window: one day back. A report of settled requests
/// names its window because the route has no default and a page of rows
/// without bounds would claim to describe a history it never covered.
constexpr int kRequestsWindowDays = 1;

/// A UTC instant `seconds` ago, formatted the way the route's `since`/`until`
/// read it: ISO-8601 with a timezone. No timezone means a 400 the person
/// cannot fix from its message, so the format is proven here and by test.
std::string WindowStartIso(int seconds_back, long long now_epoch) {
    return account::IsoTimestamp(now_epoch - seconds_back);
}

void PrintRequestsJson(const account::UsageRequestsPage& page) {
    out::JsonWriter json;
    json.begin_object();
    json.field("as_of", page.as_of);
    json.field("requests", static_cast<int64_t>(page.requests.size()));
    json.field("total_requests", static_cast<int64_t>(page.total_requests));
    json.field("prompt_tokens", static_cast<int64_t>(page.prompt_tokens));
    json.field("cached_tokens", static_cast<int64_t>(page.cached_tokens));
    json.field("noncached_prompt_tokens", static_cast<int64_t>(page.noncached_prompt_tokens));
    json.field("completion_tokens", static_cast<int64_t>(page.completion_tokens));
    json.field("reasoning_tokens", static_cast<int64_t>(page.reasoning_tokens));
    json.field("cost_micros", static_cast<int64_t>(page.cost_micros));

    json.begin_array("rows");
    for (const account::UsageRequestRow& row : page.requests) {
        json.begin_array_object();
        json.field("request_id", row.request_id);
        json.field("response_request_id", row.response_request_id);
        json.field("model", row.model);
        json.field("status_code", static_cast<int64_t>(row.status_code));
        json.field("error_code", row.error_code);
        json.field("finish_reason", row.finish_reason);
        json.field("stream", row.stream);
        json.field("ts_start", row.ts_start);
        json.field("prompt_tokens", static_cast<int64_t>(row.prompt_tokens));
        json.field("cached_tokens", static_cast<int64_t>(row.cached_tokens));
        json.field("completion_tokens", static_cast<int64_t>(row.completion_tokens));
        json.field("reasoning_tokens", static_cast<int64_t>(row.reasoning_tokens));
        json.field("cost_micros", static_cast<int64_t>(row.cost_micros));
        json.field("ttft_ms", static_cast<int64_t>(row.ttft_ms));
        json.field("pricing_version", row.pricing_version);
        json.end_object();
    }
    json.end_array();

    if (!page.next_cursor.empty()) {
        json.field("next_cursor", page.next_cursor);
    }
    json.end_object();
    out::result_line(json.str());
}

/// A row the terminal can draw: four numbers, a status and an id. Times are
/// shown as the ledger recorded them (UTC); nothing is reformatted, so what
/// the console wrote is what the reader sees.
void PrintRequestsReport(const account::UsageRequestsPage& page) {
    char line[220];
    std::snprintf(line, sizeof(line), "requests   %s of %s settled (as of %s)",
                  Grouped(static_cast<std::int64_t>(page.requests.size())).c_str(),
                  Grouped(page.total_requests).c_str(), page.as_of.c_str());
    out::result_line(line);
    std::snprintf(line, sizeof(line), "spend      %s over the window",
                  Money(page.cost_micros).c_str());
    out::result_line(line);
    std::snprintf(line, sizeof(line), "tokens     in %s (cache %s, fresh %s), out %s (reasoning %s)",
                  Grouped(page.prompt_tokens).c_str(), Grouped(page.cached_tokens).c_str(),
                  Grouped(page.noncached_prompt_tokens).c_str(),
                  Grouped(page.completion_tokens).c_str(), Grouped(page.reasoning_tokens).c_str());
    out::result_line(line);
    out::result_line("");

    if (page.requests.empty()) {
        out::result_line("no settled requests in this window");
        return;
    }

    std::snprintf(line, sizeof(line), "%-12s %-20s %4s %11s %11s %8s %8s  %s", "started",
                  "model", "code", "in", "out", "ttft", "spend", "request");
    out::result_line(line);

    for (const account::UsageRequestRow& row : page.requests) {
        const char* error =
            row.error_code.empty() ? (row.status_code >= 500 ? "5xx" : "-") : row.error_code.c_str();
        std::string started = row.ts_start;
        // The console writes `2026-09-25T03:41:07.123456+00:00`. The terminal
        // wants the moment, not the digits: the date and the time of day.
        if (started.size() > 19) {
            started = started.substr(0, 19);
        }
        std::snprintf(line, sizeof(line), "%-12s %-20s %4s %11s %11s %7sms %8s  %s",
                      started.substr(11, 8).c_str(), row.model.c_str(),
                      std::to_string(row.status_code).c_str(),
                      Grouped(row.prompt_tokens).c_str(), Grouped(row.completion_tokens).c_str(),
                      row.ttft_ms >= 0 ? std::to_string(row.ttft_ms).c_str() : "-",
                      Money(row.cost_micros).c_str(),
                      row.response_request_id.empty() ? row.request_id.c_str()
                                                      : row.response_request_id.c_str());
        out::result_line(line);
        if (std::strcmp(error, "-") != 0) {
            std::snprintf(line, sizeof(line), "%-12s %s%s%s", "", "error: ", error,
                          row.finish_reason.empty() ? "" : (" (" + row.finish_reason + ")").c_str());
            out::result_line(line);
        }
    }
    out::result_line("");
    if (!page.next_cursor.empty()) {
        out::status_line("more rows exist; pass --cursor to read the next page");
    }
}

/// `wally account usage --requests`: the per-request export
/// (`GET /v1/cli/usage/requests`, InferenceInfra #809). Read-only, one identity,
/// and a window the caller names: one day back by default, `--days` to move it,
/// capped at 31 because that is what the route refuses past.
int UsageRequests(bool as_json, int days, const std::string& model, int status_code,
                  const std::string& response_request_id, const std::string& cursor,
                  int limit, bool follow) {
    if (days < 1 || days > 31) {
        out::error_line("--days must be between 1 and 31 (the export refuses a longer window)");
        return 1;
    }

    account::Credentials credentials;
    std::string failure;
    if (!account::Load(&credentials, &failure)) {
        out::error_line(failure);
        return 1;
    }
    if (!credentials.signed_in()) {
        out::error_line("not signed in — run `wally account login`");
        return 1;
    }

    account::ConsoleClient client;
    if (credentials.access_token_expired(EpochSeconds()) &&
        !RefreshSession(client, &credentials, &failure)) {
        out::error_line(failure);
        return 1;
    }

    const long long now = EpochSeconds();
    const int seconds_back = days * 86'400;
    account::UsageRequestsQuery query;
    query.since = WindowStartIso(seconds_back, now);
    query.until = account::IsoTimestamp(now);
    query.model = model;
    query.status_code = status_code;
    query.response_request_id = response_request_id;
    query.limit = limit;
    query.cursor = cursor;

    auto fetch = [&client, &credentials, &query](account::UsageRequestsPage* page,
                                                 std::string* error) {
        account::IdentityResult result = client.FetchUsageRequests(
            credentials.console_url, credentials.access_token, query, page, error);
        if (result == account::IdentityResult::Unauthorized) {
            std::string refresh_failure;
            if (!RefreshSession(client, &credentials, &refresh_failure)) {
                // Not "expired": the console answers unknown, revoked, expired
                // and malformed with the same 401 on purpose, so which of the
                // four this was is not something we know — and sending someone
                // to re-login over a revoked key wastes the trip.
                if (error != nullptr) {
                    *error = "the console rejected this session (" + refresh_failure +
                             "); run `wally account login`";
                }
                return false;
            }
            result = client.FetchUsageRequests(credentials.console_url, credentials.access_token,
                                               query, page, error);
        }
        return result == account::IdentityResult::Ok;
    };

    account::UsageRequestsPage page;
    if (!fetch(&page, &failure)) {
        out::error_line(failure);
        return 1;
    }

    if (as_json) {
        PrintRequestsJson(page);
        // Pagination in JSON mode is the caller's to drive: the document is
        // one page, `next_cursor` says whether another exists. Walking pages
        // into one array would let a mid-walk failure pass as a complete
        // export, so it is not done even when someone asks.
        return 0;
    }

    PrintRequestsReport(page);
    if (follow && !page.next_cursor.empty()) {
        // Human mode keeps reading until a page comes back with no cursor, so
        // the report is the whole window and not its first 100 rows. Each page
        // re-uses the same filters by construction.
        int pages = 1;
        while (!page.next_cursor.empty() && pages < 100) {
            account::UsageRequestsPage next;
            query.cursor = page.next_cursor;
            if (!fetch(&next, &failure)) {
                out::error_line(failure);
                return 1;
            }
            PrintRequestsReport(next);
            page = std::move(next);
            pages++;
        }
        if (!page.next_cursor.empty()) {
            out::status_line("stopped at 100 pages; pass --cursor to continue from there");
        }
    }
    return 0;
}

}  // namespace

void register_usage(CLI::App& app, GlobalOptions& options) {
    auto as_json = std::make_shared<bool>(false);
    auto requests = std::make_shared<bool>(false);
    auto requests_days = std::make_shared<int>(1);
    auto requests_model = std::make_shared<std::string>("");
    auto requests_status = std::make_shared<int>(0);
    auto requests_rrid = std::make_shared<std::string>("");
    auto requests_cursor = std::make_shared<std::string>("");
    auto requests_limit = std::make_shared<int>(100);
    auto requests_follow = std::make_shared<bool>(false);

    // Lives under `account`. register_account runs first (app.cpp), so the
    // namespace normally exists by now, but that ordering is only a comment
    // over there, not something the type system enforces. A future reorder,
    // or any other caller that reaches for register_usage on its own, would
    // otherwise hit CLI11's bare OptionNotFound here — and configure_app()
    // runs ahead of wally_run_main's own try/catch, so nothing downstream
    // would catch it either. Guard the lookup the same way app.cpp guards its
    // own get_subcommand(hidden) calls, and say plainly what went wrong
    // instead of crashing on an unhandled exception.
    CLI::App* account_cmd = nullptr;
    try {
        account_cmd = app.get_subcommand("account");
    } catch (const CLI::OptionNotFound&) {
        out::error_line(
            "internal error: register_usage() ran before register_account() registered "
            "the `account` command");
        std::exit(1);
    }
    auto* usage =
        account_cmd->add_subcommand("usage", "Show remaining credit and the last day's spend");
    usage->add_flag("--json", *as_json, "Print as JSON");
    usage->footer(examples_footer({
        {"wally account usage", ""},
        {"wally --json account usage", ""},
    }));
    // `wally --json usage` and `wally usage --json` mean the same thing. The root
    // parser accepts the first, so reading only the command-local flag printed a
    // human table to something asking for one JSON document.
    usage->callback([as_json, &options] { fail(Usage(*as_json || options.json)); });

    // The per-request export: `wally account usage --requests`. Same read-only
    // surface, a page of settled rows instead of the windowed summary.
    usage->add_flag("--requests", *requests,
                    "List settled requests (one page by default, --follow to page through)");
    usage->add_option("--days", *requests_days, "Window length in days (1-31)")
        ->check(CLI::Range(1, 31));
    usage->add_option("--model", *requests_model, "Only this model id");
    usage->add_option("--status", *requests_status, "Only this HTTP status (100-599)")
        ->check(CLI::Range(100, 599));
    usage->add_option("--response-request-id", *requests_rrid,
                      "Only the rows for this x-request-id (what a response logged)");
    usage->add_option("--cursor", *requests_cursor, "Continue from a previous page's cursor");
    usage->add_option("--limit", *requests_limit, "Rows per page, 1-200")
        ->check(CLI::Range(1, 200));
    usage->add_flag("--follow", *requests_follow, "Read every page until the window is done");
    usage->footer(examples_footer({
        {"wally account usage --requests", "every settled request of the last day"},
        {"wally account usage --requests --days 7 --follow", "a full week, all pages"},
        {"wally account usage --requests --status 500", "the requests our side failed"},
        {"wally account usage --requests --response-request-id <id>",
         "the rows behind one x-request-id"},
        {"wally --json account usage --requests --limit 200", "one page as JSON"},
    }));
    usage->callback([as_json, requests, requests_days, requests_model, requests_status,
                     requests_rrid, requests_cursor, requests_limit, requests_follow, &options] {
        if (*requests) {
            fail(UsageRequests(*as_json || options.json, *requests_days, *requests_model,
                               *requests_status, *requests_rrid, *requests_cursor,
                               *requests_limit, *requests_follow));
            return;
        }
        fail(Usage(*as_json || options.json));
    });
}

}  // namespace wally::commands
