#include "test_common.h"

#include <chrono>
#include <cstdlib>
#include <filesystem>
#include <nlohmann/json.hpp>
#include <string>

#include "account/console.h"
#include "account/credentials.h"
#include "harness/harness.h"

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

    // Each of these would corrupt or extend the raw string-concatenated XML
    // that ide::jetbrains_profile's ModelsXML writes into a live IDE's
    // settings, or claims a directory separator no real local/upstream id
    // ever contains.
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
        if (request.url.ends_with("/v1/auth/cli/refresh")) {
            refreshed = true;
            response->status = 200;
            response->body = Json{{"access_token", "new-access-token"},
                                  {"refresh_token", "new-refresh-token"},
                                  {"expires_in", 7200}}
                                 .dump();
            return true;
        }
        if (request.url.ends_with("/v1/auth/me")) {
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
        if (request.url.ends_with("/v1/auth/cli/refresh")) {
            refresh_called = true;
            return false;
        }
        if (request.url.ends_with("/v1/auth/me") && request.bearer_token == "real-access-token") {
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
// is InferenceInfra#444: a load test drove the identity route to 429 and every
// signed-in person was refused entry to their own harness, `wally login`
// included.
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
        if (request.url.ends_with("/v1/auth/cli/refresh")) {
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

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_harness");
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
    return suite.run(argc, argv);
}
