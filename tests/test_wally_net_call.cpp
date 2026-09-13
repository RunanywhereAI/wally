#include "test_common.h"

#include <atomic>
#include <chrono>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "fake_upstream.h"
#include "net/upstream_call.h"
#include "net/upstream_pool.h"

// PostWatched (wally #81) against the fake upstream: the reader is a flag the
// test flips (the translators pass the server request's is_connection_closed;
// that wiring is proven in the shim suites), the upstream's headers and body
// can each be withheld, and the cancel is observed on the fake's cancel route
// only when the test's on_abandoned forwards it there -- what the runtime's
// cancel worker will do.

namespace {

using wally_tests::FakeUpstream;

const std::string kStreamRequest = R"({"model":"glm-5.3","messages":[],"stream":true})";

struct Recorded {
    std::atomic<int> abandoned_calls{0};
    std::string abandoned_id;
    int abandoned_status = -1;
    bool abandoned_during_prefill = false;
    std::atomic<bool> reader_gone{false};
    std::atomic<bool> stopping{false};
    std::string received;
    std::mutex mutex;
};

wally::net::WatchedCall CallFor(Recorded& rec, std::chrono::seconds id_wait = std::chrono::seconds(2)) {
    wally::net::WatchedCall call;
    call.path = "/v1/chat/completions";
    call.body = kStreamRequest;
    call.receiver = [&rec](const char* data, size_t length) {
        std::lock_guard<std::mutex> lock(rec.mutex);
        rec.received.append(data, length);
        return true;
    };
    call.reader_gone = [&rec] { return rec.reader_gone.load(); };
    call.stopping = [&rec] { return rec.stopping.load(); };
    call.on_abandoned = [&rec](const std::string& id, int status, bool during_prefill) {
        std::lock_guard<std::mutex> lock(rec.mutex);
        rec.abandoned_calls++;
        rec.abandoned_id = id;
        rec.abandoned_status = status;
        rec.abandoned_during_prefill = during_prefill;
    };
    call.poll = std::chrono::milliseconds(20);
    call.id_wait = id_wait;
    return call;
}

std::shared_ptr<wally::net::UpstreamPool> PoolFor(const FakeUpstream& upstream) {
    wally::net::UpstreamOptions options;
    options.origin = "http://127.0.0.1:" + std::to_string(std::stoi(upstream.base_url().substr(
                                                upstream.base_url().rfind(':') + 1)));
    return std::make_shared<wally::net::UpstreamPool>(options);
}

TestResult test_a_completed_stream_is_never_abandoned() {
    TestResult result;
    result.test_name = "a_completed_stream_is_never_abandoned";
    FakeUpstream upstream;
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    const auto out = wally::net::PostWatched(lease, CallFor(rec));
    if (!out.reply || out.reply->status != 200 || out.abandoned || !out.received_any) {
        result.details = "a normal stream must complete: error=" +
                         std::string(httplib::to_string(out.reply.error()));
        return result;
    }
    if (out.request_id != FakeUpstream::request_id_of(1) || out.status != 200) {
        result.details = "the response id must be captured from the headers: got " + out.request_id;
        return result;
    }
    if (rec.abandoned_calls.load() != 0 || rec.received.find("[DONE]") == std::string::npos) {
        result.details = "no abandon on a completed stream, and the whole body delivered";
        return result;
    }
    result.passed = true;
    return result;
}

// The reader leaves while the body is being withheld (the engine is decoding,
// the headers -- and so the id -- are already in hand): the cancel is named at
// once, the upstream socket is dropped, and nothing more reaches the sink.
TestResult test_reader_gone_mid_body_names_the_cancel_and_drops_the_socket() {
    TestResult result;
    result.test_name = "reader_gone_mid_body_names_the_cancel_and_drops_the_socket";
    FakeUpstream upstream;
    upstream.hold_streams_until(99);  // the body never comes on its own
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    std::thread leave([&] {
        std::this_thread::sleep_for(std::chrono::milliseconds(150));
        rec.reader_gone.store(true);
    });
    const auto started = std::chrono::steady_clock::now();
    const auto out = wally::net::PostWatched(lease, CallFor(rec));
    const auto took = std::chrono::steady_clock::now() - started;
    leave.join();
    if (!out.abandoned) {
        result.details = "the call must report the abandon";
        return result;
    }
    if (rec.abandoned_calls.load() != 1 || rec.abandoned_id != FakeUpstream::request_id_of(1) ||
        rec.abandoned_status != 200 || rec.abandoned_during_prefill) {
        result.details = "on_abandoned must fire once with the id and status, not during prefill: calls=" +
                         std::to_string(rec.abandoned_calls.load()) + " id=" + rec.abandoned_id;
        return result;
    }
    if (took > std::chrono::seconds(1)) {
        result.details = "the socket must be dropped promptly after the abandon, took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) + " ms";
        return result;
    }
    if (out.reply) {
        result.details = "a stopped call must not carry a completed reply";
        return result;
    }
    if (!rec.received.empty()) {
        result.details = "nothing may reach the sink after the reader left";
        return result;
    }
    result.passed = true;
    return result;
}

// The reader leaves while the HEADERS are withheld (prefill at the real
// endpoint): no id yet, so the call keeps the upstream open; when the headers
// arrive the response handler names the cancel and aborts the request.
TestResult test_reader_gone_during_prefill_cancels_at_the_headers() {
    TestResult result;
    result.test_name = "reader_gone_during_prefill_cancels_at_the_headers";
    FakeUpstream upstream;
    upstream.hold_headers(true);
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    std::thread script([&] {
        std::this_thread::sleep_for(std::chrono::milliseconds(150));
        rec.reader_gone.store(true);
        std::this_thread::sleep_for(std::chrono::milliseconds(300));
        if (rec.abandoned_calls.load() != 0) {
            return;  // named too early; the assertion below reports it
        }
        upstream.release_headers();
    });
    const auto out = wally::net::PostWatched(lease, CallFor(rec));
    script.join();
    if (!out.abandoned || rec.abandoned_calls.load() != 1 ||
        rec.abandoned_id != FakeUpstream::request_id_of(1) || !rec.abandoned_during_prefill) {
        result.details = "the cancel must be named exactly once, at the headers, as a prefill abandon: calls=" +
                         std::to_string(rec.abandoned_calls.load()) + " id=" + rec.abandoned_id;
        return result;
    }
    if (out.reply || out.reply.error() != httplib::Error::Canceled) {
        result.details = "the response handler must abort the request (Canceled), got " +
                         std::string(httplib::to_string(out.reply.error()));
        return result;
    }
    if (!rec.received.empty()) {
        result.details = "nothing may reach the sink";
        return result;
    }
    result.passed = true;
    return result;
}

// The reader leaves during prefill and the headers NEVER come: after id_wait
// the call gives up with an empty id and drops the socket; the fake's cancel
// route is never reached (there is nothing to name).
TestResult test_an_id_that_never_comes_gives_up_after_the_wait() {
    TestResult result;
    result.test_name = "an_id_that_never_comes_gives_up_after_the_wait";
    FakeUpstream upstream;
    upstream.hold_headers(true);
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    rec.reader_gone.store(true);  // gone before the request even goes out
    const auto started = std::chrono::steady_clock::now();
    const auto out = wally::net::PostWatched(lease, CallFor(rec, std::chrono::seconds(1)));
    const auto took = std::chrono::steady_clock::now() - started;
    upstream.release_headers();
    if (!out.abandoned || rec.abandoned_calls.load() != 1 || !rec.abandoned_id.empty() ||
        rec.abandoned_status != 0) {
        result.details = "on_abandoned must fire once with an empty id: calls=" +
                         std::to_string(rec.abandoned_calls.load()) + " id=" + rec.abandoned_id;
        return result;
    }
    if (took < std::chrono::milliseconds(900) || took > std::chrono::seconds(3)) {
        result.details = "the wait must be the configured id_wait, took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) + " ms";
        return result;
    }
    if (!upstream.cancels().empty()) {
        result.details = "nothing to name, so nothing may be cancelled";
        return result;
    }
    result.passed = true;
    return result;
}

// The wrapper is stopping while the id is still unknown: the call returns
// within a poll rather than waiting out id_wait.
TestResult test_stopping_ends_the_wait_for_an_id() {
    TestResult result;
    result.test_name = "stopping_ends_the_wait_for_an_id";
    FakeUpstream upstream;
    upstream.hold_headers(true);
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    rec.reader_gone.store(true);
    std::thread script([&] {
        std::this_thread::sleep_for(std::chrono::milliseconds(150));
        rec.stopping.store(true);
    });
    const auto started = std::chrono::steady_clock::now();
    const auto out = wally::net::PostWatched(lease, CallFor(rec, std::chrono::seconds(30)));
    const auto took = std::chrono::steady_clock::now() - started;
    script.join();
    upstream.release_headers();
    if (!out.abandoned || took > std::chrono::seconds(1)) {
        result.details = "stopping must end the wait within a poll, took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) + " ms";
        return result;
    }
    result.passed = true;
    return result;
}

// A refusal (429 here) ran nothing: a reader that left is not worth a cancel.
TestResult test_a_refusal_is_not_cancelled() {
    TestResult result;
    result.test_name = "a_refusal_is_not_cancelled";
    httplib::Server refusing;
    refusing.Post("/v1/chat/completions", [](const httplib::Request&, httplib::Response& response) {
        std::this_thread::sleep_for(std::chrono::milliseconds(300));
        response.status = 429;
        response.set_header("x-request-id", "req-refused");
        response.set_content("{\"error\":{\"message\":\"slow down\"}}", "application/json");
    });
    const int port = refusing.bind_to_any_port("127.0.0.1");
    std::thread thread([&] { refusing.listen_after_bind(); });
    refusing.wait_until_ready();
    wally::net::UpstreamOptions options;
    options.origin = "http://127.0.0.1:" + std::to_string(port);
    auto pool = std::make_shared<wally::net::UpstreamPool>(options);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    rec.reader_gone.store(true);
    const auto out = wally::net::PostWatched(lease, CallFor(rec));
    refusing.stop();
    thread.join();
    if (!out.abandoned || rec.abandoned_calls.load() != 0) {
        result.details = "a 4xx must not be cancelled: calls=" + std::to_string(rec.abandoned_calls.load());
        return result;
    }
    result.passed = true;
    return result;
}

// The reader is noticed leaving by the RECEIVER, not the poll: while tokens
// are flowing the next write to the editor fails before the watch gets its
// turn (the fake drips a frame every 5 ms; the reader flag is never set). A
// receiver saying no must count as an abandon -- the cancel named with the
// id, the request aborted -- rather than a plain stop nobody follows up.
TestResult test_a_receiver_that_refuses_the_bytes_names_the_cancel() {
    TestResult result;
    result.test_name = "a_receiver_that_refuses_the_bytes_names_the_cancel";
    FakeUpstream upstream;
    upstream.drip(400, 5);  // ~2 s of tokens, unless the reader drops out
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    wally::net::WatchedCall call = CallFor(rec, std::chrono::seconds(30));
    call.poll = std::chrono::seconds(30);  // the watch never gets a turn
    int frames = 0;
    call.receiver = [&](const char* data, size_t length) {
        std::lock_guard<std::mutex> lock(rec.mutex);
        rec.received.append(data, length);
        return ++frames < 10;  // the tenth write "fails": the editor is gone
    };
    const auto started = std::chrono::steady_clock::now();
    const auto out = wally::net::PostWatched(lease, call);
    const auto took = std::chrono::steady_clock::now() - started;
    if (!out.abandoned || rec.abandoned_calls.load() != 1 ||
        rec.abandoned_id != FakeUpstream::request_id_of(1) || rec.abandoned_status != 200 ||
        rec.abandoned_during_prefill) {
        result.details = "a refused write must be an abandon with the id in hand: abandoned=" +
                         std::string(out.abandoned ? "1" : "0") +
                         " calls=" + std::to_string(rec.abandoned_calls.load()) +
                         " id=" + rec.abandoned_id;
        return result;
    }
    if (out.reply || out.reply.error() != httplib::Error::Canceled) {
        result.details = "the request must be aborted (Canceled), got " +
                         std::string(httplib::to_string(out.reply.error()));
        return result;
    }
    if (took > std::chrono::seconds(1)) {
        result.details = "the call must end at the refused write, took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) + " ms";
        return result;
    }
    result.passed = true;
    return result;
}

// A call that completes must not wait out the watch's poll before returning:
// the watch is woken when send() returns. With poll = 2 s a completed call
// still returns in milliseconds.
TestResult test_a_completed_call_is_not_held_for_a_poll() {
    TestResult result;
    result.test_name = "a_completed_call_is_not_held_for_a_poll";
    FakeUpstream upstream;
    auto pool = PoolFor(upstream);
    Recorded rec;
    auto lease = pool->acquire("test-key");
    wally::net::WatchedCall call = CallFor(rec);
    call.poll = std::chrono::seconds(2);
    const auto started = std::chrono::steady_clock::now();
    const auto out = wally::net::PostWatched(lease, call);
    const auto took = std::chrono::steady_clock::now() - started;
    if (!out.reply || out.reply->status != 200 || out.abandoned) {
        result.details = "a normal stream must complete";
        return result;
    }
    if (took > std::chrono::milliseconds(500)) {
        result.details = "a completed call must return at once, not after the poll: took " +
                         std::to_string(std::chrono::duration_cast<std::chrono::milliseconds>(took).count()) + " ms";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_net_call");
    suite.add("a_completed_stream_is_never_abandoned", test_a_completed_stream_is_never_abandoned);
    suite.add("reader_gone_mid_body_names_the_cancel_and_drops_the_socket",
              test_reader_gone_mid_body_names_the_cancel_and_drops_the_socket);
    suite.add("reader_gone_during_prefill_cancels_at_the_headers",
              test_reader_gone_during_prefill_cancels_at_the_headers);
    suite.add("an_id_that_never_comes_gives_up_after_the_wait",
              test_an_id_that_never_comes_gives_up_after_the_wait);
    suite.add("stopping_ends_the_wait_for_an_id", test_stopping_ends_the_wait_for_an_id);
    suite.add("a_refusal_is_not_cancelled", test_a_refusal_is_not_cancelled);
    suite.add("a_receiver_that_refuses_the_bytes_names_the_cancel",
              test_a_receiver_that_refuses_the_bytes_names_the_cancel);
    suite.add("a_completed_call_is_not_held_for_a_poll", test_a_completed_call_is_not_held_for_a_poll);
    return suite.run(argc, argv);
}
