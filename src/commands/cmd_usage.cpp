#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <string>

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

}  // namespace

void register_usage(CLI::App& app, GlobalOptions& options) {
    auto as_json = std::make_shared<bool>(false);

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
}

}  // namespace wally::commands
