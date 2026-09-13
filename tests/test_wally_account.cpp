#include "test_common.h"

#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <filesystem>
#include <fstream>
#include <future>
#include <nlohmann/json.hpp>
#include <string>
#include <thread>
#include <vector>

#include <httplib.h>

#if !defined(_WIN32)
#include <cstdio>
#include <unistd.h>

#include <fcntl.h>
#include <sys/stat.h>
#endif

#include "account/console.h"
#include "account/credentials.h"

namespace {

namespace fs = std::filesystem;
using Json = nlohmann::json;

class EnvVar {
   public:
    EnvVar(const char* name, const char* value) : name_(name) {
        if (const char* previous = std::getenv(name)) {
            had_previous_ = true;
            previous_ = previous;
        }
#if defined(_WIN32)
        _putenv_s(name, value != nullptr ? value : "");
#else
        value != nullptr ? setenv(name, value, 1) : unsetenv(name);
#endif
    }

    ~EnvVar() {
#if defined(_WIN32)
        _putenv_s(name_.c_str(), had_previous_ ? previous_.c_str() : "");
#else
        had_previous_ ? setenv(name_.c_str(), previous_.c_str(), 1) : unsetenv(name_.c_str());
#endif
    }

   private:
    std::string name_;
    std::string previous_;
    bool had_previous_ = false;
};

class TempDirectory {
   public:
    TempDirectory() {
        const auto nonce = std::chrono::steady_clock::now().time_since_epoch().count();
        path_ = fs::temp_directory_path() / ("wally-account-test-" + std::to_string(nonce));
        fs::create_directories(path_);
    }
    ~TempDirectory() {
        std::error_code ignored;
        fs::remove_all(path_, ignored);
    }
    const fs::path& path() const { return path_; }

   private:
    fs::path path_;
};

TestResult test_console_url_validation() {
    TestResult result;
    result.test_name = "console_url_validation";

    struct Accepted {
        const char* input;
        const char* normalized;
    };
    const Accepted accepted[] = {
        {"https://console.runanywhere.ai", "https://console.runanywhere.ai"},
        {"HTTPS://CONSOLE.RUNANYWHERE.AI/", "https://console.runanywhere.ai"},
        {"http://localhost:8080", "http://localhost:8080"},
        {"http://127.0.0.1:8002", "http://127.0.0.1:8002"},
        {"http://[::1]:9000", "http://[::1]:9000"},
        // Development is a path prefix on the production host, not its own one.
        {"https://inference.runanywhere.ai/api-dev", "https://inference.runanywhere.ai/api-dev"},
        {"https://inference.runanywhere.ai/api-dev/", "https://inference.runanywhere.ai/api-dev"},
        {"http://localhost:8080/api-dev", "http://localhost:8080/api-dev"},
    };
    for (const Accepted& test : accepted) {
        std::string normalized;
        std::string error;
        if (!wally::account::NormalizeConsoleUrl(test.input, &normalized, &error) ||
            normalized != test.normalized) {
            result.details = std::string("rejected safe origin: ") + test.input + " " + error;
            result.expected = test.normalized;
            result.actual = normalized;
            return result;
        }
    }

    const char* rejected[] = {
        "http://console.runanywhere.ai",
        "http://localhost.evil.example",
        "http://127.0.0.1.evil.example",
        "http://[::1].evil.example",
        "http://localhost@evil.example",
        "https://user:password@console.runanywhere.ai",
        "https://console.runanywhere.ai?query=1",
        "https://console.runanywhere.ai/api-dev?query=1",
        "https://console.runanywhere.ai/api-dev#fragment",
        "https://console.runanywhere.ai//api-dev",
        "https://console.runanywhere.ai/../api-dev",
        "https://console.runanywhere.ai/api/../../dev",
        "https://console.runanywhere.ai:",
        "http://[::1]:",
        "https://",
        "file:///tmp/credentials",
    };
    for (const char* input : rejected) {
        std::string normalized;
        std::string error;
        if (wally::account::NormalizeConsoleUrl(input, &normalized, &error)) {
            result.details = std::string("accepted unsafe origin: ") + input;
            return result;
        }
    }

    if (!wally::account::BrowserUrlIsSafe("https://console.runanywhere.ai/device?code=ABCD-EFGH") ||
        !wally::account::BrowserUrlIsSafe("http://localhost:8080/device?code=ABCD") ||
        wally::account::BrowserUrlIsSafe("http://localhost.evil.example/device") ||
        !wally::account::BrowserUrlMatchesConsole(
            "https://console.runanywhere.ai/device?code=ABCD-EFGH",
            "https://console.runanywhere.ai") ||
        !wally::account::BrowserUrlMatchesConsole(
            "https://inference.runanywhere.ai/cloud/cli?code=ABCD",
            "https://inference.runanywhere.ai/api-dev") ||
        wally::account::BrowserUrlMatchesConsole("https://auth.attacker.example/device",
                                                "https://inference.runanywhere.ai/api-dev") ||
        wally::account::BrowserUrlMatchesConsole("https://auth.attacker.example/device",
                                                "https://console.runanywhere.ai")) {
        result.details = "browser URL policy does not match the origin policy";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_credential_roundtrip_and_permissions() {
    TestResult result;
    result.test_name = "credential_roundtrip_and_permissions";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    EnvVar console("WALLY_CONSOLE_URL", nullptr);

    wally::account::Credentials expected;
    expected.console_url = "https://CONSOLE.RUNANYWHERE.AI/";
    expected.email = "dev+\"json\"@example.test";
    expected.access_token = "access-secret-that-must-not-be-logged";
    expected.refresh_token = "refresh-secret-that-must-not-be-logged";
    expected.expires_at = 123456789;

    std::string error;
    if (!wally::account::Save(expected, &error)) {
        result.details = error;
        return result;
    }

#if !defined(_WIN32)
    struct stat directory{};
    struct stat file{};
    if (::stat(temporary.path().c_str(), &directory) != 0 ||
        ::stat(wally::account::CredentialsPath().c_str(), &file) != 0 ||
        (directory.st_mode & 0777) != 0700 || (file.st_mode & 0777) != 0600) {
        result.details = "credentials must be stored in mode 0700/0600";
        return result;
    }
#endif

    wally::account::Credentials actual;
    if (!wally::account::Load(&actual, &error)) {
        result.details = error;
        return result;
    }
    if (actual.console_url != "https://console.runanywhere.ai" || actual.email != expected.email ||
        actual.access_token != expected.access_token ||
        actual.refresh_token != expected.refresh_token ||
        actual.expires_at != expected.expires_at) {
        result.details = "credential JSON did not round-trip exactly";
        return result;
    }
    if (!wally::account::Clear(&error) || fs::exists(wally::account::CredentialsPath())) {
        result.details = error.empty() ? "credential file still exists after clear" : error;
        return result;
    }
    result.passed = true;
    return result;
}

// No stored session at all still yields a usable console origin. Portable, and
// the only form of the fallback that exists on every platform.
TestResult test_credentials_default_console_url_without_a_file() {
    TestResult result;
    result.test_name = "credentials_default_console_url_without_a_file";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    EnvVar console("WALLY_CONSOLE_URL", nullptr);

    wally::account::Credentials credentials;
    std::string error;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error.empty() ? "load failed with no session file present" : error;
        return result;
    }
    // Against DefaultConsoleUrl(), not a literal. What this test is about is
    // that the fallback happens at all; which host it lands on is pinned by
    // the_api_host_and_the_browser_host_stay_apart, and duplicating the string
    // here only bought two failures for one change.
    if (credentials.console_url != wally::account::DefaultConsoleUrl()) {
        result.expected = wally::account::DefaultConsoleUrl();
        result.actual = credentials.console_url;
        result.details = "no session file must still give the default console origin";
        return result;
    }
    if (credentials.signed_in()) {
        result.details = "no session file must not read as signed in";
        return result;
    }
    result.passed = true;
    return result;
}

#if !defined(_WIN32)
// A document that simply omits console_url is not a malformed one: it must
// fall back to DefaultConsoleUrl() cleanly rather than failing with "console
// URL must be HTTPS" on a value that was never actually set.
//
// POSIX only, and not by preference. The fixture is a hand-written document,
// and the scenario it stands for is a hand-edited or partially-written file.
// On Windows the store is `credentials.dat`, DPAPI ciphertext, so neither the
// fixture nor the scenario can exist: plaintext there fails to decrypt long
// before any of this parsing runs, which is what
// credentials_reject_a_document_they_cannot_unlock covers instead.
TestResult test_credentials_missing_console_url_falls_back() {
    TestResult result;
    result.test_name = "credentials_missing_console_url_falls_back";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    EnvVar console("WALLY_CONSOLE_URL", nullptr);

    std::ofstream raw(wally::account::CredentialsPath());
    raw << Json{{"email", "dev@example.test"}, {"access_token", "a-token"}}.dump();
    raw.close();
    ::chmod(wally::account::CredentialsPath().c_str(), 0600);

    wally::account::Credentials credentials;
    std::string error;
    if (!wally::account::Load(&credentials, &error)) {
        result.details = error.empty() ? "load failed on a document missing console_url" : error;
        return result;
    }
    if (credentials.console_url != wally::account::DefaultConsoleUrl()) {
        result.expected = wally::account::DefaultConsoleUrl();
        result.actual = credentials.console_url;
        result.details = "a missing console_url must fall back to the default, not error";
        return result;
    }
    result.passed = true;
    return result;
}
#else
// The Windows half of the contract above. A session file that will not decrypt
// has to be reported, not skipped past as if there were no session and not
// crashed on. DPAPI keys are per-user, so this is what a credentials.dat copied
// from another machine or account actually looks like.
TestResult test_credentials_reject_a_document_they_cannot_unlock() {
    TestResult result;
    result.test_name = "credentials_reject_a_document_they_cannot_unlock";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    EnvVar console("WALLY_CONSOLE_URL", nullptr);

    std::ofstream raw(wally::account::CredentialsPath(), std::ios::binary);
    raw << Json{{"email", "dev@example.test"}, {"access_token", "a-token"}}.dump();
    raw.close();

    wally::account::Credentials credentials;
    std::string error;
    if (wally::account::Load(&credentials, &error)) {
        result.details = "a session file that does not decrypt must not load";
        return result;
    }
    if (error.empty()) {
        result.details = "failing to unlock the session must say so";
        return result;
    }
    if (credentials.signed_in()) {
        result.details = "a failed load must not leave a usable session behind";
        return result;
    }
    result.passed = true;
    return result;
}
#endif

#if !defined(_WIN32)
// A group/world-readable credentials.json is tightened to 0600 silently
// today; Load() must say so rather than leave the reader unaware their
// bearer token was ever exposed.
TestResult test_credentials_warns_on_exposed_permissions() {
    TestResult result;
    result.test_name = "credentials_warns_on_exposed_permissions";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());
    EnvVar console("WALLY_CONSOLE_URL", nullptr);

    wally::account::Credentials seed;
    seed.console_url = "https://console.runanywhere.ai";
    seed.email = "dev@example.test";
    seed.access_token = "a-token";
    seed.expires_at = 0;
    std::string error;
    if (!wally::account::Save(seed, &error)) {
        result.details = error;
        return result;
    }
    const std::string path = wally::account::CredentialsPath();
    if (::chmod(path.c_str(), 0644) != 0) {
        result.details = "could not widen permissions for the test fixture";
        return result;
    }

    // Redirect stderr to a temp file for the duration of the Load() call, the
    // only surface a low-level, SDK-independent module like credentials.cpp
    // can use to say something without pulling in the CLI's output layer.
    const std::string capture_path = temporary.path().string() + "/stderr-capture.txt";
    const int saved_stderr = ::dup(fileno(stderr));
    const int capture_fd = ::open(capture_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (saved_stderr < 0 || capture_fd < 0) {
        result.details = "could not set up stderr capture";
        return result;
    }
    std::fflush(stderr);
    ::dup2(capture_fd, fileno(stderr));
    ::close(capture_fd);

    wally::account::Credentials loaded;
    const bool ok = wally::account::Load(&loaded, &error);

    std::fflush(stderr);
    ::dup2(saved_stderr, fileno(stderr));
    ::close(saved_stderr);

    if (!ok) {
        result.details = error;
        return result;
    }

    struct stat after {};
    if (::stat(path.c_str(), &after) != 0 || (after.st_mode & 0777) != 0600) {
        result.details = "permissions must still be tightened to 0600";
        return result;
    }

    std::ifstream capture(capture_path);
    const std::string warning((std::istreambuf_iterator<char>(capture)),
                              std::istreambuf_iterator<char>());
    if (warning.find("warning") == std::string::npos ||
        warning.find(path) == std::string::npos) {
        result.details = "Load() must warn, naming the file, when it was exposed";
        result.actual = warning;
        return result;
    }
    result.passed = true;
    return result;
}
#endif  // !defined(_WIN32)

TestResult test_console_client_contract() {
    TestResult result;
    result.test_name = "console_client_contract";
    std::vector<wally::account::HttpRequest> requests;
    int polls = 0;
    wally::account::Transport transport = [&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response, std::string*) {
        requests.push_back(request);
        if (request.url.ends_with("/auth/cli/start")) {
            response->status = 200;
            response->body =
                Json{{"request_code", "ABCD-EFGH"},
                     {"poll_secret", "poll-secret"},
                     {"verification_url", "https://console.runanywhere.ai/device?code=ABCD-EFGH"},
                     {"expires_in", 300},
                     {"interval", 1}}
                    .dump();
        } else if (request.url.ends_with("/auth/cli/poll")) {
            response->status = 200;
            response->body = polls++ == 0 ? Json{{"status", "pending"}}.dump()
                                          : Json{{"status", "approved"},
                                                 {"access_token", "access-one"},
                                                 {"refresh_token", "refresh-one"},
                                                 {"email", "dev@example.test"},
                                                 {"expires_in", 3600}}
                                                .dump();
        } else if (request.url.ends_with("/auth/cli/refresh")) {
            response->status = 200;
            response->body = Json{{"access_token", "access-two"},
                                  {"refresh_token", "refresh-two"},
                                  {"expires_in", 3600}}
                                 .dump();
        } else if (request.url.ends_with("/v1/me")) {
            response->status = 200;
            response->body = Json{{"email", "dev@example.test"}}.dump();
        } else if (request.url.ends_with("/auth/cli/revoke")) {
            response->status = 204;
        } else {
            return false;
        }
        return true;
    };

    wally::account::ConsoleClient client(transport);
    std::string error;
    wally::account::Authorization authorization;
    if (!client.BeginAuthorization("https://console.runanywhere.ai", "test-host", &authorization,
                                   &error) ||
        authorization.request_code != "ABCD-EFGH" || authorization.interval != 1) {
        result.details = error.empty() ? "authorization response mismatch" : error;
        return result;
    }
    wally::account::Grant grant;
    if (client.Poll("https://console.runanywhere.ai", authorization, &grant, &error) !=
            wally::account::PollResult::Pending ||
        client.Poll("https://console.runanywhere.ai", authorization, &grant, &error) !=
            wally::account::PollResult::Approved ||
        grant.access_token != "access-one" || grant.refresh_token != "refresh-one") {
        result.details = error.empty() ? "poll contract mismatch" : error;
        return result;
    }
    wally::account::Identity identity;
    if (client.WhoAmI("https://console.runanywhere.ai", grant.access_token, &identity, &error) !=
        wally::account::IdentityResult::Ok) {
        result.details = error.empty() ? "identity contract mismatch" : error;
        return result;
    }
    wally::account::Grant refreshed;
    if (!client.Refresh("https://console.runanywhere.ai", grant.refresh_token, &refreshed,
                        &error) ||
        refreshed.access_token != "access-two" || refreshed.refresh_token != "refresh-two" ||
        !client.Revoke("https://console.runanywhere.ai", refreshed.access_token,
                       refreshed.refresh_token, &error)) {
        result.details = error.empty() ? "refresh/revoke contract mismatch" : error;
        return result;
    }

    if (requests.size() != 6 || !requests[0].bearer_token.empty() ||
        !requests[1].bearer_token.empty() || !requests[2].bearer_token.empty() ||
        requests[3].bearer_token != "access-one" || !requests[4].bearer_token.empty() ||
        requests[5].bearer_token != "access-two") {
        result.details = "bearer tokens were attached to the wrong endpoint";
        return result;
    }
    const Json start = Json::parse(requests[0].body);
    const Json poll = Json::parse(requests[1].body);
    const Json refresh = Json::parse(requests[4].body);
    const Json revoke = Json::parse(requests[5].body);
    if (start.value("hostname", "") != "test-host" ||
        poll.value("poll_secret", "") != "poll-secret" ||
        refresh.value("refresh_token", "") != "refresh-one" ||
        revoke.value("refresh_token", "") != "refresh-two") {
        result.details = "request JSON contract mismatch";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_console_errors_do_not_echo_secrets() {
    TestResult result;
    result.test_name = "console_errors_do_not_echo_secrets";
    const std::string secret = "access-secret-from-server";
    wally::account::ConsoleClient client([&](const wally::account::HttpRequest&,
                                            wally::account::HttpResponse* response, std::string*) {
        response->status = 500;
        response->body = Json{{"detail", secret}}.dump();
        return true;
    });
    wally::account::Authorization authorization;
    std::string error;
    if (client.BeginAuthorization("https://console.runanywhere.ai", "host", &authorization,
                                  &error) ||
        error.find(secret) != std::string::npos || error.find("HTTP 500") == std::string::npos) {
        result.details = "HTTP error exposed the response body or lost its status";
        return result;
    }

    wally::account::ConsoleClient malformed([&](const wally::account::HttpRequest&,
                                               wally::account::HttpResponse* response,
                                               std::string*) {
        response->status = 200;
        response->body = "{\"access_token\":\"" + secret;
        return true;
    });
    error.clear();
    if (malformed.BeginAuthorization("https://console.runanywhere.ai", "host", &authorization,
                                     &error) ||
        error.find(secret) != std::string::npos || error != "console returned malformed JSON") {
        result.details = "JSON error exposed the response body";
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_a_rate_limit_surfaces_its_retry_after() {
    TestResult result;
    result.test_name = "a_rate_limit_surfaces_its_retry_after";

    // A 429 with a numeric Retry-After: the error a caller sees should name the
    // wait in seconds, not just "HTTP 429".
    wally::account::ConsoleClient with_hint([&](const wally::account::HttpRequest&,
                                               wally::account::HttpResponse* response, std::string*) {
        response->status = 429;
        response->headers["retry-after"] = "30";
        response->body = "{}";
        return true;
    });
    wally::account::Authorization authorization;
    std::string error;
    if (with_hint.BeginAuthorization("https://console.runanywhere.ai", "host", &authorization,
                                     &error) ||
        error.find("retry after 30s") == std::string::npos) {
        result.details = "a 429 with Retry-After did not surface the wait: " + error;
        return result;
    }

    // A 429 without the header still reads as a rate limit, just without a
    // number, and a garbage value is treated as absent rather than echoed.
    wally::account::ConsoleClient no_hint([&](const wally::account::HttpRequest&,
                                             wally::account::HttpResponse* response, std::string*) {
        response->status = 429;
        response->headers["retry-after"] = "soon";
        response->body = "{}";
        return true;
    });
    error.clear();
    if (no_hint.BeginAuthorization("https://console.runanywhere.ai", "host", &authorization,
                                   &error) ||
        error.find("rate limiting") == std::string::npos ||
        error.find("soon") != std::string::npos) {
        result.details = "a 429 with a non-numeric Retry-After was mishandled: " + error;
        return result;
    }

    // The parser itself: valid, absent, non-numeric, and the day ceiling.
    wally::account::HttpResponse r;
    r.headers["retry-after"] = "45";
    const bool ok = r.retry_after_seconds() == 45;
    r.headers.clear();
    const bool absent = r.retry_after_seconds() == -1;
    r.headers["retry-after"] = "-5";
    const bool negative = r.retry_after_seconds() == -1;
    r.headers["retry-after"] = "999999";
    const bool capped = r.retry_after_seconds() == 86400;
    if (!ok || !absent || !negative || !capped) {
        result.details = "retry_after_seconds parsed a value wrong";
        return result;
    }

    result.passed = true;
    return result;
}

TestResult test_console_rejects_header_injection() {
    TestResult result;
    result.test_name = "console_rejects_header_injection";
    wally::account::ConsoleClient client([](const wally::account::HttpRequest& request,
                                           wally::account::HttpResponse* response, std::string*) {
        response->status = 200;
        if (request.url.ends_with("/auth/cli/poll")) {
            response->body = Json{{"status", "approved"},
                                  {"access_token", "safe\r\nX-Injected: yes"},
                                  {"refresh_token", "refresh-token"}}
                                 .dump();
        } else if (request.url.ends_with("/auth/cli/start")) {
            response->body = Json{{"request_code", "ABCD\nEFGH"},
                                  {"poll_secret", "poll-secret"},
                                  {"verification_url", "https://console.runanywhere.ai/device"}}
                                 .dump();
        }
        return true;
    });

    std::string error;
    wally::account::Authorization authorization;
    if (client.BeginAuthorization("https://console.runanywhere.ai", "host", &authorization,
                                  &error) ||
        error != "console returned an invalid authorization request") {
        result.details = "terminal control characters were accepted in an authorization code";
        return result;
    }

    authorization.request_code = "ABCD-EFGH";
    authorization.poll_secret = "poll-secret";
    wally::account::Grant grant;
    error.clear();
    if (client.Poll("https://console.runanywhere.ai", authorization, &grant, &error) !=
            wally::account::PollResult::Failed ||
        error != "console returned an invalid cloud session") {
        result.details = "HTTP header control characters were accepted in an access token";
        return result;
    }
    result.passed = true;
    return result;
}

// The API host and the browser approval host are two deployments. Collapsing
// them, or pointing the API at the console's Railway host, is the regression
// this pins: measured 2026-09-04, console.runanywhere.ai answers
// /auth/cli/start and /v1/me with 404 and its own SPA HTML, while
// inference.runanywhere.ai answers 422 and 405 — the endpoints rejecting a bad
// body and a wrong verb, which is how you know they exist.
TestResult test_the_api_host_and_the_browser_host_stay_apart() {
    TestResult result;
    result.test_name = "the_api_host_and_the_browser_host_stay_apart";
    EnvVar console("WALLY_CONSOLE_URL", nullptr);
    EnvVar web("WALLY_CONSOLE_WEB_URL", nullptr);

    const std::string api = wally::account::DefaultConsoleUrl();
    const std::vector<std::string> browser = wally::account::TrustedBrowserOrigins(api);

    if (api == "https://console.runanywhere.ai") {
        result.details = "the API default is the web console, which serves no /auth/cli or /v1 route";
        result.actual = api;
        return result;
    }
    if (api != "https://inference.runanywhere.ai") {
        result.details = "the API default moved; confirm the new host serves /auth/cli/* and /v1/me";
        result.actual = api;
        return result;
    }
    for (const std::string& origin : browser) {
        if (origin == api) {
            result.details = "one host cannot be both the control plane and the approval page";
            return result;
        }
    }
    // The origin the control plane actually puts in verification_url today. Drop
    // this once that config names the custom domain; until then, removing it
    // refuses every production sign-in.
    if (!wally::account::BrowserUrlIsTrusted(
            "https://runanywhere-frontend-production.up.railway.app/cloud/cli?code=abc", browser)) {
        result.details = "production's own approval URL must pass the origin check";
        return result;
    }
    if (!wally::account::BrowserUrlIsTrusted("https://console.runanywhere.ai/cloud/cli?code=abc",
                                            browser)) {
        result.details = "the console's custom domain must pass the origin check";
        return result;
    }
    if (wally::account::BrowserUrlIsTrusted("https://console.runanywhere.ai.evil.test/cloud/cli",
                                           browser)) {
        result.details = "a lookalike host must not pass on a prefix match";
        return result;
    }
    result.passed = true;
    return result;
}

// The pinning only does something if it is on by default. Before this, the
// trusted origin came from WALLY_CONSOLE_WEB_URL alone — unset in every shipped
// install — so the check ran against an empty string and took whatever origin
// the server put in verification_url.
TestResult test_the_trusted_browser_origin_is_never_empty() {
    TestResult result;
    result.test_name = "the_trusted_browser_origin_is_never_empty";
    EnvVar web("WALLY_CONSOLE_WEB_URL", nullptr);

    // An unknown console is trusted at its own origin and nowhere else, so a
    // dev or loopback console keeps working without widening what we accept.
    const std::vector<std::string> loopback =
        wally::account::TrustedBrowserOrigins("http://127.0.0.1:8080");
    if (loopback.size() != 1 || loopback[0] != "http://127.0.0.1:8080") {
        result.details = "an unknown console must be trusted only at its own origin";
        return result;
    }
    if (wally::account::TrustedBrowserOrigins("https://dev.example.test").empty()) {
        result.details = "an empty trusted origin list pins nothing";
        return result;
    }
    if (wally::account::BrowserUrlIsTrusted("https://console.runanywhere.ai/cloud/cli", loopback)) {
        result.details = "the production console must not be trusted for a dev API";
        return result;
    }

    // A declared origin still wins, and replaces the pair rather than adding
    // to it, which is what points sign-in at a console served somewhere else.
    EnvVar declared("WALLY_CONSOLE_WEB_URL", "https://console.dev.example.test");
    const std::vector<std::string> overridden =
        wally::account::TrustedBrowserOrigins("https://inference.runanywhere.ai");
    if (overridden.size() != 1 || overridden[0] != "https://console.dev.example.test") {
        result.details = "a declared WALLY_CONSOLE_WEB_URL must outrank and replace the defaults";
        return result;
    }
    result.passed = true;
    return result;
}

// Usage counters are 64-bit all the way through. `long` is 32 bits on MSVC, so
// the values below round-tripped as garbage on Windows: cost_micros passes 2^31
// at about $2,147 of spend, which is a number a real customer reaches.
TestResult test_usage_counters_survive_beyond_thirty_two_bits() {
    TestResult result;
    result.test_name = "usage_counters_survive_beyond_thirty_two_bits";
    TempDirectory temporary;
    EnvVar profile("WALLY_PROFILE_DIR", temporary.path().string().c_str());

    constexpr std::int64_t kCost = 9'000'000'000;     // $9,000, well past 2^31
    constexpr std::int64_t kPrompt = 5'000'000'000;   // more than INT32_MAX
    constexpr std::int64_t kBalance = 4'000'000'000;

    wally::account::ConsoleClient console(
        [&](const wally::account::HttpRequest& request, wally::account::HttpResponse* response,
            std::string*) {
            if (request.url.find("/v1/cli/usage") == std::string::npos) {
                return false;
            }
            response->status = 200;
            response->body =
                Json{{"credit", {{"balance_micros", kBalance}}},
                     {"totals",
                      {{"prompt_tokens", kPrompt}, {"cost_micros", kCost}, {"cached_tokens", 0}}},
                     {"windows",
                      Json::array({Json{{"window", "1h"},
                                        {"seconds", 3600},
                                        {"totals", {{"prompt_tokens", kPrompt},
                                                    {"cost_micros", kCost}}}}})}}
                    .dump();
            return true;
        });

    wally::account::Usage usage;
    std::string error;
    const wally::account::IdentityResult status = console.FetchUsage(
        "https://console.example.test", "a-token", wally::account::UsageQuery{}, &usage, &error);
    if (status != wally::account::IdentityResult::Ok) {
        result.details = error.empty() ? "usage request failed" : error;
        return result;
    }
    if (usage.totals.cost_micros != kCost || usage.totals.prompt_tokens != kPrompt ||
        usage.credit.balance_micros != kBalance) {
        result.details = "a usage counter above 2^31 was truncated";
        result.expected = std::to_string(kCost);
        result.actual = std::to_string(usage.totals.cost_micros);
        return result;
    }
    if (usage.windows.size() != 1 || usage.windows[0].totals.cost_micros != kCost) {
        result.details = "a window counter above 2^31 was truncated";
        return result;
    }
    result.passed = true;
    return result;
}

// CancelRequest (wally #81): the typed call behind the shim's abandon path.
// Everything the control plane's contract fixes is asserted through a mock
// transport -- the path with the id escaped into it, the method, the bearer,
// the small timeout -- and each of the three outcomes maps from its status.
TestResult test_cancel_request_speaks_the_contract() {
    TestResult result;
    result.test_name = "cancel_request_speaks_the_contract";
    std::vector<wally::account::HttpRequest> requests;
    int answer = 202;
    bool reachable = true;
    wally::account::Transport transport = [&](const wally::account::HttpRequest& request,
                                             wally::account::HttpResponse* response,
                                             std::string* error) {
        requests.push_back(request);
        if (!reachable) {
            if (error != nullptr) {
                *error = "connection refused";
            }
            return false;
        }
        response->status = answer;
        response->body = answer == 202
                             ? Json{{"request_id", "abc-123"}, {"status", "cancelling"}}.dump()
                             : Json{{"code", "not_found"}, {"message", "no such request"}}.dump();
        return true;
    };
    wally::account::ConsoleClient client(transport);
    std::string error;

    auto outcome = client.CancelRequest("https://inference.runanywhere.ai/api-dev", "sess-token",
                                        "abc 123/../x", 3000, &error);
    if (outcome != wally::account::CancelOutcome::Cancelled) {
        result.details = "a 202 must be Cancelled: " + error;
        return result;
    }
    if (requests.size() != 1 || requests[0].method != "POST" ||
        requests[0].url !=
            "https://inference.runanywhere.ai/api-dev/v1/requests/abc%20123%2F..%2Fx/cancel" ||
        requests[0].bearer_token != "sess-token" || !requests[0].body.empty() ||
        requests[0].timeout_ms != 3000) {
        result.details = "request shape drifted: " + (requests.empty() ? "" : requests[0].url) +
                         " timeout_ms=" + std::to_string(requests.empty() ? -1 : requests[0].timeout_ms);
        return result;
    }

    answer = 404;
    if (client.CancelRequest("https://inference.runanywhere.ai", "sess-token", "abc-123", 3000,
                             &error) != wally::account::CancelOutcome::NotFound) {
        result.details = "a 404 must be NotFound";
        return result;
    }
    if (requests.back().url != "https://inference.runanywhere.ai/v1/requests/abc-123/cancel") {
        result.details = "production shape drifted: " + requests.back().url;
        return result;
    }

    answer = 500;
    if (client.CancelRequest("https://inference.runanywhere.ai", "sess-token", "abc-123", 3000,
                             &error) != wally::account::CancelOutcome::Failed ||
        error.empty()) {
        result.details = "a 500 must be Failed with a message";
        return result;
    }
    reachable = false;
    if (client.CancelRequest("https://inference.runanywhere.ai", "sess-token", "abc-123", 3000,
                             &error) != wally::account::CancelOutcome::Failed) {
        result.details = "an unreachable console must be Failed";
        return result;
    }
    // No id, no token: refused before any transport call.
    const std::size_t before = requests.size();
    if (client.CancelRequest("https://inference.runanywhere.ai", "sess-token", "", 3000, &error) !=
            wally::account::CancelOutcome::Failed ||
        client.CancelRequest("https://inference.runanywhere.ai", "", "abc-123", 3000, &error) !=
            wally::account::CancelOutcome::Failed ||
        requests.size() != before) {
        result.details = "an empty id or token must be refused without a call";
        return result;
    }
    result.passed = true;
    return result;
}

// The transport honours `timeout_ms`: a socket that accepts and never answers
// is given up within the request's own bound, not the 30 s default. Bites: with
// the field ignored this test takes ~30 s and fails its budget.
TestResult test_a_request_timeout_bounds_the_real_transport() {
    TestResult result;
    result.test_name = "a_request_timeout_bounds_the_real_transport";
    // The handler holds the request until the test lets go, so the test's own
    // wall time is the client's bound, not a fixed sleep; neutered, the client
    // waits its 30 s default before this test can fail it.
    std::promise<void> release;
    std::shared_future<void> released = release.get_future().share();
    httplib::Server server;
    server.Post("/v1/requests/:id/cancel",
                [released](const httplib::Request&, httplib::Response&) { released.wait(); });
    const int port = server.bind_to_any_port("127.0.0.1");
    std::thread thread([&] { server.listen_after_bind(); });
    server.wait_until_ready();

    wally::account::ConsoleClient client;  // the real transport
    std::string error;
    const auto started = std::chrono::steady_clock::now();
    const auto outcome = client.CancelRequest("http://127.0.0.1:" + std::to_string(port),
                                              "sess-token", "abc-123", 500, &error);
    const auto took = std::chrono::steady_clock::now() - started;
    release.set_value();
    server.stop();
    thread.join();
    if (outcome != wally::account::CancelOutcome::Failed) {
        result.details = "a call that never gets an answer must be Failed";
        return result;
    }
    if (took > std::chrono::seconds(5)) {
        result.details = "timeout_ms was not honoured: the call took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) +
                         " ms";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_account");
    suite.add("console_url_validation", test_console_url_validation);
    suite.add("credential_roundtrip_and_permissions", test_credential_roundtrip_and_permissions);
    suite.add("credentials_default_console_url_without_a_file",
              test_credentials_default_console_url_without_a_file);
#if !defined(_WIN32)
    suite.add("credentials_missing_console_url_falls_back",
              test_credentials_missing_console_url_falls_back);
    suite.add("credentials_warns_on_exposed_permissions",
              test_credentials_warns_on_exposed_permissions);
#else
    suite.add("credentials_reject_a_document_they_cannot_unlock",
              test_credentials_reject_a_document_they_cannot_unlock);
#endif
    suite.add("console_client_contract", test_console_client_contract);
    suite.add("console_errors_do_not_echo_secrets", test_console_errors_do_not_echo_secrets);
    suite.add("a_rate_limit_surfaces_its_retry_after", test_a_rate_limit_surfaces_its_retry_after);
    suite.add("console_rejects_header_injection", test_console_rejects_header_injection);
    suite.add("the_api_host_and_the_browser_host_stay_apart",
              test_the_api_host_and_the_browser_host_stay_apart);
    suite.add("the_trusted_browser_origin_is_never_empty",
              test_the_trusted_browser_origin_is_never_empty);
    suite.add("usage_counters_survive_beyond_thirty_two_bits",
              test_usage_counters_survive_beyond_thirty_two_bits);
    suite.add("cancel_request_speaks_the_contract", test_cancel_request_speaks_the_contract);
    suite.add("a_request_timeout_bounds_the_real_transport",
              test_a_request_timeout_bounds_the_real_transport);
    return suite.run(argc, argv);
}
