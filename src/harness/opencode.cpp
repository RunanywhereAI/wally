#include "harness/opencode.h"

#include "harness/harness.h"

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <nlohmann/json.hpp>
#include <string>
#include <utility>
#include <vector>

#if defined(_WIN32)
#include <process.h>
#else
#include <unistd.h>

#include <sys/wait.h>
#endif

#include "account/credentials.h"
#include "account/model_cache.h"
#include "io/output.h"

namespace wally::harness {
namespace {

constexpr const char* kOpenCodeConfigVariable = "OPENCODE_CONFIG_CONTENT";



bool SetEnvironment(const char* name, const std::string& value) {
#if defined(_WIN32)
    return _putenv_s(name, value.c_str()) == 0;
#else
    return setenv(name, value.c_str(), 1) == 0;
#endif
}

bool UnsetEnvironment(const char* name) {
#if defined(_WIN32)
    return _putenv_s(name, "") == 0;
#else
    return unsetenv(name) == 0;
#endif
}

class ScopedOpenCodeConfig {
   public:
    ScopedOpenCodeConfig() {
        const char* previous = std::getenv(kOpenCodeConfigVariable);
        if (previous != nullptr) {
            had_previous_ = true;
            previous_ = previous;
        }
    }

    ScopedOpenCodeConfig(const ScopedOpenCodeConfig&) = delete;
    ScopedOpenCodeConfig& operator=(const ScopedOpenCodeConfig&) = delete;

    ~ScopedOpenCodeConfig() {
        if (!active_) {
            return;
        }
        if (had_previous_) {
            static_cast<void>(SetEnvironment(kOpenCodeConfigVariable, previous_));
        } else {
            static_cast<void>(UnsetEnvironment(kOpenCodeConfigVariable));
        }
    }

    bool Activate(const std::string& value) {
        active_ = SetEnvironment(kOpenCodeConfigVariable, value);
        return active_;
    }

   private:
    std::string previous_;
    bool had_previous_ = false;
    bool active_ = false;
};

/// Not an error line. Nothing went wrong — the tool simply is not here yet,
/// and the only useful thing to say is how to get it.
void MissingOpenCode() {
    out::status_line("opencode is not installed on this machine");
    out::status_line("install it with `npm i -g opencode-ai`, then run this again");
}

#if defined(_WIN32)
// Quote one argument so the child re-parses it as a single token. The _spawn*
// family joins argv into a command line WITHOUT quoting, so an argument that
// contains a space would otherwise arrive split in two. Rules per the
// documented MSVCRT parser: double the run of backslashes that precedes a quote
// (or the closing quote), and backslash-escape embedded quotes. The POSIX path
// needs none of this -- execvp hands argv to the child verbatim.
std::string QuoteWindowsArg(const std::string& arg) {
    if (!arg.empty() && arg.find_first_of(" \t\n\v\"") == std::string::npos) {
        return arg;
    }
    std::string quoted = "\"";
    for (std::size_t i = 0;; ++i) {
        std::size_t backslashes = 0;
        while (i < arg.size() && arg[i] == '\\') {
            ++i;
            ++backslashes;
        }
        if (i == arg.size()) {
            quoted.append(backslashes * 2, '\\');
            break;
        }
        if (arg[i] == '"') {
            quoted.append(backslashes * 2 + 1, '\\');
            quoted.push_back('"');
        } else {
            quoted.append(backslashes, '\\');
            quoted.push_back(arg[i]);
        }
    }
    quoted.push_back('"');
    return quoted;
}
#endif

int Spawn(const std::string& executable, const std::vector<std::string>& arguments) {
#if defined(_WIN32)
    std::vector<std::string> owned;
    owned.reserve(arguments.size() + 1);
    owned.push_back(QuoteWindowsArg(executable));
    for (const std::string& argument : arguments) {
        owned.push_back(QuoteWindowsArg(argument));
    }
    std::vector<char*> argv;
    argv.reserve(owned.size() + 1);
    for (std::string& value : owned) {
        argv.push_back(value.data());
    }
    argv.push_back(nullptr);

    const intptr_t status = _spawnvp(_P_WAIT, executable.c_str(), argv.data());
    if (status < 0) {
        MissingOpenCode();
        return 127;
    }
    return static_cast<int>(status);
#else
    std::vector<std::string> owned;
    owned.reserve(arguments.size() + 1);
    owned.push_back(executable);
    owned.insert(owned.end(), arguments.begin(), arguments.end());

    std::vector<char*> argv;
    argv.reserve(owned.size() + 1);
    for (std::string& value : owned) {
        argv.push_back(value.data());
    }
    argv.push_back(nullptr);

    const pid_t child = fork();
    if (child < 0) {
        out::error_line("could not start OpenCode");
        return 1;
    }
    if (child == 0) {
        execvp(executable.c_str(), argv.data());
        _exit(127);
    }

    int status = 0;
    while (waitpid(child, &status, 0) < 0) {
        if (errno == EINTR) {
            continue;
        }
        out::error_line("lost track of OpenCode");
        return 1;
    }
    if (!WIFEXITED(status)) {
        return 1;
    }
    const int exit_code = WEXITSTATUS(status);
    if (exit_code == 127) {
        MissingOpenCode();
    }
    return exit_code;
#endif
}

}  // namespace

std::string BuildOpenCodeCloudConfig(const std::string& model, const std::string& base_url,
                                     const std::string& access_token, std::int64_t context_window,
                                     std::int64_t max_output, std::int64_t input_per_mtok,
                                     std::int64_t output_per_mtok) {
    using Json = nlohmann::json;
    Json entry = {{"name", model}};
    // The real limits, so opencode's context gauge and auto-compaction fire at
    // the model's actual window instead of a wrong default (which makes it nag
    // to compact and never stop). Output is a sane cap, never the whole context
    // -- opencode's own docs warn against that.
    if (context_window > 0) {
        const std::int64_t output =
            max_output > 0 ? max_output : std::min<std::int64_t>(context_window, 65536);
        entry["limit"] = Json{{"context", context_window}, {"output", output}};
    }
    // The real price, so opencode shows spend instead of $0.00. opencode's cost
    // is USD per million tokens; the catalog is micro-dollars per million, so a
    // million micros is one dollar.
    if (input_per_mtok > 0 || output_per_mtok > 0) {
        entry["cost"] = Json{{"input", static_cast<double>(input_per_mtok) / 1'000'000.0},
                             {"output", static_cast<double>(output_per_mtok) / 1'000'000.0}};
    }
    const Json provider = {
        {"npm", "@ai-sdk/openai-compatible"},
        {"name", "RunAnywhere"},
        {"options", {{"baseURL", base_url}, {"apiKey", access_token}}},
        {"models", {{model, entry}}},
    };
    return Json{{"provider", {{"runanywhere", provider}}}, {"model", "runanywhere/" + model}}
        .dump();
}

int LaunchOpenCodeCloud(const std::string& model, const std::vector<std::string>& arguments,
                        const account::ConsoleClient& console, const SpawnFunction& spawn) {
    // The same two gates the local path gets. This function does not go through
    // harness::Resolve(), so before this it had its own weaker model check
    // (control characters only, so `/` and `<` sailed through) and trusted
    // `signed_in()` — a non-empty string — as proof of a session. A fabricated
    // token launched a real editor against the hosted endpoint.
    if (!ModelIdIsSafe(model)) {
        out::error_line("'" + model + "' is not a valid model id");
        return 2;
    }

    account::Credentials credentials;
    std::string error;
    if (!account::Load(&credentials, &error)) {
        out::error_line(error);
        return 1;
    }
    if (!credentials.signed_in()) {
        ReportNotSignedIn();
        return 1;
    }
    // Refresh the catalog for next time without blocking, and reject a mistyped
    // id from the cache. Fail open on an empty cache.
    account::RefreshModelCacheIfStale(account::kModelCacheTtlSeconds);
    if (account::CacheHasModels() && !account::ModelIsCached(model)) {
        // Stale cache: refresh live and retry rather than reject a valid model.
        if (!RefreshAndRecheckModel(credentials, model)) {
            return 1;
        }
    }
    bool unverified = false;
    if (!VerifyCloudSession(console, &credentials, nullptr, &error, &unverified)) {
        if (!unverified) {
            ReportCloudSessionInvalid(model);
            return 1;
        }
        // The console could not be asked right now. That is not a disproof of
        // the session already on disk, and refusing here locks a signed-in
        // person out of their harness over a transient 429 (InferenceInfra#444).
        // Go in on the stored session; the harness's own calls surface the real
        // error if it is still there.
        out::status_line("could not confirm the cloud session (" + error +
                         ") - continuing on the stored session");
    }

    const std::string base_url = credentials.console_url + "/v1";
    // The model's real context window and price, so opencode's compaction fires
    // at the right point and its usage shows real spend. A failed fetch is not
    // fatal -- launch with whatever we learned.
    std::int64_t context_window = 0;
    std::int64_t max_output = 0;
    std::int64_t input_price = 0;
    std::int64_t output_price = 0;
    std::string fetch_error;
    std::vector<account::ModelInfo> models;
    if (console.FetchModels(credentials.console_url, credentials.access_token, &models,
                            &fetch_error) == account::IdentityResult::Ok) {
        for (const account::ModelInfo& info : models) {
            if (info.id == model) {
                context_window = info.context_window;
                max_output = info.max_output_tokens;
                break;
            }
        }
    } else {
        out::status_line("could not read the model list (" + fetch_error +
                         "); launching without a context-window hint");
    }
    std::vector<account::CatalogPrice> prices;
    if (console.FetchCatalog(credentials.console_url, credentials.access_token, &prices,
                             &fetch_error) == account::IdentityResult::Ok) {
        for (const account::CatalogPrice& price : prices) {
            if (price.id == model) {
                input_price = price.input_per_mtok;
                output_price = price.output_per_mtok;
                break;
            }
        }
    }
    if (context_window > 0) {
        out::status_line("context window: " + std::to_string(context_window) + " tokens");
    }
    const std::string config = BuildOpenCodeCloudConfig(
        model, base_url, credentials.access_token, context_window, max_output, input_price,
        output_price);
    ScopedOpenCodeConfig environment;
    if (!environment.Activate(config)) {
        out::error_line("could not set the temporary OpenCode configuration");
        return 1;
    }

    out::status_line("launching OpenCode with the RunAnywhere cloud session");
    return spawn("opencode", arguments);
}

int LaunchOpenCodeCloud(const std::string& model, const std::vector<std::string>& arguments) {
    const account::ConsoleClient console;
    return LaunchOpenCodeCloud(model, arguments, console, Spawn);
}

}  // namespace wally::harness
