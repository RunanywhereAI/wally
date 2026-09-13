#include "test_common.h"

#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <nlohmann/json.hpp>
#include <stdexcept>
#include <string>
#include <vector>

#include "account/console.h"
#include "account/credentials.h"
#include "harness/opencode.h"

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
        path_ = fs::temp_directory_path() / ("wally-opencode-test-" + std::to_string(nonce));
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

bool Seed(const fs::path& profile, long long expires_at, std::string* error) {
    Environment scoped_profile("WALLY_PROFILE_DIR", profile.string().c_str());
    wally::account::Credentials credentials;
    credentials.console_url = "https://console.runanywhere.ai";
    credentials.email = "developer@example.test";
    credentials.access_token = "old-access-token";
    credentials.refresh_token = "refresh-token";
    credentials.expires_at = expires_at;
    return wally::account::Save(credentials, error);
}

TestResult test_ephemeral_config_and_passthrough() {
    TestResult result;
    result.test_name = "ephemeral_config_and_passthrough";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    Environment existing("OPENCODE_CONFIG_CONTENT", "{\"keep\":true}");
    std::string error;
    if (!Seed(temporary.path(), Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool spawned = false;
    const std::vector<std::string> arguments = {"run", "--agent", "build", "two words"};
    // The launch path verifies the session against the console before it starts
    // anything, so the console has to be answered here. A default ConsoleClient
    // would put a real request on the wire, which a unit test must never do.
    const wally::account::ConsoleClient console(
        [](const wally::account::HttpRequest& request, wally::account::HttpResponse* response,
           std::string*) {
            if (!request.url.ends_with("/v1/auth/me")) {
                return false;
            }
            response->status = 200;
            response->body = Json{{"email", "developer@example.test"}}.dump();
            return true;
        });
    const int status = wally::harness::LaunchOpenCodeCloud(
        "glm-5.3", arguments, console,
        [&](const std::string& executable, const std::vector<std::string>& received) {
            spawned = true;
            if (executable != "opencode" || received != arguments) {
                return 91;
            }
            const char* raw = std::getenv("OPENCODE_CONFIG_CONTENT");
            if (raw == nullptr) {
                return 92;
            }
            const Json config = Json::parse(raw);
            const Json& provider = config.at("provider").at("runanywhere");
            if (config.at("model") != "runanywhere/glm-5.3" ||
                provider.at("npm") != "@ai-sdk/openai-compatible" ||
                provider.at("options").at("baseURL") != "https://console.runanywhere.ai/v1" ||
                provider.at("options").at("apiKey") != "old-access-token" ||
                provider.at("models").at("glm-5.3").at("name") != "glm-5.3") {
                return 93;
            }
            return 0;
        });
    const char* restored = std::getenv("OPENCODE_CONFIG_CONTENT");
    if (status != 0 || !spawned || restored == nullptr ||
        std::string(restored) != "{\"keep\":true}") {
        result.details = "launch did not preserve arguments and the existing environment";
        result.actual = std::to_string(status);
        return result;
    }
    if (std::distance(fs::directory_iterator(temporary.path()), fs::directory_iterator{}) != 1) {
        result.details = "launch wrote a tool or project configuration file";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_refreshes_expired_session_without_sdk_bootstrap() {
    TestResult result;
    result.test_name = "refreshes_expired_session_without_sdk_bootstrap";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    Environment api_key("RUNANYWHERE_API_KEY", nullptr);
    Environment environment("RUNANYWHERE_ENVIRONMENT", nullptr);
    Environment config("OPENCODE_CONFIG_CONTENT", nullptr);
    std::string error;
    if (!Seed(temporary.path(), Now() - 1, &error)) {
        result.details = error;
        return result;
    }

    bool refreshed = false;
    wally::account::ConsoleClient console([&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        if (request.url.ends_with("/v1/auth/me")) {
            response->status = 200;
            response->body = Json{{"email", "developer@example.test"}}.dump();
            return true;
        }
        if (!request.url.ends_with("/v1/auth/cli/refresh") || request.bearer_token.size() != 0 ||
            Json::parse(request.body).at("refresh_token") != "refresh-token") {
            return false;
        }
        refreshed = true;
        response->status = 200;
        response->body = Json{{"access_token", "new-access-token"},
                              {"refresh_token", "new-refresh-token"},
                              {"expires_in", 7200}}
                             .dump();
        return true;
    });

    const int status = wally::harness::LaunchOpenCodeCloud(
        "hosted-model", {}, console, [&](const std::string&, const std::vector<std::string>&) {
            const Json value = Json::parse(std::getenv("OPENCODE_CONFIG_CONTENT"));
            return value.at("provider").at("runanywhere").at("options").at("apiKey") ==
                           "new-access-token"
                       ? 0
                       : 94;
        });
    if (status != 0 || !refreshed || std::getenv("OPENCODE_CONFIG_CONTENT") != nullptr) {
        result.details = "expired cloud credentials were not refreshed ephemerally";
        result.actual = std::to_string(status);
        return result;
    }

    wally::account::Credentials stored;
    if (!wally::account::Load(&stored, &error) || stored.access_token != "new-access-token" ||
        stored.refresh_token != "new-refresh-token" || stored.expires_at <= Now()) {
        result.details = error.empty() ? "refreshed session was not stored" : error;
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_restores_config_when_spawn_throws() {
    TestResult result;
    result.test_name = "restores_config_when_spawn_throws";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    Environment config("OPENCODE_CONFIG_CONTENT", "original-value");
    std::string error;
    if (!Seed(temporary.path(), Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool threw = false;
    try {
        const wally::account::ConsoleClient console(
            [](const wally::account::HttpRequest& request, wally::account::HttpResponse* response,
               std::string*) {
                if (!request.url.ends_with("/v1/auth/me")) {
                    return false;
                }
                response->status = 200;
                response->body = Json{{"email", "developer@example.test"}}.dump();
                return true;
            });
        static_cast<void>(wally::harness::LaunchOpenCodeCloud(
            "hosted-model", {}, console,
            [](const std::string&, const std::vector<std::string>&) -> int {
                throw std::runtime_error("synthetic spawn failure");
            }));
    } catch (const std::runtime_error&) {
        threw = true;
    }
    const char* restored = std::getenv("OPENCODE_CONFIG_CONTENT");
    if (!threw || restored == nullptr || std::string(restored) != "original-value") {
        result.details = "temporary OpenCode config survived an exceptional child launch";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

TestResult test_config_injects_limit_and_cost() {
    TestResult result;
    result.test_name = "config_injects_limit_and_cost";
    using Json = nlohmann::json;

    // glm-like: 1M context, no separate output cap (0 -> sane default), and
    // $0.60/$2.20 per Mtok (600000/2200000 micro-dollars).
    const Json j = Json::parse(wally::harness::BuildOpenCodeCloudConfig(
        "glm-5.3-flash", "https://x/v1", "tok", 1048576, 0, 600000, 2200000));
    const Json m = j["provider"]["runanywhere"]["models"]["glm-5.3-flash"];
    const double cin = m.value("cost", Json::object()).value("input", -1.0);
    const double cout = m.value("cost", Json::object()).value("output", -1.0);
    if (m["limit"]["context"].get<std::int64_t>() != 1048576 ||
        m["limit"]["output"].get<std::int64_t>() != 65536 ||
        cin < 0.5999 || cin > 0.6001 || cout < 2.1999 || cout > 2.2001) {
        result.details = "limit/cost injection wrong: " + m.dump();
        return result;
    }
    // No metadata (all zeros) -> neither block is emitted.
    const Json bare = Json::parse(
        wally::harness::BuildOpenCodeCloudConfig("m", "https://x/v1", "tok", 0, 0, 0, 0));
    if (bare["provider"]["runanywhere"]["models"]["m"].contains("limit") ||
        bare["provider"]["runanywhere"]["models"]["m"].contains("cost")) {
        result.details = "empty metadata should omit limit and cost";
        return result;
    }
    result.passed = true;
    return result;
}

int main(int argc, char** argv) {
    TestSuite suite("wally_opencode");
    suite.add("config_injects_limit_and_cost", test_config_injects_limit_and_cost);
    suite.add("ephemeral_config_and_passthrough", test_ephemeral_config_and_passthrough);
    suite.add("refreshes_expired_session_without_sdk_bootstrap",
              test_refreshes_expired_session_without_sdk_bootstrap);
    suite.add("restores_config_when_spawn_throws", test_restores_config_when_spawn_throws);
    return suite.run(argc, argv);
}
