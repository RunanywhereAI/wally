#include "test_common.h"

#include <string>
#include <thread>
#include <vector>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "fake_upstream.h"
#include "harness/harness.h"
#include "ide/openai_proxy.h"

// The JetBrains proxy's upstream connection behaviour (wally #80), against
// the fake upstreams in fake_upstream.h. Same properties as the Anthropic
// translator's suite: one connection across sequential requests, and a
// stale reused connection tried once more on a fresh one.

namespace {

using Json = nlohmann::json;
using wally_tests::Describe;
using wally_tests::FakeUpstream;
#if !defined(_WIN32)
using wally_tests::HalfOpenUpstream;
#endif

/// A port nothing is listening on right now. StartProxy needs a real number:
/// it writes the address into the editor's settings, so 0 is not an option.
///
/// The probe has to actually listen and then stop: httplib's stop() closes
/// the listening socket only while the server is running, and its destructor
/// does not close it at all, so a bind-and-drop probe leaks a listening
/// socket per call.
int FreePort() {
    httplib::Server probe;
    const int port = probe.bind_to_any_port("127.0.0.1");
    std::thread listener([&probe] { probe.listen_after_bind(); });
    probe.wait_until_ready();
    probe.stop();
    listener.join();
    return port;
}

/// A proxy started against `upstream_base_url`, stopped on scope exit.
class RunningProxy {
   public:
    explicit RunningProxy(const std::string& upstream_base_url) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream_base_url;
        endpoint.api_key = "test-upstream-key";
        started_ = wally::ide::StartProxy(endpoint, "glm-5.3", FreePort(), &proxy_, false);
    }
    ~RunningProxy() { wally::ide::StopProxy(&proxy_); }

    bool started() const { return started_; }

    /// One OpenAI-shaped request through the proxy; the status it answered
    /// with, or 0 when nothing came back.
    int Send(bool streaming) const {
        httplib::Client client(proxy_.base_url);
        client.set_read_timeout(10, 0);
        const Json body{{"model", "anything"},
                        {"stream", streaming},
                        {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
        const httplib::Result reply =
            client.Post("/v1/chat/completions",
                        {{"Authorization", "Bearer " + proxy_.auth_token}},
                        body.dump(), "application/json");
        return reply ? reply->status : 0;
    }

   private:
    wally::ide::Proxy proxy_;
    bool started_ = false;
};

TestResult test_proxy_sequential_requests_reuse_the_upstream_connection() {
    TestResult result;
    result.test_name = "proxy_sequential_requests_reuse_the_upstream_connection";
    FakeUpstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const int first = proxy.Send(true);
    const int second = proxy.Send(false);
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

TestResult test_proxy_stale_reused_connection_is_retried_once() {
    TestResult result;
    result.test_name = "proxy_stale_reused_connection_is_retried_once";
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
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const int first = proxy.Send(false);
    const int second = proxy.Send(false);
    const std::vector<int> ports = upstream.ports();
    result.expected =
        "200, 200; three upstream requests, the first two on one connection, the third on another";
    result.actual = std::to_string(first) + ", " + std::to_string(second) + "; " + Describe(ports);
    result.passed = first == 200 && second == 200 && ports.size() == 3 &&
                    ports[0] == ports[1] && ports[2] != ports[0];
    return result;
#endif
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_ide_proxy");
    suite.add("proxy_sequential_requests_reuse_the_upstream_connection",
              test_proxy_sequential_requests_reuse_the_upstream_connection);
    suite.add("proxy_stale_reused_connection_is_retried_once",
              test_proxy_stale_reused_connection_is_retried_once);
    return suite.run(argc, argv);
}
