#include "test_common.h"

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "anthropic/messages.h"
#include "harness/harness.h"
#include "net/upstream_pool.h"

// The translator's upstream connection behaviour, proven against a fake
// OpenAI-shaped server on loopback. Every assertion here is about which TCP
// connection a request arrived on, read from the server's side as the peer's
// ephemeral port: the same port across requests means the same connection was
// reused; a different port means a new connect (and, against the real
// endpoint, a new TLS handshake). No network beyond 127.0.0.1, no models.

namespace {

using Json = nlohmann::json;

constexpr const char* kStreamBody =
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],"
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n"
    "data: [DONE]\n\n";

constexpr const char* kJsonBody =
    "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,"
    "\"message\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}],"
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}";

/// An OpenAI-shaped upstream that remembers which connection each request
/// arrived on. `hold_streams_until` makes streaming responses wait until that
/// many requests have arrived before emitting anything, so two concurrent
/// requests are provably in flight together rather than one finishing and
/// lending its connection to the next.
class FakeUpstream {
   public:
    FakeUpstream() {
        server_.Post("/v1/chat/completions",
                     [this](const httplib::Request& request, httplib::Response& response) {
                         Record(request);
                         const bool streaming = Json::parse(request.body).value("stream", false);
                         if (!streaming) {
                             response.set_content(kJsonBody, "application/json");
                             return;
                         }
                         response.set_chunked_content_provider(
                             "text/event-stream",
                             [this](size_t, httplib::DataSink& sink) {
                                 WaitForHold();
                                 const std::string body = kStreamBody;
                                 sink.write(body.data(), body.size());
                                 sink.done();
                                 return true;
                             });
                     });
        port_ = server_.bind_to_any_port("127.0.0.1");
        thread_ = std::thread([this] { server_.listen_after_bind(); });
        server_.wait_until_ready();
    }

    ~FakeUpstream() {
        server_.stop();
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    std::string base_url() const { return "http://127.0.0.1:" + std::to_string(port_) + "/v1"; }

    std::vector<int> ports() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_;
    }

    void hold_streams_until(int arrivals) { hold_until_ = arrivals; }

   private:
    void Record(const httplib::Request& request) {
        std::lock_guard<std::mutex> lock(mutex_);
        ports_.push_back(request.remote_port);
        ++arrivals_;
        arrived_.notify_all();
    }

    void WaitForHold() {
        std::unique_lock<std::mutex> lock(mutex_);
        arrived_.wait_for(lock, std::chrono::seconds(5),
                          [this] { return arrivals_ >= hold_until_; });
    }

    httplib::Server server_;
    std::thread thread_;
    int port_ = 0;
    mutable std::mutex mutex_;
    std::condition_variable arrived_;
    std::vector<int> ports_;
    int arrivals_ = 0;
    int hold_until_ = 0;
};

/// A translator started against `upstream`, stopped on scope exit.
class RunningShim {
   public:
    explicit RunningShim(const FakeUpstream& upstream) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream.base_url();
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

std::string Describe(const std::vector<int>& ports) {
    std::string out = "[";
    for (size_t i = 0; i < ports.size(); ++i) {
        out += (i ? ", " : "") + std::to_string(ports[i]);
    }
    return out + "]";
}

// Test A. Two requests, one after the other, must arrive at the upstream on
// the same connection. Building a client per request (the behaviour #80
// fixes) opens a new connection each time, so the ports differ.
TestResult test_sequential_requests_reuse_the_upstream_connection() {
    TestResult result;
    result.test_name = "sequential_requests_reuse_the_upstream_connection";
    FakeUpstream upstream;
    RunningShim shim(upstream);
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
    RunningShim shim(upstream);
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
    return suite.run(argc, argv);
}
