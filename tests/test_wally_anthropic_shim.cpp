// The Anthropic shim over a real socket.
//
// These run a mock upstream on loopback, start the real shim in front of it, and
// talk to the shim with a real HTTP client. Nothing is stubbed, because the bug
// they pin (#83) is an ordering bug between writing the downstream headers and
// making the upstream call: a mocked transport cannot see it.

#include "test_common.h"

#include <atomic>
#include <memory>
#include <string>
#include <thread>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "anthropic/messages.h"
#include "harness/harness.h"

namespace {

using Json = nlohmann::json;

// A mock upstream OpenAI endpoint on its own port, torn down with the object.
class Upstream {
   public:
    using Handler = std::function<void(const httplib::Request&, httplib::Response&)>;

    explicit Upstream(Handler handler) {
        server_.Post("/v1/chat/completions",
                     [handler](const httplib::Request& request, httplib::Response& response) {
                         handler(request, response);
                     });
        port_ = server_.bind_to_any_port("127.0.0.1");
        thread_ = std::thread([this] { server_.listen_after_bind(); });
        server_.wait_until_ready();
    }

    ~Upstream() {
        server_.stop();
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    std::string base_url() const { return "http://127.0.0.1:" + std::to_string(port_) + "/v1"; }

   private:
    httplib::Server server_;
    std::thread thread_;
    int port_ = 0;
};

// The shim in front of `upstream`, stopped with the object.
class Shim {
   public:
    explicit Shim(const Upstream& upstream) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream.base_url();
        started_ = wally::anthropic::Start(endpoint, "glm-5.3-flash", &shim_);
    }
    ~Shim() { wally::anthropic::Stop(&shim_); }

    bool started() const { return started_; }
    const wally::anthropic::Shim& handle() const { return shim_; }

    // One POST /v1/messages, authenticated the way the wrapped tool does.
    httplib::Result Post(const Json& body) const {
        httplib::Client client("127.0.0.1", Port());
        client.set_read_timeout(30, 0);
        httplib::Headers headers{{"x-api-key", shim_.auth_token}};
        return client.Post("/v1/messages", headers, body.dump(), "application/json");
    }

   private:
    int Port() const {
        const std::size_t colon = shim_.base_url.rfind(':');
        return std::stoi(shim_.base_url.substr(colon + 1));
    }

    wally::anthropic::Shim shim_;
    bool started_ = false;
};

// The text the stream actually carried, reassembled from its text_delta events.
// OpenAI splits a word across chunks as it pleases, so the deltas are only
// meaningful once they are joined back up.
std::string StreamedText(const std::string& body) {
    std::string text;
    std::size_t at = 0;
    const std::string marker = "data: ";
    while ((at = body.find(marker, at)) != std::string::npos) {
        at += marker.size();
        const std::size_t end = body.find('\n', at);
        if (end == std::string::npos) {
            break;
        }
        Json event;
        try {
            event = Json::parse(body.substr(at, end - at));
        } catch (const Json::exception&) {
            continue;
        }
        if (event.value("type", "") == "content_block_delta") {
            text += event["delta"].value("text", "");
        }
    }
    return text;
}

Json StreamingRequest() {
    return Json{{"model", "claude-sonnet-4"},
                {"stream", true},
                {"max_tokens", 128},
                {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
}

// A refusal the endpoint makes before it has produced a single token. This is the
// case the client can still act on, so the status and the delay have to survive.
TestResult test_streaming_overload_keeps_status_and_retry_after() {
    TestResult result;
    result.test_name = "streaming_overload_keeps_status_and_retry_after";

    Upstream upstream([](const httplib::Request&, httplib::Response& response) {
        response.status = 429;
        response.set_header("Retry-After", "7");
        response.set_content(Json{{"error", {{"message", "Too many parallel requests"}}}}.dump(),
                             "application/json");
    });
    Shim shim(upstream);
    if (!shim.started()) {
        result.details = "the shim did not start";
        return result;
    }

    const httplib::Result reply = shim.Post(StreamingRequest());
    if (!reply) {
        result.details = "the shim did not answer";
        return result;
    }
    result.expected = "429 with Retry-After: 7";
    result.actual = std::to_string(reply->status) + " with Retry-After: " +
                    reply->get_header_value("Retry-After");
    if (reply->status != 429) {
        result.details = "a streaming overload must keep its 429, not become a 200 carrying an "
                         "SSE error";
        return result;
    }
    if (reply->get_header_value("Retry-After") != "7") {
        result.details = "the upstream Retry-After must survive the streaming path";
        return result;
    }
    // The failure is an error document, not a turn the model took.
    Json body;
    try {
        body = Json::parse(reply->body);
    } catch (const Json::exception& error) {
        result.details = std::string("the 429 body is not JSON: ") + error.what();
        return result;
    }
    if (body.value("type", "") != "error" ||
        body["error"].value("type", "") != "rate_limit_error") {
        result.details = "a 429 must read as an Anthropic rate_limit_error";
        return result;
    }
    result.passed = true;
    return result;
}

// 503 has no Retry-After to carry, and must still arrive as itself.
TestResult test_streaming_unavailable_keeps_its_status() {
    TestResult result;
    result.test_name = "streaming_unavailable_keeps_its_status";

    Upstream upstream([](const httplib::Request&, httplib::Response& response) {
        response.status = 503;
        response.set_content(Json{{"error", {{"message", "no capacity"}}}}.dump(),
                             "application/json");
    });
    Shim shim(upstream);
    if (!shim.started()) {
        result.details = "the shim did not start";
        return result;
    }

    const httplib::Result reply = shim.Post(StreamingRequest());
    if (!reply) {
        result.details = "the shim did not answer";
        return result;
    }
    result.expected = "503";
    result.actual = std::to_string(reply->status);
    if (reply->status != 503) {
        result.details = "a streaming 503 must not be rewritten as a successful stream";
        return result;
    }
    result.passed = true;
    return result;
}

// The ordinary path still streams, and still closes itself properly.
TestResult test_streaming_success_still_streams() {
    TestResult result;
    result.test_name = "streaming_success_still_streams";

    Upstream upstream([](const httplib::Request&, httplib::Response& response) {
        const Json first{
            {"choices", Json::array({Json{{"index", 0},
                                          {"delta", {{"role", "assistant"}, {"content", "he"}}}}})}};
        const Json second{
            {"choices", Json::array({Json{{"index", 0}, {"delta", {{"content", "llo"}}}}})}};
        const Json last{{"choices", Json::array({Json{{"index", 0},
                                                      {"delta", Json::object()},
                                                      {"finish_reason", "stop"}}})}};
        const std::string body = "data: " + first.dump() + "\n\ndata: " + second.dump() +
                                 "\n\ndata: " + last.dump() + "\n\ndata: [DONE]\n\n";
        response.set_content(body, "text/event-stream");
    });
    Shim shim(upstream);
    if (!shim.started()) {
        result.details = "the shim did not start";
        return result;
    }

    const httplib::Result reply = shim.Post(StreamingRequest());
    if (!reply) {
        result.details = "the shim did not answer";
        return result;
    }
    result.expected = "200 with message_start, text, message_stop";
    result.actual = std::to_string(reply->status) + " body=" + reply->body.substr(0, 200);
    if (reply->status != 200) {
        result.details = "a good stream must stay a 200";
        return result;
    }
    const std::string text = StreamedText(reply->body);
    result.actual = std::to_string(reply->status) + " text=" + text;
    if (reply->body.find("event: message_start") == std::string::npos) {
        result.details = "the translated stream lost its message_start";
        return result;
    }
    if (text != "hello") {
        result.details = "the translated stream lost its text, got: " + text;
        return result;
    }
    if (reply->body.find("event: message_stop") == std::string::npos) {
        result.details = "the translated stream lost its message_stop";
        return result;
    }
    if (reply->body.find("event: error") != std::string::npos) {
        result.details = "a successful stream must carry no error event";
        return result;
    }
    result.passed = true;
    return result;
}

// The path that was already correct, kept correct: a non-streaming refusal.
TestResult test_nonstreaming_overload_keeps_status_and_retry_after() {
    TestResult result;
    result.test_name = "nonstreaming_overload_keeps_status_and_retry_after";

    Upstream upstream([](const httplib::Request&, httplib::Response& response) {
        response.status = 429;
        response.set_header("Retry-After", "3");
        response.set_content(Json{{"error", {{"message", "slow down"}}}}.dump(),
                             "application/json");
    });
    Shim shim(upstream);
    if (!shim.started()) {
        result.details = "the shim did not start";
        return result;
    }

    Json request = StreamingRequest();
    request["stream"] = false;
    const httplib::Result reply = shim.Post(request);
    if (!reply) {
        result.details = "the shim did not answer";
        return result;
    }
    result.expected = "429 with Retry-After: 3";
    result.actual = std::to_string(reply->status) + " with Retry-After: " +
                    reply->get_header_value("Retry-After");
    if (reply->status != 429 || reply->get_header_value("Retry-After") != "3") {
        result.details = "the non-streaming path lost the status or the delay";
        return result;
    }
    result.passed = true;
    return result;
}

// A stream that dies after the first token. The 200 is already spent, so this can
// only be a typed error event, but it must never look like a clean finish.
TestResult test_stream_cut_short_is_an_error_not_a_clean_stop() {
    TestResult result;
    result.test_name = "stream_cut_short_is_an_error_not_a_clean_stop";

    Upstream upstream([](const httplib::Request&, httplib::Response& response) {
        const Json first{
            {"choices", Json::array({Json{{"index", 0},
                                          {"delta", {{"role", "assistant"}, {"content", "he"}}}}})}};
        // Content-Length promises more than the body delivers, so the client sees
        // the connection end mid-stream.
        response.set_header("Content-Type", "text/event-stream");
        response.set_content_provider(
            4096, "text/event-stream",
            [first](std::size_t, std::size_t, httplib::DataSink& sink) {
                const std::string chunk = "data: " + first.dump() + "\n\n";
                sink.write(chunk.data(), chunk.size());
                return false;
            });
    });
    Shim shim(upstream);
    if (!shim.started()) {
        result.details = "the shim did not start";
        return result;
    }

    const httplib::Result reply = shim.Post(StreamingRequest());
    if (!reply) {
        // The shim's own connection died with the upstream's. That is a failure the
        // client can see, which is the point of the test.
        result.passed = true;
        return result;
    }
    result.expected = "an error event, and no message_stop";
    result.actual = "status=" + std::to_string(reply->status) + " body=" + reply->body.substr(0, 300);
    if (reply->body.find("event: error") == std::string::npos) {
        result.details = "a stream cut short must say so";
        return result;
    }
    if (reply->body.find("event: message_stop") != std::string::npos) {
        result.details = "a stream cut short must not be closed off as a normal turn";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_anthropic_shim");
    suite.add("streaming_overload_keeps_status_and_retry_after",
              test_streaming_overload_keeps_status_and_retry_after);
    suite.add("streaming_unavailable_keeps_its_status", test_streaming_unavailable_keeps_its_status);
    suite.add("streaming_success_still_streams", test_streaming_success_still_streams);
    suite.add("nonstreaming_overload_keeps_status_and_retry_after",
              test_nonstreaming_overload_keeps_status_and_retry_after);
    suite.add("stream_cut_short_is_an_error_not_a_clean_stop",
              test_stream_cut_short_is_an_error_not_a_clean_stop);
    return suite.run(argc, argv);
}
