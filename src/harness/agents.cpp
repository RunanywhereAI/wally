#include "harness/agents.h"

#include <algorithm>
#include <cctype>
#include <cstdio>
#include <cstdlib>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <memory>
#include <random>
#include <string>
#include <system_error>
#include <vector>

#if !defined(_WIN32)
#include <fcntl.h>
#include <unistd.h>
#endif

#include "account/console.h"
#include "account/credentials.h"
#include "harness/harness.h"
#include "harness/local_models.h"
#include "io/output.h"

namespace wally::harness {
namespace {

/// An OpenAI client sends an Authorization header whatever is in it, and the
/// local server ignores the value. A placeholder keeps both ends happy.
std::string KeyOrPlaceholder(const std::string& api_key) {
    return api_key.empty() ? std::string("local") : api_key;
}

bool SetEnvironment(const char* name, const std::string& value) {
#if defined(_WIN32)
    return _putenv_s(name, value.c_str()) == 0;
#else
    return setenv(name, value.c_str(), 1) == 0;
#endif
}

void UnsetEnvironment(const char* name) {
#if defined(_WIN32)
    static_cast<void>(_putenv_s(name, ""));
#else
    static_cast<void>(unsetenv(name));
#endif
}

/// Sets a variable for the child and puts back what was there on the way out.
///
/// The process outlives one launch — tests run several, and the REPL can too —
/// so a value left behind would silently configure the next tool that named no
/// model.
class ScopedEnv {
   public:
    ScopedEnv(const char* name, const std::string& value) : name_(name) {
        const char* previous = std::getenv(name);
        if (previous != nullptr) {
            had_previous_ = true;
            previous_ = previous;
        }
        applied_ = SetEnvironment(name, value);
    }

    ScopedEnv(const ScopedEnv&) = delete;
    ScopedEnv& operator=(const ScopedEnv&) = delete;

    ~ScopedEnv() {
        if (!applied_) {
            return;
        }
        if (had_previous_) {
            static_cast<void>(SetEnvironment(name_, previous_));
        } else {
            UnsetEnvironment(name_);
        }
    }

    bool applied() const { return applied_; }

   private:
    const char* name_;
    std::string previous_;
    bool had_previous_ = false;
    bool applied_ = false;
};

/// A config file that exists only while the tool is running.
///
/// Written into the temp directory rather than the person's own config tree:
/// `~/.openclaw/config.json` is theirs, and a run of wally is not a reason to
/// rewrite it.
class TemporaryConfig {
   public:
    bool Write(const std::string& contents, std::string* error,
               const std::string& extension = ".json") {
        std::error_code code;
        const std::filesystem::path directory = std::filesystem::temp_directory_path(code);
        if (code) {
            *error = "no temp directory to write the agent config into";
            return false;
        }
        std::random_device entropy;
        path_ = directory / ("wally-agent-" + std::to_string(entropy()) + extension);

#if !defined(_WIN32)
        // The config carries the session's API key, so the file is created
        // 0600 up front rather than chmod'd after the write: chmod leaves a
        // window where the key sits in a 0644 (umask-default) file, and
        // O_EXCL refuses a name a local attacker pre-created or symlinked
        // (CWE-378). fdopen adopts the descriptor so the ofstream path below
        // stays a Windows-only fallback.
        const int fd = open(path_.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        if (fd < 0) {
            *error = "could not create the agent config at " + path_.string();
            path_.clear();
            return false;
        }
        FILE* file = fdopen(fd, "wb");
        if (file == nullptr) {
            close(fd);
            Remove();
            *error = "could not write the agent config to " + path_.string();
            return false;
        }
        const bool ok = std::fwrite(contents.data(), 1, contents.size(), file) == contents.size();
        if (std::fclose(file) != 0 || !ok) {
            Remove();
            *error = "could not write the agent config to " + path_.string();
            return false;
        }
#else
        // Windows: the per-user temp directory is ACL-restricted to its owner,
        // so a plain write is already private on the platform that has no mode
        // bits to set.
        std::ofstream file(path_, std::ios::binary | std::ios::trunc);
        if (!file) {
            *error = "could not write the agent config to " + path_.string();
            path_.clear();
            return false;
        }
        file << contents;
        file.close();
#endif
        return true;
    }

    std::string path() const { return path_.string(); }

    ~TemporaryConfig() { Remove(); }

   private:
    void Remove() {
        if (path_.empty()) {
            return;
        }
        std::error_code code;
        std::filesystem::remove(path_, code);
        path_.clear();
    }

    std::filesystem::path path_;
};

constexpr const char* kProviderId = "runanywhere";

/// The environment variable the dsh provider's `apiKeyEnv` points at. Fixed
/// rather than host-derived: dsh resolves the reference itself and puts no
/// host rule on the name.
constexpr const char* kDeepSeekKeyVariable = "RUNANYWHERE_API_KEY";

/// Whether the person gave dsh a job to do rather than flags for its web app.
///
/// The first token that is not a flag is a prompt, and a prompt means the
/// headless profile. `dsh web --port 8080` stays on the browser.


/// `agent.default_args` split on spaces, or `args` when the person passed any.
///
/// Theirs wins whole: a tool started with their own subcommand is theirs to
/// drive, and mixing our default into it would produce a command line neither
/// of us wrote.
std::vector<std::string> EffectiveArgs(const Agent& agent,
                                       const std::vector<std::string>& args) {
    if (!args.empty() || agent.default_args == nullptr || *agent.default_args == 0) {
        return args;
    }
    std::vector<std::string> defaults;
    const std::string spec(agent.default_args);
    std::size_t at = 0;
    while (at < spec.size()) {
        const std::size_t space = spec.find(' ', at);
        const std::string piece = spec.substr(at, space == std::string::npos ? space : space - at);
        if (!piece.empty()) {
            defaults.push_back(piece);
        }
        if (space == std::string::npos) {
            break;
        }
        at = space + 1;
    }
    return defaults;
}

/// Where OpenClaw keeps its state, and the config inside it.
///
/// `OPENCLAW_STATE_DIR` wins, then `OPENCLAW_HOME`, then `~/.openclaw` — the
/// order `resolveConfigDir` uses. Naming the state directory explicitly matters
/// more than it looks: OpenClaw otherwise derives it from the config file's own
/// folder, so pointing `OPENCLAW_CONFIG_PATH` at a temp file would move their
/// agents and sessions into the temp directory for the run.
std::filesystem::path OpenClawStateDirectory() {
    if (const char* state = std::getenv("OPENCLAW_STATE_DIR"); state != nullptr && *state != 0) {
        return {state};
    }
    if (const char* home = std::getenv("OPENCLAW_HOME"); home != nullptr && *home != 0) {
        return std::filesystem::path(home) / ".openclaw";
    }
    if (const char* home = std::getenv("HOME"); home != nullptr && *home != 0) {
        return std::filesystem::path(home) / ".openclaw";
    }
#if defined(_WIN32)
    // PowerShell and cmd.exe leave HOME unset; openclaw falls back to the profile.
    if (const char* profile = std::getenv("USERPROFILE"); profile != nullptr && *profile != 0) {
        return std::filesystem::path(profile) / ".openclaw";
    }
#endif
    return {};
}

/// What the console says this model costs and how much it can hold.
///
/// A local server has no catalog entry and no price: it is served with the
/// context size `Resolve` starts it with, which is the honest number to declare.
struct ModelLimits {
    std::int64_t context_window = 0;
    std::int64_t max_output = 0;
    std::int64_t input_per_mtok = 0;
    std::int64_t output_per_mtok = 0;
};

ModelLimits LookupLimits(const Endpoint& endpoint, const std::string& model) {
    ModelLimits limits;
    if (endpoint.api_key.empty()) {
        // The size `harness::Resolve` started the local server with.
        limits.context_window = LocalContextSize(model);
        return limits;
    }

    account::Credentials credentials;
    std::string error;
    if (!account::Load(&credentials, &error) || !credentials.signed_in()) {
        return limits;
    }
    const account::ConsoleClient console;
    std::vector<account::ModelInfo> models;
    if (console.FetchModels(credentials.console_url, credentials.access_token, &models, &error) ==
        account::IdentityResult::Ok) {
        for (const account::ModelInfo& info : models) {
            if (info.id == model) {
                limits.context_window = info.context_window;
                limits.max_output = info.max_output_tokens;
                break;
            }
        }
    } else {
        out::status_line("could not read the model list (" + error +
                         "); launching without a context-window hint");
    }
    std::vector<account::CatalogPrice> prices;
    if (console.FetchCatalog(credentials.console_url, credentials.access_token, &prices, &error) ==
        account::IdentityResult::Ok) {
        for (const account::CatalogPrice& price : prices) {
            if (price.id == model) {
                limits.input_per_mtok = price.input_per_mtok;
                limits.output_per_mtok = price.output_per_mtok;
                break;
            }
        }
    }
    return limits;
}

/// Their current config document, or empty when they have none.
std::string ReadOpenClawConfig() {
    std::filesystem::path path;
    if (const char* override_path = std::getenv("OPENCLAW_CONFIG_PATH");
        override_path != nullptr && *override_path != 0) {
        path = override_path;
    } else {
        const std::filesystem::path state = OpenClawStateDirectory();
        if (state.empty()) {
            return {};
        }
        path = state / "openclaw.json";
    }
    std::ifstream file(path, std::ios::binary);
    if (!file) {
        return {};
    }
    return std::string(std::istreambuf_iterator<char>(file), std::istreambuf_iterator<char>());
}

}  // namespace

const Agent kAgents[] = {
    {"hermes", "hermes", "Open Hermes with a model", Agent::Handoff::CustomEndpointEnvironment,
     "--tui"},
    {"openclaw", "openclaw", "Open OpenClaw with a model", Agent::Handoff::ConfigFile,
     "tui --local"},
    // No default arguments: this row picks its own profile below, and a `web`
    // default would arrive here as the person's first positional — which is to
    // say, as a prompt.
    {"deepseek", "dsh", "Open DeepSeek Harness with a model", Agent::Handoff::PatchOverlay, ""},
};

const int kAgentCount = static_cast<int>(sizeof(kAgents) / sizeof(kAgents[0]));

std::string HermesKeyVariable(const std::string& base_url) {
    // Mirrors hermes_cli/runtime_provider._host_derived_api_key: strip the
    // scheme, drop leading `api.`/`www.` labels, and name the registrable one.
    std::string host = base_url;
    const std::size_t scheme = host.find("://");
    if (scheme != std::string::npos) {
        host = host.substr(scheme + 3);
    }
    host = host.substr(0, host.find_first_of("/:"));
    if (host.empty() || host == "localhost") {
        return {};
    }

    std::vector<std::string> labels;
    std::size_t at = 0;
    while (at <= host.size()) {
        const std::size_t dot = host.find('.', at);
        const std::string label = host.substr(at, dot == std::string::npos ? dot : dot - at);
        if (!label.empty()) {
            labels.push_back(label);
        }
        if (dot == std::string::npos) {
            break;
        }
        at = dot + 1;
    }
    // An IP address ends in digits, and Hermes reads no key for one.
    if (labels.empty() || std::isdigit(static_cast<unsigned char>(labels.back().back())) != 0) {
        return {};
    }
    while (!labels.empty() && (labels.front() == "api" || labels.front() == "www")) {
        labels.erase(labels.begin());
    }
    if (labels.size() < 2) {
        return {};
    }

    std::string vendor;
    for (const char c : labels[labels.size() - 2]) {
        vendor += std::isalnum(static_cast<unsigned char>(c)) != 0
                      ? static_cast<char>(std::toupper(static_cast<unsigned char>(c)))
                      : '_';
    }
    if (std::isalpha(static_cast<unsigned char>(vendor.front())) == 0 || vendor == "OPENAI" ||
        vendor == "OPENROUTER" || vendor == "OLLAMA") {
        // Those three are host-gated on their own vendors' domains; borrowing
        // the name would hand our token to a check that is not about us.
        return {};
    }
    return vendor + "_API_KEY";
}

std::string HermesContextHint(std::int64_t context_window) {
    if (context_window <= 0) {
        return {};
    }
    const std::string tokens = std::to_string(context_window);
    return "hermes has no way to take a context-window hint from wally; this model "
           "supports " +
           tokens + " tokens — add model.context_length: " + tokens +
           " to your own ~/.hermes/config.yaml if you want hermes to budget the "
           "session against the full window";
}

std::vector<std::string> HermesArgv(const std::string& model,
                                    const std::vector<std::string>& child_args) {
    std::vector<std::string> argv{"--provider", "custom", "--model", model};
    argv.insert(argv.end(), child_args.begin(), child_args.end());
    return argv;
}

// ISO-8601 UTC "now". OpenClaw treats a non-empty `wizard.lastRunAt` as
// "onboarding complete", so this is what lets a first run skip its wizard.
std::string IsoNow() {
    const std::time_t now = std::time(nullptr);
    std::tm utc{};
#if defined(_WIN32)
    gmtime_s(&utc, &now);
#else
    gmtime_r(&now, &utc);
#endif
    char buffer[32];
    std::strftime(buffer, sizeof(buffer), "%Y-%m-%dT%H:%M:%SZ", &utc);
    return buffer;
}

std::string BuildOpenClawConfig(const std::string& existing, const std::string& primary,
                                const std::string& base_url, const std::string& api_key,
                                const std::vector<CatalogModel>& models) {
    nlohmann::json config = nlohmann::json::object();
    if (!existing.empty()) {
        nlohmann::json parsed = nlohmann::json::parse(existing, nullptr, false);
        if (parsed.is_object()) {
            config = std::move(parsed);
        }
    }

    // `mode: merge` keeps the catalogs from their own providers; this adds ours
    // and selects one for the run.
    config["models"]["mode"] = "merge";
    nlohmann::json entries = nlohmann::json::array();
    for (const CatalogModel& model : models) {
        nlohmann::json entry = {{"id", model.id}, {"name", model.id}, {"input", {"text"}}};
        // Only capabilities checked against this gateway. It returns usage on the
        // final streaming chunk when `stream_options.include_usage` is set, and it
        // takes `max_tokens` rather than `max_completion_tokens`.
        entry["compat"] = {{"supportsUsageInStreaming", true}, {"maxTokensField", "max_tokens"}};
        if (model.context_window > 0) {
            entry["contextWindow"] = model.context_window;
            // OpenClaw wants a maxTokens beside the window; without one it budgets
            // the session against a cap it invented.
            entry["maxTokens"] = model.max_output > 0
                                     ? model.max_output
                                     : std::min<std::int64_t>(model.context_window, 65536);
        }
        if (model.input_per_mtok > 0 || model.output_per_mtok > 0) {
            // Per-million-token rates, in whole currency units. The catalog carries
            // micro-dollars, so this is the same number a dollar sign away.
            const double per_micro = 1.0 / 1000000.0;
            entry["cost"] = {{"input", static_cast<double>(model.input_per_mtok) * per_micro},
                             {"output", static_cast<double>(model.output_per_mtok) * per_micro},
                             {"cacheRead", 0},
                             {"cacheWrite", 0}};
        }
        entries.push_back(std::move(entry));
    }
    config["models"]["providers"][kProviderId] = {
        {"baseUrl", base_url},
        {"apiKey", KeyOrPlaceholder(api_key)},
        {"api", "openai-completions"},
        {"models", std::move(entries)}};
    config["agents"]["defaults"]["model"]["primary"] = std::string(kProviderId) + "/" + primary;
    // Mark onboarding done so a first launch skips OpenClaw's wizard: it goes
    // straight into the tui against the provider we just wrote, no setup page.
    config["wizard"]["lastRunAt"] = IsoNow();
    return config.dump();
}

bool DeepSeekWantsHeadless(const std::vector<std::string>& args) {
    // The FIRST token only. A later bare word is a flag's value — `--port 8080`
    // is the web app being configured, not a prompt — and dsh reads its own
    // command line the same way: the first token the launcher does not
    // recognise is where the app's arguments start.
    return !args.empty() && !args.front().empty() && args.front().front() != '-';
}

std::string BuildDeepSeekSettings(const std::string& base_url, const std::string& key_variable,
                                  const std::vector<CatalogModel>& models) {
    nlohmann::json entries = nlohmann::json::array();
    for (const CatalogModel& model : models) {
        nlohmann::json entry = {{"id", model.id}, {"name", model.id}};
        if (model.context_window > 0) {
            entry["contextWindow"] = model.context_window;
            entry["maxTokens"] = model.max_output > 0
                                     ? model.max_output
                                     : std::min<std::int64_t>(model.context_window, 32768);
        }
        entries.push_back(std::move(entry));
    }
    nlohmann::json provider = {{"displayName", "RunAnywhere"},
                               {"api", "openai-completions"},
                               {"baseURL", base_url},
                               {"models", std::move(entries)},
                               // Always referenced, local server included. This used to
                               // be omitted for a loopback endpoint on the theory that
                               // no reference meant a keyless route; dsh 0.1.5 instead
                               // refuses the turn with "No API key for provider:
                               // runanywhere" before any request is made. The variable
                               // carries a placeholder for a local server, which ignores
                               // the Authorization header anyway.
                               {"apiKeyEnv", key_variable}};
    const nlohmann::json settings = {
        {"llm-pi-ai", {{"providers", {{kProviderId, provider}}}}}};
    return settings.dump();
}

/// A YAML single-quoted scalar: the value wrapped in \'...\' with every single
/// quote doubled. A temp path or model id has no business carrying a quote, but
/// an unescaped one would break the whole patch document rather than fail
/// loudly, so the boundary is closed here.
std::string YamlSingleQuoted(const std::string& value) {
    std::string escaped = "'";
    for (const char c : value) {
        if (c == '\'') {
            escaped += "''";
        } else {
            escaped += c;
        }
    }
    escaped += "'";
    return escaped;
}

std::string BuildDeepSeekPatch(const std::string& settings_path, const std::string& model) {
    return std::string("- id: settings\n") +               //
           "  config:\n" +                                 //
           "    path: " + YamlSingleQuoted(settings_path) + "\n" +  //
           "- id: agent-default-model\n" +                 //
           "  config:\n" +                                 //
           "    provider: " + kProviderId + "\n" +         //
           "    model: " + YamlSingleQuoted(model) + "\n";
}

int LaunchAgent(const Agent& agent, const std::string& model,
                const std::vector<std::string>& args) {
    if (model.empty()) {
        // Nothing to wire, so do not pretend to. Same contract as
        // `wally opencode` with no model.
        return Launch(agent.command, "", args);
    }

    Endpoint endpoint;
    if (!Resolve(model, &endpoint)) {
        return 1;
    }
    const std::vector<std::string> child_args = EffectiveArgs(agent, args);

    TemporaryConfig config;
    // The second document the dsh overlay needs; unused by the other handoffs
    // and removed with the first.
    TemporaryConfig settings;
    int status = 1;

    switch (agent.handoff) {
        case Agent::Handoff::CustomEndpointEnvironment: {
            const ScopedEnv base("CUSTOM_BASE_URL", endpoint.base_url);
            const ScopedEnv provider("HERMES_INFERENCE_PROVIDER", "custom");
            // Both names: the TUI launcher reads HERMES_MODEL and the oneshot
            // path reads HERMES_INFERENCE_MODEL, and which one runs depends on
            // arguments wally does not control.
            const ScopedEnv model_env("HERMES_INFERENCE_MODEL", model);
            const ScopedEnv tui_model("HERMES_MODEL", model);
            if (!base.applied() || !provider.applied() || !model_env.applied() ||
                !tui_model.applied()) {
                out::error_line("could not set the endpoint for " + std::string(agent.id));
                Release(endpoint);
                return 1;
            }
            // A key only reaches a host whose own name asks for it. An upstream
            // endpoint gets one under that name; a loopback server is handed
            // none, which is what it expects.
            const std::string key_variable = HermesKeyVariable(endpoint.base_url);
            std::unique_ptr<ScopedEnv> key;
            if (!endpoint.api_key.empty() && !key_variable.empty()) {
                key = std::make_unique<ScopedEnv>(key_variable.c_str(), endpoint.api_key);
            } else if (!endpoint.api_key.empty()) {
                out::status_line("this endpoint takes no host-gated key name; "
                                 "hermes will call it unauthenticated");
            }
            // The environment is not enough on its own. `model.provider` in
            // their config.yaml outranks HERMES_INFERENCE_PROVIDER, and with
            // the provider left at `auto` Hermes guesses one from the model id
            // -- `glm-5.3-flash` resolves to `zai`, which then fails for want
            // of a ZAI key, or worse routes the prompt to a third party the
            // person never chose. Naming it on the argv is what actually
            // pins the route, and it only applies on the `-z` and `--tui`
            // paths, which is why `--tui` is this row's default.
            // Surfaced, not injected — see `HermesContextHint`'s doc comment for
            // why there is nothing for wally to write instead.
            const ModelLimits limits = LookupLimits(endpoint, model);
            const std::string hint = HermesContextHint(limits.context_window);
            if (!hint.empty()) {
                out::status_line(hint);
            }
            out::status_line(std::string(agent.id) + " will talk to " + model + " through " +
                             endpoint.base_url);
            status = Launch(agent.id, "", HermesArgv(model, child_args));
            break;
        }
        case Agent::Handoff::ConfigFile: {
            const std::vector<CatalogModel> catalog = CatalogModels(endpoint, model);
            if (catalog.front().context_window > 0) {
                out::status_line("context window: " +
                                 std::to_string(catalog.front().context_window) + " tokens");
            }
            std::string failure;
            if (!config.Write(
                    BuildOpenClawConfig(ReadOpenClawConfig(), model, endpoint.base_url,
                                        endpoint.api_key, catalog),
                    &failure)) {
                out::error_line(failure);
                Release(endpoint);
                return 1;
            }
            const std::filesystem::path state = OpenClawStateDirectory();
            if (state.empty()) {
                out::error_line("could not work out where openclaw keeps its state");
                Release(endpoint);
                return 1;
            }
            // Pinned before the config path, because OpenClaw derives the state
            // directory from the config file's folder when this is unset.
            const ScopedEnv state_dir("OPENCLAW_STATE_DIR", state.string());
            const ScopedEnv path("OPENCLAW_CONFIG_PATH", config.path());
            if (!state_dir.applied() || !path.applied()) {
                out::error_line("could not set the endpoint for " + std::string(agent.id));
                Release(endpoint);
                return 1;
            }
            out::status_line(std::string(agent.id) + " will talk to " + model + " through " +
                             endpoint.base_url);
            status = Launch(agent.command, "", child_args);
            break;
        }
        case Agent::Handoff::PatchOverlay: {
            const std::vector<CatalogModel> catalog = CatalogModels(endpoint, model);
            if (catalog.front().context_window > 0) {
                out::status_line("context window: " +
                                 std::to_string(catalog.front().context_window) + " tokens");
            }
            std::string failure;
            if (!settings.Write(
                    BuildDeepSeekSettings(endpoint.base_url, kDeepSeekKeyVariable, catalog),
                    &failure) ||
                !config.Write(BuildDeepSeekPatch(settings.path(), model), &failure, ".yml")) {
                out::error_line(failure);
                Release(endpoint);
                return 1;
            }

            // The real key for a hosted model; a placeholder for a local server,
            // which dsh insists on having and the server never reads.
            const ScopedEnv key(kDeepSeekKeyVariable,
                                endpoint.api_key.empty() ? "local" : endpoint.api_key);
            if (!key.applied()) {
                out::error_line("could not set the endpoint for " + std::string(agent.id));
                Release(endpoint);
                return 1;
            }

            // `--patch` belongs to the launcher, so it goes ahead of anything
            // the app itself parses. A prompt of their own switches the profile:
            // dsh's interactive surface is the browser, and its terminal entry
            // is one-shot.
            std::vector<std::string> launch;
            const bool names_profile =
                std::find(child_args.begin(), child_args.end(), "--profile") != child_args.end();
            if (names_profile) {
                // They are driving the launcher themselves; add the overlay and
                // stay out of the way.
                launch = {"--patch", config.path()};
            } else if (DeepSeekWantsHeadless(child_args)) {
                launch = {"--profile", "headless", "--patch", config.path()};
            } else {
                launch = {"web", "--patch", config.path()};
                out::status_line("opening the dsh web ui; pass a prompt to run headless instead");
            }
            launch.insert(launch.end(), child_args.begin(), child_args.end());
            out::status_line(std::string(agent.id) + " will talk to " + model + " through " +
                             endpoint.base_url);
            status = Launch(agent.command, "", launch);
            break;
        }
    }

    Release(endpoint);
    return status;
}

}  // namespace wally::harness
