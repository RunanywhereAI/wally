#include "test_common.h"

#include <atomic>
#include <mutex>
#include <chrono>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "anthropic/messages.h"
#include "fake_upstream.h"
#include "harness/harness.h"
#include "net/upstream_pool.h"

// The Anthropic translator's upstream connection behaviour, against the fake
// upstreams in fake_upstream.h.

namespace {

using Json = nlohmann::json;
using wally_tests::Describe;
using wally_tests::FakeUpstream;
#if !defined(_WIN32)
using wally_tests::HalfOpenUpstream;
#endif

/// A translator started against `upstream`, stopped on scope exit.
class RunningShim {
   public:
    /// `console_url` is where the shim cancels an abandoned request (#81):
    /// the fake upstream serves the cancel route on its own origin, so tests
    /// pass its base without the `/v1`. Empty means a local server -- no
    /// cancel is ever sent.
    explicit RunningShim(const std::string& upstream_base_url, std::string console_url = {}) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream_base_url;
        endpoint.api_key = "test-upstream-key";
        endpoint.console_url = std::move(console_url);
        started_ = wally::anthropic::Start(endpoint, "glm-5.3", &shim_);
    }
    /// Stops the translator now (what the wrapper does when the editor exits)
    /// and returns how long that took.
    std::chrono::milliseconds StopNow() {
        const auto started = std::chrono::steady_clock::now();
        wally::anthropic::Stop(&shim_);
        return std::chrono::duration_cast<std::chrono::milliseconds>(
            std::chrono::steady_clock::now() - started);
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

/// The origin of a fake upstream's base URL: "http://127.0.0.1:port/v1" ->
/// "http://127.0.0.1:port". What the shim treats as the console for cancels.
std::string OriginOf(const std::string& base_url) {
    return base_url.substr(0, base_url.rfind("/v1"));
}

/// An editor that opens a streaming request to the shim on its own thread and
/// can leave in the middle of it -- `Leave()` closes its socket, which is
/// what Claude Code's abort does (undici destroys the socket: a FIN).
class Editor {
   public:
    explicit Editor(const wally::anthropic::Shim& shim)
        : client_(std::make_unique<httplib::Client>(shim.base_url)), token_(shim.auth_token) {
        client_->set_read_timeout(10, 0);
    }
    void StartStreaming() {
        thread_ = std::thread([this] {
            const Json body{{"model", "claude-x"},
                            {"max_tokens", 16},
                            {"stream", true},
                            {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
            const httplib::Result reply = client_->Post(
                "/v1/messages", {{"Authorization", "Bearer " + token_}}, body.dump(),
                "application/json", [this](const char* data, size_t length) {
                    std::lock_guard<std::mutex> lock(mutex_);
                    received_.append(data, length);
                    return true;
                });
            status_ = reply ? reply->status : 0;
        });
    }
    /// Leaves the way an editor does: the socket is CLOSED, not just shut
    /// down (Claude Code aborts the fetch; a quitting app closes everything),
    /// so the shim's next write to it fails. `stop()` alone keeps the fd open
    /// until the client is destroyed, and writes into it keep succeeding.
    void Leave() {
        client_->stop();
        Join();
        client_.reset();
    }
    void Join() {
        if (thread_.joinable()) {
            thread_.join();
        }
    }
    std::string received() {
        std::lock_guard<std::mutex> lock(mutex_);
        return received_;
    }
    int status() const { return status_.load(); }

   private:
    std::unique_ptr<httplib::Client> client_;
    std::string token_;
    std::thread thread_;
    std::mutex mutex_;
    std::string received_;
    std::atomic<int> status_{0};
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
        result.actual = std::to_string(first) + ", " + std::to_string(second) + ", " +
                        Describe(ports);
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
        result.actual = std::to_string(first.load()) + ", " + std::to_string(second.load()) +
                        ", " + Describe(ports);
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
            result.actual = "reused=" + std::to_string(first.reused()) + " idle=" +
                            std::to_string(pool->idle());
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
        {E::SSLServerVerification, false, false, true, false, "certificate failure is not transient"},
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
    result.passed = first == 200 && second == 200 && ports.size() == 3 &&
                    ports[0] == ports[1] && ports[2] != ports[0];
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
    result.expected = "200, 200 (the error rides inside the stream); exactly two upstream "
                      "requests on one connection";
    result.actual = std::to_string(warm) + ", " + std::to_string(dying) + "; " + Describe(ports);
    result.passed = warm == 200 && dying == 200 && ports.size() == 2 && ports[0] == ports[1];
    return result;
}

// #81. The editor leaves while the upstream is still producing the body (the
// engine is decoding; the id is already in hand). Within a second the fake's
// cancel route sees that id with this session's bearer, the upstream socket
// is dropped, and -- the pool having been warmed so the lease is REUSED --
// the stale-retry rule does not re-send the prompt: arrivals stay at two.
TestResult test_an_abandoned_stream_is_cancelled_by_name_and_never_resent() {
    TestResult result;
    result.test_name = "an_abandoned_stream_is_cancelled_by_name_and_never_resent";
    FakeUpstream upstream;
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    // Warm the pool: the second request goes out on a reused connection.
    if (shim.Send(false) != 200) {
        result.details = "warm-up request failed";
        return result;
    }
    upstream.hold_streams_until(99);  // the body never comes on its own
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();

    if (!upstream.wait_for_cancels(1, std::chrono::seconds(1))) {
        result.details = "no cancel reached the endpoint within 1 s of the editor leaving";
        return result;
    }
    const auto cancels = upstream.cancels();
    if (cancels[0].request_id != FakeUpstream::request_id_of(2) ||
        cancels[0].authorization != "Bearer test-upstream-key") {
        result.details = "the cancel must name the abandoned request with the session's bearer: id=" +
                         cancels[0].request_id + " auth=" + cancels[0].authorization;
        return result;
    }
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    if (upstream.arrivals() != 2) {
        result.expected = "two upstream requests (warm-up + the abandoned one); no re-send";
        result.actual = std::to_string(upstream.arrivals()) + " arrivals";
        return result;
    }
    result.passed = true;
    return result;
}

// #81. Leaving while tokens are FLOWING -- Esc mid-answer, the common case.
// Here the leave is noticed by the failed write to the editor, not by the
// poll (the fake drips a frame every 5 ms; the 100 ms poll rarely gets there
// first), and that path must name the cancel just the same, and drop the
// upstream socket so the drip stops.
TestResult test_leaving_while_tokens_flow_cancels_by_name() {
    TestResult result;
    result.test_name = "leaving_while_tokens_flow_cancels_by_name";
    FakeUpstream upstream;
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    if (shim.Send(false) != 200) {  // warm the pool: the stream's lease is reused
        result.details = "warm-up request failed";
        return result;
    }
    upstream.drip(1000, 2);  // ~2 s of tokens
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();
    if (!upstream.wait_for_cancels(1, std::chrono::seconds(1))) {
        result.details = "no cancel reached the endpoint within 1 s of the editor leaving";
        return result;
    }
    const auto cancels = upstream.cancels();
    if (cancels.size() != 1 || cancels[0].request_id != FakeUpstream::request_id_of(2) ||
        cancels[0].authorization != "Bearer test-upstream-key") {
        result.details = "one cancel naming the abandoned request: n=" + std::to_string(cancels.size()) +
                         (cancels.empty() ? std::string() : " id=" + cancels[0].request_id);
        return result;
    }
    // The upstream socket is dropped: the fake's drip stops growing.
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    const int dripped = upstream.dripped();
    std::this_thread::sleep_for(std::chrono::milliseconds(200));
    if (upstream.dripped() != dripped || dripped >= 1000) {
        result.details = "the upstream socket must be dropped once the editor left: dripped " +
                         std::to_string(dripped) + " then " + std::to_string(upstream.dripped());
        return result;
    }
    if (upstream.arrivals() != 2 || upstream.cancels().size() != 1) {
        result.details = "no re-send and no second cancel: arrivals=" +
                         std::to_string(upstream.arrivals()) + " cancels=" +
                         std::to_string(upstream.cancels().size());
        return result;
    }
    if (editor.received().find("content_block_delta") == std::string::npos) {
        result.details = "the frames before the leave must have reached the editor";
        return result;
    }
    result.passed = true;
    return result;
}

// #81. A stream that completed is never cancelled, whenever the editor goes.
TestResult test_a_completed_stream_is_not_cancelled() {
    TestResult result;
    result.test_name = "a_completed_stream_is_not_cancelled";
    FakeUpstream upstream;
    upstream.die_mid_stream(false);
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    // A stream the fake finishes on its own is over in a millisecond, so the
    // editor leaves after it -- that must NOT cancel (nothing is running).
    Editor editor(shim.shim());
    editor.StartStreaming();
    editor.Join();
    editor.Leave();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    if (!upstream.cancels().empty()) {
        result.details = "a completed stream must never be cancelled";
        return result;
    }
    if (editor.status() != 200 || editor.received().find("message_stop") == std::string::npos) {
        result.details = "the completed stream must have reached the editor whole";
        return result;
    }
    result.passed = true;
    return result;
}

// #81. A local endpoint (no console, no key) has nothing to cancel: the
// abandon is logged and no cancel is attempted anywhere.
TestResult test_a_local_endpoint_is_never_cancelled() {
    TestResult result;
    result.test_name = "a_local_endpoint_is_never_cancelled";
    FakeUpstream upstream;
    upstream.hold_streams_until(99);
    RunningShim shim(upstream.base_url());  // no console_url
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();
    std::this_thread::sleep_for(std::chrono::milliseconds(400));
    if (!upstream.cancels().empty()) {
        result.details = "a local server must not be asked to cancel";
        return result;
    }
    result.passed = true;
    return result;
}

// #81. The wrapper exits right after the editor abandoned a stream (app quit):
// Stop() must let the cancel go out before returning -- the fake sits on its
// answer for 500 ms, so an un-joined Stop() would return without it -- and
// must still return within the bound (3 s per queued cancel), not after
// waiting for the engine's first token.
TestResult test_stop_sends_the_last_cancel_before_returning() {
    TestResult result;
    result.test_name = "stop_sends_the_last_cancel_before_returning";
    FakeUpstream upstream;
    upstream.hold_streams_until(99);
    upstream.delay_cancel_reply(500);
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();
    const auto took = shim.StopNow();
    if (upstream.cancels().size() != 1) {
        result.details = "Stop() returned without sending the abandoned request's cancel";
        return result;
    }
    if (took > std::chrono::milliseconds(3500)) {
        result.details = "Stop() took " + std::to_string(took.count()) + " ms";
        return result;
    }
    result.passed = true;
    return result;
}

// #81. The editor leaves during PREFILL -- the upstream has not sent its
// headers, so no id exists yet. The shim keeps the upstream open, and when
// the headers arrive it cancels by the id they carry; if the wrapper exits
// first, it gives up within a poll instead of waiting for the first token.
TestResult test_leaving_during_prefill_cancels_at_the_first_token() {
    TestResult result;
    result.test_name = "leaving_during_prefill_cancels_at_the_first_token";
    FakeUpstream upstream;
    upstream.hold_headers(true);
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    if (!upstream.cancels().empty()) {
        result.details = "nothing can be cancelled before the id exists";
        return result;
    }
    upstream.release_headers();  // the first token
    if (!upstream.wait_for_cancels(1, std::chrono::seconds(1))) {
        result.details = "the cancel must follow the headers within 1 s";
        return result;
    }
    if (upstream.cancels()[0].request_id != FakeUpstream::request_id_of(1)) {
        result.details = "wrong id: " + upstream.cancels()[0].request_id;
        return result;
    }
    result.passed = true;
    return result;
}

TestResult test_stopping_during_prefill_does_not_wait_for_the_first_token() {
    TestResult result;
    result.test_name = "stopping_during_prefill_does_not_wait_for_the_first_token";
    FakeUpstream upstream;
    upstream.hold_headers(true);
    RunningShim shim(upstream.base_url(), OriginOf(upstream.base_url()));
    if (!shim.started()) {
        result.details = "translator did not start";
        return result;
    }
    Editor editor(shim.shim());
    editor.StartStreaming();
    std::this_thread::sleep_for(std::chrono::milliseconds(300));
    editor.Leave();
    editor.Join();
    const auto took = shim.StopNow();
    upstream.release_headers();
    if (took > std::chrono::milliseconds(1500)) {
        result.details = "Stop() waited for the first token: " + std::to_string(took.count()) + " ms";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_anthropic");
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
    suite.add("an_abandoned_stream_is_cancelled_by_name_and_never_resent",
              test_an_abandoned_stream_is_cancelled_by_name_and_never_resent);
    suite.add("leaving_while_tokens_flow_cancels_by_name", test_leaving_while_tokens_flow_cancels_by_name);
    suite.add("a_completed_stream_is_not_cancelled", test_a_completed_stream_is_not_cancelled);
    suite.add("a_local_endpoint_is_never_cancelled", test_a_local_endpoint_is_never_cancelled);
    suite.add("stop_sends_the_last_cancel_before_returning",
              test_stop_sends_the_last_cancel_before_returning);
    suite.add("leaving_during_prefill_cancels_at_the_first_token",
              test_leaving_during_prefill_cancels_at_the_first_token);
    suite.add("stopping_during_prefill_does_not_wait_for_the_first_token",
              test_stopping_during_prefill_does_not_wait_for_the_first_token);
    suite.add("upstream_dying_mid_stream_is_not_retried",
              test_upstream_dying_mid_stream_is_not_retried);
    return suite.run(argc, argv);
}
