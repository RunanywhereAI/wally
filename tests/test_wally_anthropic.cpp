#include "fake_upstream.h"
#include "test_common.h"

#include <httplib.h>

#include <atomic>
#include <nlohmann/json.hpp>
#include <string>
#include <thread>
#include <vector>

#include "anthropic/messages.h"
#include "harness/harness.h"
#include "net/upstream_pool.h"

// The Anthropic translator's upstream connection behaviour, against the fake
// upstreams in fake_upstream.h.

namespace {

using Json = nlohmann::json;
using wally_tests::Describe;
using wally_tests::FakeUpstream;

TestResult test_overload_headers_survive_streaming() {
    TestResult result;
    result.test_name = "overload_headers_survive_streaming";
    httplib::Server server;
    std::atomic<int> calls{0};
    server.Post("/v1/chat/completions", [&](const httplib::Request& request,
                                            httplib::Response& response) {
        ++calls;
        response.status = Json::parse(request.body).value("max_tokens", 429);
        response.set_header("Retry-After", response.status == 429 ? "7" : "Wed, 21 Oct 2037 07:28:00 GMT");
        if (request.get_header_value("Authorization") != "Bearer test-upstream-key") {
            response.status = 401;
        }
        response.set_content(R"({"error":{"message":"capacity exhausted"}})", "application/json");
    });
    const int port = server.bind_to_any_port("127.0.0.1");
    std::thread thread([&] { server.listen_after_bind(); });
    server.wait_until_ready();
    wally::harness::Endpoint endpoint;
    endpoint.base_url = "http://127.0.0.1:" + std::to_string(port) + "/v1";
    endpoint.api_key = "test-upstream-key";
    wally::anthropic::Shim shim;
    const bool started = wally::anthropic::Start(endpoint, "test-model", &shim);
    bool okay = started;
    if (started) {
        httplib::Client client(shim.base_url);
        for (bool streaming : {false, true}) {
            for (int status : {429, 503}) {
                Json body{{"model", "test"},
                          {"stream", streaming},
                          {"max_tokens", status},
                          {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
                auto reply = client.Post("/v1/messages", {{"x-api-key", shim.auth_token}},
                                         body.dump(), "application/json");
                okay = okay && reply && reply->status == status &&
                       reply->get_header_value("Retry-After") ==
                           (status == 429 ? "7" : "Wed, 21 Oct 2037 07:28:00 GMT") &&
                       reply->get_header_value("Content-Type").find("application/json") == 0 &&
                       reply->body.find("capacity exhausted") != std::string::npos;
                result.actual += std::to_string(streaming) + ":" +
                                 std::to_string(reply ? reply->status : 0) + ":" +
                                 (reply ? reply->get_header_value("Retry-After") : "") + " ";
            }
        }
    }
    wally::anthropic::Stop(&shim);
    server.stop();
    thread.join();
    result.passed = okay && calls == 4;
    result.expected = "stream/nonstream preserve 429/503 and numeric/date Retry-After; authenticated once each";
    return result;
}
#if !defined(_WIN32)
using wally_tests::HalfOpenUpstream;
#endif

/// A translator started against `upstream`, stopped on scope exit.
class RunningShim {
   public:
    explicit RunningShim(const std::string& upstream_base_url) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream_base_url;
        endpoint.api_key = "test-upstream-key";
        started_ = wally::anthropic::Start(endpoint, "glm-5.3", &shim_);
    }
    ~RunningShim() { wally::anthropic::Stop(&shim_); }

    bool started() const { return started_; }
    const wally::anthropic::Shim& shim() const { return shim_; }

    /// One Anthropic-shaped request through the translator; the status it
    /// answered with, or 0 when nothing came back.
    int Send(bool streaming) const {
        httplib::Client client(shim_.base_url);
        client.set_read_timeout(10, 0);
        const Json body{{"model", "claude-x"},
                        {"max_tokens", 16},
                        {"stream", streaming},
                        {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
        const httplib::Result reply =
            client.Post("/v1/messages", {{"Authorization", "Bearer " + shim_.auth_token}},
                        body.dump(), "application/json");
        return reply ? reply->status : 0;
    }

   private:
    wally::anthropic::Shim shim_;
    bool started_ = false;
};

// Test A. Two requests, one after the other, must arrive at the upstream on
// the same connection. Building a client per request (the behaviour #80
// fixes) opens a new connection each time, so the ports differ.
TestResult test_sequential_requests_reuse_the_upstream_connection() {
    TestResult result;
    result.test_name = "sequential_requests_reuse_the_upstream_connection";
    FakeUpstream upstream;
    RunningShim shim(upstream.base_url());
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    const int first = shim.Send(true);
    const int second = shim.Send(false);
    const std::vector<int> ports = upstream.ports();
    if (first != 200 || second != 200 || ports.size() != 2) {
        result.expected = "two 200s and two upstream requests";
        result.actual =
            std::to_string(first) + ", " + std::to_string(second) + ", " + Describe(ports);
        return result;
    }
    result.passed = ports[0] == ports[1];
    result.expected = "same peer port on both upstream requests (one connection)";
    result.actual = Describe(ports);
    return result;
}

// Test B. Two requests in flight at once must NOT share a connection: an
// httplib client serialises requests on its socket, so a single shared client
// would queue the second stream behind the first. The upstream holds both
// streams until both have arrived, so a finished stream cannot lend its
// connection and make the test pass by legitimate reuse.
TestResult test_concurrent_requests_use_separate_connections() {
    TestResult result;
    result.test_name = "concurrent_requests_use_separate_connections";
    FakeUpstream upstream;
    upstream.hold_streams_until(2);
    RunningShim shim(upstream.base_url());
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    std::atomic<int> first{0};
    std::atomic<int> second{0};
    std::thread a([&] { first = shim.Send(true); });
    std::thread b([&] { second = shim.Send(true); });
    a.join();
    b.join();
    const std::vector<int> ports = upstream.ports();
    if (first != 200 || second != 200 || ports.size() != 2) {
        result.expected = "two 200s and two upstream requests";
        result.actual = std::to_string(first.load()) + ", " + std::to_string(second.load()) + ", " +
                        Describe(ports);
        return result;
    }
    result.passed = ports[0] != ports[1];
    result.expected = "different peer ports (two connections in flight)";
    result.actual = Describe(ports);
    return result;
}

// The pool on its own, no translator in front of it.
TestResult test_pool_returns_a_clean_lease_and_drops_a_discarded_one() {
    TestResult result;
    result.test_name = "pool_returns_a_clean_lease_and_drops_a_discarded_one";
    wally::net::UpstreamOptions options;
    options.origin = "http://127.0.0.1:9";
    options.idle_limit = 2;
    auto pool = std::make_shared<wally::net::UpstreamPool>(options);
    {
        wally::net::UpstreamLease first = pool->acquire("k");
        if (first.reused() || pool->idle() != 0) {
            result.expected = "a fresh lease from an empty pool";
            result.actual = "reused=" + std::to_string(first.reused()) +
                            " idle=" + std::to_string(pool->idle());
            return result;
        }
    }
    if (pool->idle() != 1) {
        result.expected = "1 idle client after a clean lease ends";
        result.actual = std::to_string(pool->idle());
        return result;
    }
    {
        wally::net::UpstreamLease second = pool->acquire("k");
        if (!second.reused()) {
            result.expected = "the idle client reused";
            result.actual = "a fresh client";
            return result;
        }
        second.discard();
    }
    if (pool->idle() != 0) {
        result.expected = "0 idle after a discarded lease ends";
        result.actual = std::to_string(pool->idle());
        return result;
    }
    // Three clean leases at once, then returned: the idle set stops at the limit.
    {
        wally::net::UpstreamLease a = pool->acquire("k");
        wally::net::UpstreamLease b = pool->acquire("k");
        wally::net::UpstreamLease c = pool->acquire("k");
    }
    result.passed = pool->idle() == 2;
    result.expected = "idle capped at 2";
    result.actual = std::to_string(pool->idle());
    return result;
}

// A pool outlives its leases: dropping the last shared_ptr while a lease is
// out must not leave the lease pointing at freed memory.
TestResult test_pool_outlives_an_outstanding_lease() {
    TestResult result;
    result.test_name = "pool_outlives_an_outstanding_lease";
    wally::net::UpstreamOptions options;
    options.origin = "http://127.0.0.1:9";
    auto pool = std::make_shared<wally::net::UpstreamPool>(options);
    std::weak_ptr<wally::net::UpstreamPool> weak = pool;
    {
        wally::net::UpstreamLease lease = pool->acquire("k");
        pool.reset();
        if (weak.expired()) {
            result.expected = "pool alive while a lease is out";
            result.actual = "expired";
            return result;
        }
    }
    result.passed = weak.expired();
    result.expected = "pool freed once the last lease returned";
    result.actual = weak.expired() ? "freed" : "still alive";
    return result;
}

TestResult test_retry_rule_only_on_a_stale_reused_connection() {
    TestResult result;
    result.test_name = "retry_rule_only_on_a_stale_reused_connection";
    using wally::net::RetryOnFreshConnection;
    using E = httplib::Error;
    struct Case {
        E error;
        bool has_response;
        bool received_any;
        bool reused;
        bool expect;
        const char* why;
    };
    const Case cases[] = {
        {E::Read, false, false, true, true, "stale reused socket, nothing back"},
        {E::Connection, false, false, true, true, "reused, connect-class error"},
        {E::ConnectionClosed, false, false, true, true, "reused, closed by peer"},
        {E::Write, false, false, true, true, "reused, write failed"},
        {E::SSLConnection, false, false, true, true, "reused, TLS layer reset"},
        {E::Read, false, false, false, false, "fresh connection: a real outage surfaces"},
        {E::Read, true, false, true, false, "a status arrived: never repeat"},
        {E::Read, false, true, true, false, "bytes reached the caller: never repeat"},
        {E::Timeout, false, false, true, false, "read timeout is not a stale socket"},
        {E::ConnectionTimeout, false, false, true, false, "connect timeout is the network"},
        {E::SSLServerVerification, false, false, true, false,
         "certificate failure is not transient"},
        {E::Canceled, false, false, true, false, "a reader that left is not a stale socket"},
    };
    for (const Case& c : cases) {
        const bool got = RetryOnFreshConnection(c.error, c.has_response, c.received_any, c.reused);
        if (got != c.expect) {
            result.expected = std::string(c.why) + " -> " + (c.expect ? "retry" : "no retry");
            result.actual = got ? "retry" : "no retry";
            return result;
        }
    }
    result.passed = true;
    return result;
}

// Step 4a. A reused connection the far side has quietly stopped serving:
// the request goes out, nothing comes back, the connection ends. The
// translator must try once more on a fresh connection and answer 200, and the
// upstream must see exactly three requests: the first (answered), the stale
// one (dropped), and the retry (answered) on a NEW connection.
TestResult test_stale_reused_connection_is_retried_once_on_a_fresh_one() {
    TestResult result;
    result.test_name = "stale_reused_connection_is_retried_once_on_a_fresh_one";
#if defined(_WIN32)
    result.passed = true;
    result.details = "skipped: raw-socket fake upstream is POSIX-only";
    return result;
#else
    HalfOpenUpstream upstream;
    if (!upstream.ok()) {
        result.details = "could not bind the half-open upstream";
        return result;
    }
    RunningShim shim(upstream.base_url());
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    const int first = shim.Send(false);
    const int second = shim.Send(false);
    const std::vector<int> ports = upstream.ports();
    result.expected =
        "200, 200; three upstream requests, the first two on one connection, the third on another";
    result.actual = std::to_string(first) + ", " + std::to_string(second) + "; " + Describe(ports);
    result.passed = first == 200 && second == 200 && ports.size() == 3 && ports[0] == ports[1] &&
                    ports[2] != ports[0];
    return result;
#endif
}

// Step 4b. The case received_any exists for: a REUSED connection whose far
// side dies after it has started answering. The error is connection-class
// and there is no status, so only the "bytes reached the caller" rule stops
// a retry -- and a retry would run the generation twice. The upstream must
// see exactly two requests: the one that warmed the connection and the one
// that died on it.
//
// (An "editor abandons the stream" variant was tried and removed: on loopback
// the translator's writes to the closed reader keep succeeding for longer than
// the stream lasts, so that test could not observe the abort path and passed
// for the wrong reason. Abandonment is Error::Canceled, which the retry table
// test covers.)
TestResult test_upstream_dying_mid_stream_is_not_retried() {
    TestResult result;
    result.test_name = "upstream_dying_mid_stream_is_not_retried";
    FakeUpstream upstream;
    RunningShim shim(upstream.base_url());
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    const int warm = shim.Send(false);
    upstream.die_mid_stream(true);
    const int dying = shim.Send(true);
    const std::vector<int> ports = upstream.ports();
    result.expected =
        "200, 200 (the error rides inside the stream); exactly two upstream "
        "requests on one connection";
    result.actual = std::to_string(warm) + ", " + std::to_string(dying) + "; " + Describe(ports);
    result.passed = warm == 200 && dying == 200 && ports.size() == 2 && ports[0] == ports[1];
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_anthropic");
    suite.add("overload_headers_survive_streaming", test_overload_headers_survive_streaming);
    suite.add("sequential_requests_reuse_the_upstream_connection",
              test_sequential_requests_reuse_the_upstream_connection);
    suite.add("concurrent_requests_use_separate_connections",
              test_concurrent_requests_use_separate_connections);
    suite.add("pool_returns_a_clean_lease_and_drops_a_discarded_one",
              test_pool_returns_a_clean_lease_and_drops_a_discarded_one);
    suite.add("pool_outlives_an_outstanding_lease", test_pool_outlives_an_outstanding_lease);
    suite.add("retry_rule_only_on_a_stale_reused_connection",
              test_retry_rule_only_on_a_stale_reused_connection);
    suite.add("stale_reused_connection_is_retried_once_on_a_fresh_one",
              test_stale_reused_connection_is_retried_once_on_a_fresh_one);
    suite.add("upstream_dying_mid_stream_is_not_retried",
              test_upstream_dying_mid_stream_is_not_retried);
    return suite.run(argc, argv);
}
