#include "test_common.h"

#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <nlohmann/json.hpp>
#include <set>
#include <string>

#include "account/console.h"
#include "account/credentials.h"
#include "harness/agents.h"
#include "harness/harness.h"
#include "harness/local_models.h"

namespace {

namespace fs = std::filesystem;
using Json = nlohmann::json;

class Environment {
   public:
    Environment(const char* name, const char* value) : name_(name) {
        if (const char* previous = std::getenv(name)) {
            had_previous_ = true;
            previous_ = previous;
        }
        Set(value);
    }
    ~Environment() { Set(had_previous_ ? previous_.c_str() : nullptr); }

   private:
    void Set(const char* value) {
#if defined(_WIN32)
        _putenv_s(name_.c_str(), value != nullptr ? value : "");
#else
        value != nullptr ? setenv(name_.c_str(), value, 1) : unsetenv(name_.c_str());
#endif
    }
    std::string name_;
    std::string previous_;
    bool had_previous_ = false;
};

class TemporaryDirectory {
   public:
    TemporaryDirectory() {
        const auto nonce = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("wally-harness-test-" + std::to_string(nonce));
        fs::create_directories(path_);
    }
    ~TemporaryDirectory() {
        std::error_code ignored;
        fs::remove_all(path_, ignored);
    }
    const fs::path& path() const { return path_; }

   private:
    fs::path path_;
};

long long Now() {
    return std::chrono::duration_cast<std::chrono::seconds>(
               std::chrono::system_clock::now().time_since_epoch())
        .count();
}

// ---------------------------------------------------------------------------
// ModelIdIsSafe — finding 4: garbage / path-traversal / XML-structural
// strings must never reach a live editor session.
// ---------------------------------------------------------------------------

TestResult test_model_id_rejects_empty_and_control_characters() {
    TestResult result;
    result.test_name = "model_id_rejects_empty_and_control_characters";

    const std::string unsafe[] = {"", std::string("qwen\nrm -rf"), std::string("qwen\x7f", 5)};
    for (const std::string& id : unsafe) {
        if (wally::harness::ModelIdIsSafe(id)) {
            result.details = "accepted an id containing a control character or empty id";
            return result;
        }
    }
    result.passed = true;
    return result;
}

TestResult test_model_id_rejects_xml_and_path_structural_characters() {
    TestResult result;
    result.test_name = "model_id_rejects_xml_and_path_structural_characters";

    // Each of these would corrupt or extend a raw string-concatenated config
    // file wally writes for a tool it wires up, or claims a directory
    // separator no real local/upstream id ever contains.
    const std::string unsafe[] = {
        "qwen3\"/><option name=\"evil\" value=\"x",
        "qwen3</option><option name=\"x",
        "../../etc/passwd",
        "org/repo",
        "a\\b",
        "at&t-model",
        "it's-a-model",
    };
    for (const std::string& id : unsafe) {
        if (wally::harness::ModelIdIsSafe(id)) {
            result.details = "accepted an XML/path-structural model id: " + id;
            return result;
        }
    }
    result.passed = true;
    return result;
}

TestResult test_model_id_accepts_ordinary_ids() {
    TestResult result;
    result.test_name = "model_id_accepts_ordinary_ids";

    const std::string safe[] = {"mlx-qwen3", "whisper-tiny", "qwen3.8-27b-1bit-npu", "smolvlm2"};
    for (const std::string& id : safe) {
        if (!wally::harness::ModelIdIsSafe(id)) {
            result.details = "rejected an ordinary model id: " + id;
            return result;
        }
    }
    result.passed = true;
    return result;
}

// ---------------------------------------------------------------------------
// VerifyCloudSession — findings 1/2: Resolve() must confirm a session against
// the console, not just check that a token string is non-empty.
// ---------------------------------------------------------------------------

bool Seed(const fs::path& profile, const std::string& access_token,
         const std::string& refresh_token, long long expires_at, std::string* error) {
    Environment scoped_profile("WALLY_PROFILE_DIR", profile.string().c_str());
    wally::account::Credentials credentials;
    credentials.console_url = "https://console.runanywhere.ai";
    credentials.email = "developer@example.test";
    credentials.access_token = access_token;
    credentials.refresh_token = refresh_token;
    credentials.expires_at = expires_at;
    return wally::account::Save(credentials, error);
}

// A hand-written credentials.json with any non-empty access_token and no
// refresh_token — exactly what signed_in() alone accepted — must fail
// VerifyCloudSession instead of being treated as a real session.
TestResult test_verify_cloud_session_rejects_unverifiable_token() {
    TestResult result;
    result.test_name = "verify_cloud_session_rejects_unverifiable_token";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    if (!Seed(temporary.path(), "hand-written-garbage-token", "", Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool contacted_console = false;
    wally::account::ConsoleClient console([&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        contacted_console = true;
        response->status = 401;
        return true;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    const bool ok = wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error);
    if (ok) {
        result.details = "a garbage token with no refresh token must not verify";
        return result;
    }
    if (!contacted_console) {
        result.details = "VerifyCloudSession must ask the console, not just check the token shape";
        return result;
    }
    if (verify_error.empty()) {
        result.details = "failure must explain why";
        return result;
    }
    result.passed = true;
    return result;
}

// An expired access token with a good refresh token is refreshed and then
// re-verified, exactly the `wally usage` dance, and the refreshed session is
// what Resolve() goes on to use.
TestResult test_verify_cloud_session_refreshes_and_reverifies() {
    TestResult result;
    result.test_name = "verify_cloud_session_refreshes_and_reverifies";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    if (!Seed(temporary.path(), "old-access-token", "refresh-token", Now() - 1, &error)) {
        result.details = error;
        return result;
    }

    bool refreshed = false;
    bool verified_with_new_token = false;
    wally::account::ConsoleClient console([&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        if (request.url.ends_with("/auth/cli/refresh")) {
            refreshed = true;
            response->status = 200;
            response->body = Json{{"access_token", "new-access-token"},
                                  {"refresh_token", "new-refresh-token"},
                                  {"expires_in", 7200}}
                                 .dump();
            return true;
        }
        if (request.url.ends_with("/v1/me")) {
            verified_with_new_token = request.bearer_token == "new-access-token";
            response->status = 200;
            response->body = Json{{"email", "developer@example.test"}}.dump();
            return true;
        }
        return false;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    const bool ok = wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error);
    if (!ok || !refreshed || !verified_with_new_token) {
        result.details = "expired token should be refreshed, then verified with the new token";
        result.actual = verify_error;
        return result;
    }
    if (email != "developer@example.test") {
        result.expected = "developer@example.test";
        result.actual = email;
        return result;
    }
    if (credentials.access_token != "new-access-token") {
        result.details = "credentials must carry the refreshed token back to the caller";
        return result;
    }
    result.passed = true;
    return result;
}

// A valid, unexpired token that the console still accepts verifies without
// ever calling refresh.
TestResult test_verify_cloud_session_accepts_real_session() {
    TestResult result;
    result.test_name = "verify_cloud_session_accepts_real_session";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    if (!Seed(temporary.path(), "real-access-token", "refresh-token", Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool refresh_called = false;
    wally::account::ConsoleClient console([&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        if (request.url.ends_with("/auth/cli/refresh")) {
            refresh_called = true;
            return false;
        }
        if (request.url.ends_with("/v1/me") && request.bearer_token == "real-access-token") {
            response->status = 200;
            response->body = Json{{"email", "developer@example.test"}}.dump();
            return true;
        }
        return false;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    const bool ok = wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error);
    if (!ok || refresh_called) {
        result.details = "a valid session should verify without refreshing";
        result.actual = verify_error;
        return result;
    }
    result.passed = true;
    return result;
}

// A console that is merely RATE LIMITING must not read as a bad session. This
// is InferenceInfra#444: a load test drove /v1/me to 429 and every signed-in
// person was refused entry to their own harness, `wally login` included.
TestResult test_verify_cloud_session_rate_limit_is_unverified_not_bad() {
    TestResult result;
    result.test_name = "verify_cloud_session_rate_limit_is_unverified_not_bad";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    if (!Seed(temporary.path(), "good-access-token", "refresh-token", Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    wally::account::ConsoleClient console([&](const wally::account::HttpRequest&,
                                             wally::account::HttpResponse* response, std::string*) {
        response->status = 429;
        return true;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    bool unverified = false;
    const bool ok =
        wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error, &unverified);
    if (ok) {
        result.details = "a 429 is not a verified session";
        return result;
    }
    if (!unverified) {
        result.details =
            "a rate-limited console must report the session as UNVERIFIED, not as bad - "
            "otherwise the harness refuses a signed-in person over a transient 429";
        return result;
    }
    result.passed = true;
    return result;
}

// The other half: a console that actually rejects the session must NOT be
// reported as merely unverified, or a revoked key would walk straight into a
// harness.
TestResult test_verify_cloud_session_rejected_session_is_not_unverified() {
    TestResult result;
    result.test_name = "verify_cloud_session_rejected_session_is_not_unverified";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    if (!Seed(temporary.path(), "revoked-access-token", "revoked-refresh-token", Now() + 3600,
              &error)) {
        result.details = error;
        return result;
    }

    wally::account::ConsoleClient console([&](const wally::account::HttpRequest&,
                                             wally::account::HttpResponse* response, std::string*) {
        response->status = 401;
        return true;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    bool unverified = false;
    const bool ok =
        wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error, &unverified);
    if (ok) {
        result.details = "a 401 session must not verify";
        return result;
    }
    if (unverified) {
        result.details = "a rejected session must not be reported as merely unverified";
        return result;
    }
    result.passed = true;
    return result;
}

// The path a real user actually hits: tokens expire hourly, so an expired
// access token refreshes FIRST, and that refresh is itself a console call that
// can be rate limited. The 429 fix on the identity check did not cover it, and
// the launch was still refused before the identity check was ever reached
// (InferenceInfra#444, reported against wally 0.5.6).
TestResult test_verify_cloud_session_rate_limited_refresh_is_unverified_not_bad() {
    TestResult result;
    result.test_name = "verify_cloud_session_rate_limited_refresh_is_unverified_not_bad";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    std::string error;
    // Expired access token, so VerifyCloudSession refreshes before anything else.
    if (!Seed(temporary.path(), "expired-access-token", "refresh-token", Now() - 1, &error)) {
        result.details = error;
        return result;
    }

    bool asked_refresh = false;
    wally::account::ConsoleClient console([&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        if (request.url.ends_with("/auth/cli/refresh")) {
            asked_refresh = true;
        }
        response->status = 429;
        return true;
    });

    wally::account::Credentials credentials;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error;
        return result;
    }

    std::string email;
    std::string verify_error;
    bool unverified = false;
    const bool ok =
        wally::harness::VerifyCloudSession(console, &credentials, &email, &verify_error, &unverified);
    if (ok) {
        result.details = "a rate-limited refresh is not a verified session";
        return result;
    }
    if (!asked_refresh) {
        result.details = "an expired token must attempt a refresh first";
        return result;
    }
    if (!unverified) {
        result.details =
            "a rate-limited REFRESH must report the session as UNVERIFIED, not as bad - this is "
            "the path that still refused the harness after the identity check was fixed";
        return result;
    }
    result.passed = true;
    return result;
}

// OpenClaw reads a whole config document rather than a base-URL variable, so the
// document is the contract: the wrong provider id, a missing `mode`, or a model
// the selection does not name all fail silently as "it ignored our endpoint".
TestResult test_openclaw_config_selects_our_provider_and_model() {
    TestResult result;
    result.test_name = "openclaw_config_selects_our_provider_and_model";

    const std::vector<wally::harness::CatalogModel> catalog = {
        {"gemma-4-31b-it", 131072, 8192, 300000, 1200000}};
    const Json config = Json::parse(wally::harness::BuildOpenClawConfig(
        "", "gemma-4-31b-it", "https://inference.runanywhere.ai/v1", "sk-live-xyz", catalog));

    if (config["agents"]["defaults"]["model"]["primary"] != "runanywhere/gemma-4-31b-it") {
        result.details = "the agent default must name <provider>/<model>, or OpenClaw keeps its own";
        return result;
    }
    if (config["models"]["mode"] != "merge") {
        result.details = "merge mode, so the person's own providers survive the run";
        return result;
    }
    const Json& provider = config["models"]["providers"]["runanywhere"];
    if (provider["baseUrl"] != "https://inference.runanywhere.ai/v1" ||
        provider["apiKey"] != "sk-live-xyz" || provider["api"] != "openai-completions") {
        result.details = "the provider must carry our endpoint, key and API shape";
        return result;
    }
    if (provider["models"].size() != 1 || provider["models"][0]["id"] != "gemma-4-31b-it") {
        result.details = "the provider must advertise exactly the model that was asked for";
        return result;
    }
    // Without these OpenClaw shows its own default 128k and no spend, whatever
    // the model really is.
    if (provider["models"][0]["contextWindow"] != 131072 ||
        provider["models"][0]["maxTokens"] != 8192) {
        result.details = "the real context window and output cap must reach the config";
        return result;
    }
    if (provider["models"][0]["cost"]["input"] != 0.3 ||
        provider["models"][0]["cost"]["output"] != 1.2) {
        result.details = "micro-dollars per Mtok must arrive as whole currency per Mtok";
        return result;
    }
    result.passed = true;
    return result;
}

// A local server needs no credential and is handed none by Resolve, but an
// OpenAI client sends the Authorization header regardless. An empty key there
// is a 401 from our own loopback server.
TestResult test_openclaw_config_substitutes_a_key_for_a_local_endpoint() {
    TestResult result;
    result.test_name = "openclaw_config_substitutes_a_key_for_a_local_endpoint";

    const Json config = Json::parse(wally::harness::BuildOpenClawConfig(
        "", "qwen3-0.6b", "http://127.0.0.1:52431/v1", "", {{"qwen3-0.6b", 8192, 0, 0, 0}}));
    const std::string key = config["models"]["providers"]["runanywhere"]["apiKey"];
    if (key.empty()) {
        result.details = "an empty apiKey must become a placeholder, not an empty header";
        return result;
    }
    result.passed = true;
    return result;
}

// The table is the integration surface. A row that names an id the command
// registration cannot use, or a duplicate, is a broken subcommand.
TestResult test_agent_table_rows_are_usable_subcommands() {
    TestResult result;
    result.test_name = "agent_table_rows_are_usable_subcommands";

    std::set<std::string> seen;
    for (int index = 0; index < wally::harness::kAgentCount; ++index) {
        const std::string id = wally::harness::kAgents[index].id;
        if (id.empty() || id.find(' ') != std::string::npos) {
            result.details = "an agent id must be a single bare word";
            return result;
        }
        if (!seen.insert(id).second) {
            result.details = "duplicate agent id: " + id;
            return result;
        }
        if (wally::harness::kAgents[index].summary == nullptr ||
            wally::harness::kAgents[index].default_args == nullptr) {
            result.details = id + " has a null summary or default_args";
            return result;
        }
        // The subcommand and the executable are not always the same word, and
        // launching the subcommand name is a "not installed" for a tool that is.
        const std::string command = wally::harness::kAgents[index].command;
        if (command.empty() || command.find(' ') != std::string::npos) {
            result.details = id + " has no usable executable name";
            return result;
        }
    }
    for (const std::string& required : {"hermes", "openclaw", "deepseek"}) {
        if (seen.find(required) == seen.end()) {
            result.details = "agent table is missing " + required;
            return result;
        }
    }
    result.passed = true;
    return result;
}

// OPENCLAW_CONFIG_PATH replaces the whole document, so anything of theirs that
// is not carried over is gone for the run: the wizard flag, the agents, the
// gateway token. Dropping the first is what made onboarding run every launch.
TestResult test_openclaw_config_preserves_the_existing_document() {
    TestResult result;
    result.test_name = "openclaw_config_preserves_the_existing_document";

    const std::string existing = R"({
      "wizard": {"securityAcknowledgedAt": "2026-09-15T08:01:31.669Z"},
      "telemetry": {"enabled": false},
      "gateway": {"auth": {"token": "abc123"}, "port": 18789},
      "agents": {"entries": {"main": {"name": "main"}}}
    })";
    const Json config = Json::parse(wally::harness::BuildOpenClawConfig(
        existing, "glm-5.3-flash", "https://inference.runanywhere.ai/api-dev/v1", "sk-live",
        {{"glm-5.3-flash", 0, 0, 0, 0}}));

    if (!config.contains("wizard") || !config["wizard"].contains("securityAcknowledgedAt")) {
        result.details = "the wizard flag must survive, or onboarding runs on every launch";
        return result;
    }
    if (config["gateway"]["auth"]["token"] != "abc123" || config["gateway"]["port"] != 18789) {
        result.details = "the gateway token and port must survive";
        return result;
    }
    if (config["agents"]["entries"]["main"]["name"] != "main") {
        result.details = "their agents must survive";
        return result;
    }
    if (config["agents"]["defaults"]["model"]["primary"] != "runanywhere/glm-5.3-flash") {
        result.details = "and our model must still be selected alongside them";
        return result;
    }
    result.passed = true;
    return result;
}

// The whole catalog reaches the picker, not just the launched model, and the
// launched one stays the default.
TestResult test_openclaw_config_lists_every_catalog_model() {
    TestResult result;
    result.test_name = "openclaw_config_lists_every_catalog_model";

    const std::vector<wally::harness::CatalogModel> catalog = {{"glm-5.3-flash", 1048567, 0, 0, 0},
                                                               {"qwen3.8-27b", 262144, 0, 0, 0},
                                                               {"gemma-4", 131072, 0, 0, 0}};
    const Json config = Json::parse(wally::harness::BuildOpenClawConfig(
        "", "glm-5.3-flash", "https://inference.runanywhere.ai/v1", "sk-live", catalog));
    const Json& models = config["models"]["providers"]["runanywhere"]["models"];
    if (models.size() != 3) {
        result.details = "every catalog model must be a selectable entry; got " + models.dump();
        return result;
    }
    std::set<std::string> ids;
    for (const Json& entry : models) {
        ids.insert(entry["id"].get<std::string>());
    }
    if (ids.count("glm-5.3-flash") == 0 || ids.count("qwen3.8-27b") == 0 ||
        ids.count("gemma-4") == 0) {
        result.details = "all three catalog ids must appear: " + models.dump();
        return result;
    }
    if (config["agents"]["defaults"]["model"]["primary"] != "runanywhere/glm-5.3-flash") {
        result.details = "the launched model must stay the default";
        return result;
    }
    result.passed = true;
    return result;
}

// Hermes gates a key on the endpoint's own host. The wrong variable name means
// the key is silently dropped and the call goes out unauthenticated.
TestResult test_hermes_key_variable_follows_the_host() {
    TestResult result;
    result.test_name = "hermes_key_variable_follows_the_host";

    const std::string upstream =
        wally::harness::HermesKeyVariable("https://inference.runanywhere.ai/api-dev/v1");
    if (upstream != "RUNANYWHERE_API_KEY") {
        result.details = "expected RUNANYWHERE_API_KEY, got '" + upstream + "'";
        return result;
    }
    if (!wally::harness::HermesKeyVariable("http://127.0.0.1:52431/v1").empty()) {
        result.details = "a loopback server takes no key name";
        return result;
    }
    if (!wally::harness::HermesKeyVariable("https://api.openai.com/v1").empty()) {
        result.details = "OPENAI_API_KEY is host-gated on its own vendor; never borrow the name";
        return result;
    }
    result.passed = true;
    return result;
}

// Hermes has no config-path override, no CLI flag, and a fresh HERMES_HOME costs
// the person's SOUL.md/skills/sessions to deliver one field (see `HermesContextHint`'s
// doc comment) — so the real number is surfaced in a status line rather than
// written anywhere. The line must actually carry the number and the self-serve
// fix, and a caller must be able to tell "nothing to say" from "say it."
TestResult test_hermes_context_hint_surfaces_the_real_window() {
    TestResult result;
    result.test_name = "hermes_context_hint_surfaces_the_real_window";

    const std::string hint = wally::harness::HermesContextHint(1048567);
    if (hint.find("1048567") == std::string::npos) {
        result.details = "the hint must carry the actual token count";
        return result;
    }
    if (hint.find("model.context_length") == std::string::npos) {
        result.details = "the hint must name the self-serve override the person can set";
        return result;
    }
    if (!wally::harness::HermesContextHint(0).empty()) {
        result.details = "an unknown window (0) must produce no hint, not a hint about zero";
        return result;
    }
    if (!wally::harness::HermesContextHint(-1).empty()) {
        result.details = "a negative window must produce no hint either";
        return result;
    }
    result.passed = true;
    return result;
}

// `--provider`/`--model` must lead the argv Hermes actually parses (see
// `HermesArgv`'s doc comment): pinned ahead of `--tui`, and ahead of whatever
// a person's own args carry so a `--provider`/`--model` of theirs still wins
// (Hermes argparse keeps the last value of a repeated flag).
TestResult test_hermes_argv_pins_provider_and_model_ahead_of_the_rest() {
    TestResult result;
    result.test_name = "hermes_argv_pins_provider_and_model_ahead_of_the_rest";

    const std::vector<std::string> bare =
        wally::harness::HermesArgv("glm-5.3-flash", {"--tui"});
    const std::vector<std::string> want_bare{"--provider", "custom", "--model", "glm-5.3-flash",
                                             "--tui"};
    if (bare != want_bare) {
        result.details = "expected --provider/--model ahead of --tui, in that order";
        return result;
    }

    const std::vector<std::string> overridden =
        wally::harness::HermesArgv("glm-5.3-flash", {"--provider", "anthropic", "-z", "hi"});
    const std::vector<std::string> want_overridden{"--provider", "custom", "--model",
                                                    "glm-5.3-flash", "--provider", "anthropic",
                                                    "-z",           "hi"};
    if (overridden != want_overridden) {
        result.details = "our pin must still lead; the person's own --provider rides after it";
        return result;
    }

    const std::vector<std::string> no_args = wally::harness::HermesArgv("qwen3-0.6b", {});
    if (no_args != std::vector<std::string>{"--provider", "custom", "--model", "qwen3-0.6b"}) {
        result.details = "no child args must still produce exactly the pinned four";
        return result;
    }

    result.passed = true;
    return result;
}

// dsh reads our provider out of a settings document it is pointed at, so the
// document is the contract. A missing apiKeyEnv fails every turn with "No API
// key for provider: runanywhere" (dsh 0.1.5), on a loopback route as much as
// an upstream one, so the reference is always present and the launcher puts a
// placeholder in the variable for a local server.
TestResult test_deepseek_settings_carry_the_route() {
    TestResult result;
    result.test_name = "deepseek_settings_carry_the_route";

    const Json upstream = Json::parse(wally::harness::BuildDeepSeekSettings(
        "https://inference.runanywhere.ai/api-dev/v1", "RUNANYWHERE_API_KEY",
        {{"glm-5.3-flash", 1000000, 32768, 0, 0}}));
    const Json& provider = upstream["llm-pi-ai"]["providers"]["runanywhere"];
    if (provider["api"] != "openai-completions" ||
        provider["baseURL"] != "https://inference.runanywhere.ai/api-dev/v1") {
        result.details = "the route must carry our endpoint and protocol";
        return result;
    }
    if (provider["apiKeyEnv"] != "RUNANYWHERE_API_KEY") {
        result.details = "the key must arrive as a reference, never as a literal in the file";
        return result;
    }
    if (provider["models"][0]["contextWindow"] != 1000000 ||
        provider["models"][0]["maxTokens"] != 32768) {
        result.details = "the catalog's real limits must reach the settings document";
        return result;
    }

    const Json local = Json::parse(wally::harness::BuildDeepSeekSettings(
        "http://127.0.0.1:52431/v1", "RUNANYWHERE_API_KEY", {{"qwen3-0.6b", 8192, 0, 0, 0}}));
    if (local["llm-pi-ai"]["providers"]["runanywhere"]["apiKeyEnv"] != "RUNANYWHERE_API_KEY") {
        result.details = "a local route must still name the key reference, or dsh refuses the turn";
        return result;
    }

    // The whole catalog reaches dsh's settings, not just the launched model.
    const Json many = Json::parse(wally::harness::BuildDeepSeekSettings(
        "https://inference.runanywhere.ai/api-dev/v1", "RUNANYWHERE_API_KEY",
        {{"glm-5.3-flash", 0, 0, 0, 0}, {"qwen3.8-27b", 0, 0, 0, 0}, {"gemma-4", 0, 0, 0, 0}}));
    if (many["llm-pi-ai"]["providers"]["runanywhere"]["models"].size() != 3) {
        result.details = "every catalog model must reach the dsh settings document";
        return result;
    }
    result.passed = true;
    return result;
}

// The overlay is the only thing that reaches dsh: it repoints the settings row
// at our document and names our provider for a fresh agent. Getting either row
// id wrong is reported on stderr as an unmatched target and otherwise ignored.
TestResult test_deepseek_patch_targets_both_rows() {
    TestResult result;
    result.test_name = "deepseek_patch_targets_both_rows";

    const std::string patch = wally::harness::BuildDeepSeekPatch("/tmp/x.json", "glm-5.3-flash");
    if (patch.find("- id: settings\n") == std::string::npos ||
        patch.find("path: '/tmp/x.json'") == std::string::npos) {
        result.details = "the settings row must be repointed at our document";
        return result;
    }
    if (patch.find("- id: agent-default-model\n") == std::string::npos ||
        patch.find("provider: runanywhere") == std::string::npos ||
        patch.find("model: 'glm-5.3-flash'") == std::string::npos) {
        result.details = "a fresh agent must start on our provider and model";
        return result;
    }
    result.passed = true;
    return result;
}

// dsh's interactive surface is a browser and its terminal entry is one-shot, so
// which one runs is decided by whether the person gave it something to do.
TestResult test_deepseek_prompt_picks_headless() {
    TestResult result;
    result.test_name = "deepseek_prompt_picks_headless";

    if (wally::harness::DeepSeekWantsHeadless({})) {
        result.details = "no arguments means the web ui";
        return result;
    }
    if (wally::harness::DeepSeekWantsHeadless({"--port", "8080"})) {
        result.details = "flags belong to the web app, not to a prompt";
        return result;
    }
    if (!wally::harness::DeepSeekWantsHeadless({"run the tests"})) {
        result.details = "a prompt means headless";
        return result;
    }
    // The value after a flag is not a prompt, which is the case the first
    // version of this got wrong.
    if (wally::harness::DeepSeekWantsHeadless({"--no-open", "--port", "8080"})) {
        result.details = "a flag's value must not be read as a prompt";
        return result;
    }
    result.passed = true;
    return result;
}


// The context a local server is started with never drops below the 8192 every
// launch used before, and never exceeds what the catalog says the model was
// trained on. The RAM tier in between depends on the machine, so only the two
// bounds and the unknown-model path are pinned here.
TestResult test_local_context_size_respects_floor_and_model_window() {
    TestResult result;
    result.test_name = "local_context_size_respects_floor_and_model_window";

    // qwen3-0.6b's catalog window is 4096, below the floor: the floor wins.
    if (wally::harness::LocalContextSize("qwen3-0.6b") != 8192) {
        result.details = "a model window under 8192 must not pull the server below the floor";
        return result;
    }
    // A model the catalog has never heard of gets the machine's tier, which is
    // at least the floor and a power of two the server accepts.
    const std::int64_t unknown = wally::harness::LocalContextSize("hf.co/someone/some-model");
    if (unknown < 8192 || (unknown & (unknown - 1)) != 0) {
        result.details = "an unknown model must get the RAM tier, >= 8192 and a power of two";
        return result;
    }
    // A catalog model is never given more than the tier an unknown one gets.
    if (wally::harness::LocalContextSize("bonsai-27b") > unknown) {
        result.details = "a catalog model must not exceed the machine's tier";
        return result;
    }
    result.passed = true;
    return result;
}



}  // namespace

#if defined(_WIN32)
TestResult test_windows_args_survive_the_spawn_command_line() {
    TestResult result;
    result.test_name = "windows_args_survive_the_spawn_command_line";

    const struct {
        const char* in;
        const char* want;
    } cases[] = {
        {"plain", "plain"},
        {"", "\"\""},
        {"fix the tests", "\"fix the tests\""},
        {"say \"hi\"", "\"say \\\"hi\\\"\""},
        {"C:\dir with space\\", "\"C:\dir with space\\\\\""},
    };
    for (const auto& c : cases) {
        const std::string got = wally::harness::QuoteWindowsArg(c.in);
        if (got != c.want) {
            result.details = std::string("QuoteWindowsArg(") + c.in + ") = " + got + ", want " + c.want;
            return result;
        }
    }
    result.passed = true;
    return result;
}
#endif

int main(int argc, char** argv) {
    TestSuite suite("wally_harness");
    suite.add("local_context_size_respects_floor_and_model_window",
              test_local_context_size_respects_floor_and_model_window);
    suite.add("model_id_rejects_empty_and_control_characters",
              test_model_id_rejects_empty_and_control_characters);
    suite.add("model_id_rejects_xml_and_path_structural_characters",
              test_model_id_rejects_xml_and_path_structural_characters);
    suite.add("model_id_accepts_ordinary_ids", test_model_id_accepts_ordinary_ids);
    suite.add("verify_cloud_session_rejects_unverifiable_token",
              test_verify_cloud_session_rejects_unverifiable_token);
    suite.add("verify_cloud_session_refreshes_and_reverifies",
              test_verify_cloud_session_refreshes_and_reverifies);
    suite.add("verify_cloud_session_accepts_real_session",
              test_verify_cloud_session_accepts_real_session);
    suite.add("verify_cloud_session_rate_limit_is_unverified_not_bad",
              test_verify_cloud_session_rate_limit_is_unverified_not_bad);
    suite.add("verify_cloud_session_rejected_session_is_not_unverified",
              test_verify_cloud_session_rejected_session_is_not_unverified);
    suite.add("verify_cloud_session_rate_limited_refresh_is_unverified_not_bad",
              test_verify_cloud_session_rate_limited_refresh_is_unverified_not_bad);
    suite.add("openclaw_config_selects_our_provider_and_model",
              test_openclaw_config_selects_our_provider_and_model);
    suite.add("openclaw_config_substitutes_a_key_for_a_local_endpoint",
              test_openclaw_config_substitutes_a_key_for_a_local_endpoint);
    suite.add("agent_table_rows_are_usable_subcommands",
              test_agent_table_rows_are_usable_subcommands);
    suite.add("openclaw_config_lists_every_catalog_model",
              test_openclaw_config_lists_every_catalog_model);
    suite.add("openclaw_config_preserves_the_existing_document",
              test_openclaw_config_preserves_the_existing_document);
    suite.add("hermes_key_variable_follows_the_host",
              test_hermes_key_variable_follows_the_host);
    suite.add("hermes_context_hint_surfaces_the_real_window",
              test_hermes_context_hint_surfaces_the_real_window);
    suite.add("deepseek_settings_carry_the_route", test_deepseek_settings_carry_the_route);
    suite.add("deepseek_patch_targets_both_rows", test_deepseek_patch_targets_both_rows);
    suite.add("deepseek_prompt_picks_headless", test_deepseek_prompt_picks_headless);
    suite.add("hermes_argv_pins_provider_and_model_ahead_of_the_rest",
              test_hermes_argv_pins_provider_and_model_ahead_of_the_rest);
#if defined(_WIN32)
    suite.add("windows_args_survive_the_spawn_command_line",
              test_windows_args_survive_the_spawn_command_line);
#endif
    return suite.run(argc, argv);
}
