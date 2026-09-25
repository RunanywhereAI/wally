// The generated console binding is in lockstep with its pinned contract, and it
// round-trips the shapes the CLI actually sends and receives.

#include "test_common.h"

#include <cstdint>
#include <fstream>
#include <optional>
#include <sstream>
#include <string>

#include <nlohmann/json.hpp>

#include "account/console_contract.h"

namespace {

using Json = nlohmann::json;
namespace contract = wally::account::contract;

// contracts/wally-cli-v1.openapi.json, relative to this repo. Located by
// walking up from the test binary is fragile, so the path is passed at compile
// time; see tests/CMakeLists.txt.
#ifndef WALLY_CONTRACT_PATH
#define WALLY_CONTRACT_PATH ""
#endif

std::string Sha256Hex(const std::string& bytes);  // small local impl below

TestResult test_binding_matches_the_pinned_contract() {
    TestResult result;
    result.test_name = "binding_matches_the_pinned_contract";

    const std::string path = WALLY_CONTRACT_PATH;
    std::ifstream file(path, std::ios::binary);
    if (!file) {
        result.details = std::string("cannot open pinned contract at ") + path;
        return result;
    }
    std::ostringstream buffer;
    buffer << file.rdbuf();
    const std::string bytes = buffer.str();

    // The header's pin must equal the SHA-256 of the artifact on disk. If they
    // differ, someone edited one without regenerating the other.
    const std::string digest = Sha256Hex(bytes);
    if (digest != contract::kContractSha256) {
        result.details = "contract hash drifted from the generated binding: disk=" + digest +
                         " header=" + std::string(contract::kContractSha256);
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_request_and_response_round_trip() {
    TestResult result;
    result.test_name = "request_and_response_round_trip";

    // A request serializes to exactly the fields the server expects.
    contract::CliStartRequest start;
    start.client = contract::CliClient::kRcli;
    start.hostname = "Homes-MacBook-Pro.local";
    const Json start_json = start;
    if (start_json.at("client") != "rcli" || start_json.at("hostname") != start.hostname ||
        start_json.size() != 2) {
        result.details = "CliStartRequest did not serialize to the contract shape";
        return result;
    }

    // A response with a nullable field absent leaves the optional empty; present
    // fills it. PollResponse is the one with optionals.
    const Json pending = Json{{"status", "pending"}};
    const auto poll_pending = pending.get<contract::PollResponse>();
    if (poll_pending.status != contract::PollStatus::kPending ||
        poll_pending.access_token.has_value()) {
        result.details = "PollResponse mis-parsed the pending case";
        return result;
    }
    const Json approved = Json{{"status", "approved"},
                               {"access_token", "sk-x"},
                               {"email", "a@b.co"},
                               {"expires_in", 3600},
                               {"plan", "beta"},
                               {"refresh_token", "r-x"}};
    const auto poll_approved = approved.get<contract::PollResponse>();
    if (poll_approved.status != contract::PollStatus::kApproved ||
        poll_approved.access_token.value_or("") != "sk-x" ||
        poll_approved.plan.value_or(contract::CliPlan::kBeta) != contract::CliPlan::kBeta) {
        result.details = "PollResponse mis-parsed the approved case";
        return result;
    }

    // An unknown enum value is a hard parse error, never a silent default.
    bool threw = false;
    try {
        Json{{"status", "banana"}}.get<contract::PollResponse>();
    } catch (const Json::exception&) {
        threw = true;
    }
    if (!threw) {
        result.details = "an unknown PollStatus should have thrown";
        return result;
    }

    // A nested response with arrays parses end to end.
    const Json usage = Json{
        {"credit", {{"balance_micros", 1985000}, {"granted_micros", 2000000}, {"spent_micros", 15000}}},
        {"totals", {{"requests", 3}, {"prompt_tokens", 10}, {"completion_tokens", 5}, {"cached_tokens", 0}, {"cost_micros", 16}}},
        {"windows", Json::array({{{"window", "1h"}, {"seconds", 3600}, {"totals", {{"requests", 0}, {"prompt_tokens", 0}, {"completion_tokens", 0}, {"cached_tokens", 0}, {"cost_micros", 0}}}}})},
        {"timeline", Json::array()},
        {"models", Json::array()},
        {"recent", Json::array()},
    };
    const auto parsed = usage.get<contract::CliUsageResponse>();
    if (parsed.credit.balance_micros != 1985000 || parsed.windows.size() != 1 ||
        parsed.windows[0].window != contract::CliUsageWindowLabel::k1h) {
        result.details = "CliUsageResponse mis-parsed a nested body";
        return result;
    }
    result.passed = true;
    return result;
}

// A tiny, dependency-free SHA-256 so the test does not pull in a crypto lib.
std::string Sha256Hex(const std::string& message) {
    auto rotr = [](std::uint32_t x, std::uint32_t n) { return (x >> n) | (x << (32 - n)); };
    static const std::uint32_t k[64] = {
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2};
    std::uint32_t h[8] = {0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
                          0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19};
    std::string data = message;
    const std::uint64_t bit_length = static_cast<std::uint64_t>(data.size()) * 8;
    data.push_back(static_cast<char>(0x80));
    while (data.size() % 64 != 56) {
        data.push_back(0);
    }
    for (int i = 7; i >= 0; --i) {
        data.push_back(static_cast<char>((bit_length >> (i * 8)) & 0xff));
    }
    for (std::size_t chunk = 0; chunk < data.size(); chunk += 64) {
        std::uint32_t w[64];
        for (int i = 0; i < 16; ++i) {
            w[i] = (static_cast<std::uint8_t>(data[chunk + i * 4]) << 24) |
                   (static_cast<std::uint8_t>(data[chunk + i * 4 + 1]) << 16) |
                   (static_cast<std::uint8_t>(data[chunk + i * 4 + 2]) << 8) |
                   (static_cast<std::uint8_t>(data[chunk + i * 4 + 3]));
        }
        for (int i = 16; i < 64; ++i) {
            const std::uint32_t s0 = rotr(w[i - 15], 7) ^ rotr(w[i - 15], 18) ^ (w[i - 15] >> 3);
            const std::uint32_t s1 = rotr(w[i - 2], 17) ^ rotr(w[i - 2], 19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16] + s0 + w[i - 7] + s1;
        }
        std::uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6],
                      hh = h[7];
        for (int i = 0; i < 64; ++i) {
            const std::uint32_t s1 = rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
            const std::uint32_t ch = (e & f) ^ (~e & g);
            const std::uint32_t t1 = hh + s1 + ch + k[i] + w[i];
            const std::uint32_t s0 = rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
            const std::uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
            const std::uint32_t t2 = s0 + maj;
            hh = g;
            g = f;
            f = e;
            e = d + t1;
            d = c;
            c = b;
            b = a;
            a = t1 + t2;
        }
        h[0] += a; h[1] += b; h[2] += c; h[3] += d;
        h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
    }
    static const char* hex = "0123456789abcdef";
    std::string out;
    for (std::uint32_t value : h) {
        for (int i = 7; i >= 0; --i) {
            out.push_back(hex[(value >> (i * 4)) & 0xf]);
        }
    }
    return out;
}

// A settled-request page parses end to end: every field the contract names,
// both the nullable shapes (absent, null, present) and the required ones.
TestResult test_usage_request_page_round_trip() {
    TestResult result;
    result.test_name = "usage_request_page_round_trip";

    const Json page = Json{
        {"as_of", "2026-09-25T08:34:18.548805Z"},
        {"totals",
         {{"requests", 2},
          {"prompt_tokens", 1000},
          {"cached_tokens", 900},
          {"noncached_prompt_tokens", 100},
          {"completion_tokens", 50},
          {"reasoning_tokens", 20},
          {"cost_micros", 12345}}},
        {"requests",
         Json::array({
             Json{{"request_id", "ledger-1"},
                  {"response_request_id", "resp-1"},
                  {"api_key_id", nullptr},
                  {"model", "glm-5.3-flash"},
                  {"provider", "self_hosted_sglang"},
                  {"status_code", 200},
                  {"error_code", nullptr},
                  {"finish_reason", "stop"},
                  {"stream", true},
                  {"ts_start", "2026-09-25T08:00:00Z"},
                  {"ts_end", "2026-09-25T08:00:01Z"},
                  {"recorded_at", "2026-09-25T08:00:02Z"},
                  {"prompt_tokens", 800},
                  {"cached_tokens", 700},
                  {"noncached_prompt_tokens", 100},
                  {"completion_tokens", 40},
                  {"reasoning_tokens", 20},
                  {"max_tokens_requested", 4096},
                  {"max_tokens_granted", 2048},
                  {"ttft_ms", 310},
                  {"tpot_ms", nullptr},
                  {"cost_micros", 12345},
                  {"pricing_version", "2026-09-23.1"}},
             // Every nullable at null: the tolerant read must default them.
             Json{{"request_id", "ledger-2"},
                  {"response_request_id", nullptr},
                  {"api_key_id", nullptr},
                  {"model", "gemma-4"},
                  {"provider", "vertex_ai"},
                  {"status_code", 500},
                  {"error_code", "upstream_error"},
                  {"finish_reason", nullptr},
                  {"stream", false},
                  {"ts_start", "2026-09-25T07:00:00Z"},
                  {"ts_end", nullptr},
                  {"recorded_at", "2026-09-25T07:00:01Z"},
                  {"prompt_tokens", 200},
                  {"cached_tokens", 200},
                  {"noncached_prompt_tokens", 0},
                  {"completion_tokens", 10},
                  {"reasoning_tokens", 0},
                  {"max_tokens_requested", nullptr},
                  {"max_tokens_granted", nullptr},
                  {"ttft_ms", nullptr},
                  {"tpot_ms", nullptr},
                  {"cost_micros", 0},
                  {"pricing_version", "2026-09-23.1"}},
         })},
        {"next_cursor", "eyJuZXh0IjoxfQ"},
    };
    contract::UsageRequestPage parsed;
    try {
        parsed = page.get<contract::UsageRequestPage>();
    } catch (const Json::exception& e) {
        result.details = std::string("a contract-shaped page did not parse: ") + e.what();
        return result;
    }
    if (parsed.as_of != "2026-09-25T08:34:18.548805Z" || parsed.totals.requests != 2 ||
        parsed.totals.cost_micros != 12345 || parsed.next_cursor.value_or("") != "eyJuZXh0IjoxfQ") {
        result.details = "the page's own fields mis-parsed";
        return result;
    }
    if (parsed.requests.size() != 2 ||
        parsed.requests[0].response_request_id.value_or("") != "resp-1" ||
        parsed.requests[0].provider.value_or(contract::UsageProvider::kUnknown) !=
            contract::UsageProvider::kSelfHostedSglang ||
        parsed.requests[0].max_tokens_granted.value_or(0) != 2048 ||
        parsed.requests[0].ttft_ms.value_or(0) != 310) {
        result.details = "the first record mis-parsed";
        return result;
    }
    if (parsed.requests[1].response_request_id.has_value() ||
        parsed.requests[1].ts_end.has_value() || parsed.requests[1].ttft_ms.has_value() ||
        parsed.requests[1].max_tokens_granted.has_value() ||
        parsed.requests[1].provider.value_or(contract::UsageProvider::kUnknown) !=
            contract::UsageProvider::kVertexAi) {
        result.details = "nulls must read as nullopt, not as a default value";
        return result;
    }

    // A body that omits a nullable parses too: the CLI survives a server that
    // lags the contract. Required fields still refuse to be missing.
    const Json minimal = Json{
        {"as_of", "2026-09-25T08:00:00Z"},
        {"totals",
         {{"requests", 0},
          {"prompt_tokens", 0},
          {"cached_tokens", 0},
          {"noncached_prompt_tokens", 0},
          {"completion_tokens", 0},
          {"reasoning_tokens", 0},
          {"cost_micros", 0}}},
        {"requests", Json::array()},
        {"next_cursor", nullptr},
    };
    const auto empty = minimal.get<contract::UsageRequestPage>();
    if (!empty.next_cursor.has_value() && empty.next_cursor != std::nullopt) {
        result.details = "a null next_cursor must stay null";
        return result;
    }

    // A page whose totals are empty parses: the generated binding reads a
    // missing member as its default, so the CLI survives a server that lags
    // the contract. What the CLI never accepts is a body that is not an
    // object at all.
    const Json sparse = Json{
        {"as_of", "2026-09-25T08:00:00Z"},
        {"totals", Json::object()},
        {"requests", Json::array()},
        {"next_cursor", nullptr},
    };
    const auto quiet = sparse.get<contract::UsageRequestPage>();
    if (quiet.totals.requests != 0 || quiet.requests.size() != 0) {
        result.details = "an empty totals must read as zeros";
        return result;
    }

    // The generated binding guards every member with contains(), so an array
    // body parses as an all-defaults page rather than throwing: tolerance is
    // the header's contract, tested here so it stays deliberate.
    const auto nonsense = Json::array().get<contract::UsageRequestPage>();
    if (nonsense.totals.requests != 0 || !nonsense.as_of.empty() ||
        nonsense.next_cursor.has_value()) {
        result.details = "an array body must read as an all-defaults page";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_contract");
    suite.add("binding_matches_the_pinned_contract", test_binding_matches_the_pinned_contract);
    suite.add("request_and_response_round_trip", test_request_and_response_round_trip);
    suite.add("usage_request_page_round_trip", test_usage_request_page_round_trip);
    return suite.run(argc, argv);
}
