#include "test_common.h"

#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <nlohmann/json.hpp>
#include <sstream>
#include <stdexcept>
#include <string>
#include <vector>

#include "account/console.h"
#include "account/credentials.h"
#include "harness/codex.h"

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
        path_ = fs::temp_directory_path() / ("wally-codex-test-" + std::to_string(nonce));
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

std::string ReadFile(const fs::path& path) {
    std::ifstream in(path, std::ios::binary);
    std::ostringstream buffer;
    buffer << in.rdbuf();
    return buffer.str();
}

wally::account::ConsoleClient WhoAmIConsole() {
    return wally::account::ConsoleClient(
        [](const wally::account::HttpRequest& request, wally::account::HttpResponse* response,
           std::string*) {
            if (!request.url.ends_with("/v1/auth/me")) {
                return false;
            }
            response->status = 200;
            response->body = Json{{"email", "developer@example.test"}}.dump();
            return true;
        });
}

// The config names the env var and never carries the key, and the provider
// talks the Responses API.
TestResult test_build_config_names_key_env_and_responses() {
    TestResult result;
    result.test_name = "build_config_names_key_env_and_responses";
    const std::string config =
        wally::harness::BuildCodexConfig("glm-5.3-flash", "https://console.runanywhere.ai/v1");
    const bool ok = config.find("model = \"glm-5.3-flash\"") != std::string::npos &&
                    config.find("base_url = \"https://console.runanywhere.ai/v1\"") !=
                        std::string::npos &&
                    config.find("env_key = \"WALLY_CODEX_API_KEY\"") != std::string::npos &&
                    config.find("wire_api = \"responses\"") != std::string::npos &&
                    config.find("old-access-token") == std::string::npos;
    if (!ok) {
        result.details = "config.toml missing provider fields or leaked the key";
        result.actual = config;
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_ephemeral_home_key_env_and_passthrough() {
    TestResult result;
    result.test_name = "ephemeral_home_key_env_and_passthrough";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    Environment existing_home("CODEX_HOME", nullptr);
    Environment existing_key("WALLY_CODEX_API_KEY", nullptr);
    std::string error;
    if (!Seed(temporary.path(), Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool spawned = false;
    fs::path captured_home;
    const std::vector<std::string> arguments = {"exec", "write a haiku"};
    const wally::account::ConsoleClient console = WhoAmIConsole();
    const int status = wally::harness::LaunchCodexCloud(
        "glm-5.3", arguments, console,
        [&](const std::string& executable, const std::vector<std::string>& received) {
            spawned = true;
            if (executable != "codex" || received != arguments) {
                return 91;
            }
            const char* home = std::getenv("CODEX_HOME");
            const char* key = std::getenv("WALLY_CODEX_API_KEY");
            if (home == nullptr || key == nullptr || std::string(key) != "old-access-token") {
                return 92;
            }
            captured_home = home;
            const std::string config = ReadFile(captured_home / "config.toml");
            if (config.find("wire_api = \"responses\"") == std::string::npos ||
                config.find("base_url = \"https://console.runanywhere.ai/v1\"") ==
                    std::string::npos ||
                // The credential lives in the env var, never in the file.
                config.find("old-access-token") != std::string::npos) {
                return 93;
            }
            return 0;
        });

    if (status != 0 || !spawned) {
        result.details = "launch did not wire Codex and pass its arguments through";
        result.actual = std::to_string(status);
        return result;
    }
    // The throwaway CODEX_HOME and both env vars are gone afterwards.
    if (fs::exists(captured_home) || std::getenv("CODEX_HOME") != nullptr ||
        std::getenv("WALLY_CODEX_API_KEY") != nullptr) {
        result.details = "temporary CODEX_HOME or key env survived the launch";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_restores_env_when_spawn_throws() {
    TestResult result;
    result.test_name = "restores_env_when_spawn_throws";
    TemporaryDirectory temporary;
    Environment profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    Environment home("CODEX_HOME", "/some/original/home");
    Environment key("WALLY_CODEX_API_KEY", "original-key");
    std::string error;
    if (!Seed(temporary.path(), Now() + 3600, &error)) {
        result.details = error;
        return result;
    }

    bool threw = false;
    try {
        const wally::account::ConsoleClient console = WhoAmIConsole();
        static_cast<void>(wally::harness::LaunchCodexCloud(
            "hosted-model", {}, console,
            [](const std::string&, const std::vector<std::string>&) -> int {
                throw std::runtime_error("synthetic spawn failure");
            }));
    } catch (const std::runtime_error&) {
        threw = true;
    }
    const char* home_now = std::getenv("CODEX_HOME");
    const char* key_now = std::getenv("WALLY_CODEX_API_KEY");
    if (!threw || home_now == nullptr || std::string(home_now) != "/some/original/home" ||
        key_now == nullptr || std::string(key_now) != "original-key") {
        result.details = "Codex env did not restore after an exceptional child launch";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_codex");
    suite.add("build_config_names_key_env_and_responses",
              test_build_config_names_key_env_and_responses);
    suite.add("ephemeral_home_key_env_and_passthrough",
              test_ephemeral_home_key_env_and_passthrough);
    suite.add("restores_env_when_spawn_throws", test_restores_env_when_spawn_throws);
    return suite.run(argc, argv);
}
