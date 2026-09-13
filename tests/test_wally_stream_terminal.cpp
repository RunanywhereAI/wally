#include "test_common.h"
#include <httplib.h>
#include <nlohmann/json.hpp>
#include <thread>
#include "anthropic/messages.h"
#include "harness/harness.h"

namespace {
TestResult test_stream_terminal_contract() {
    TestResult result;
    result.test_name = "stream_terminal_contract";
    const std::string text = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
    const std::string finish = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    const std::string done = "data: [DONE]\n\n";
    const std::string tool = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":\"}}]},\"finish_reason\":null}]}\n\n";
    const std::string tool_end = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
    const std::string tool_rest = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":null}]}\n\n";
    const std::string usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3}}\n\n";
    std::string crlf;
    for (char c : text + finish + usage + done) {
        if (c == '\n') crlf += '\r';
        crlf += c;
    }
    struct Case { std::string name, body; bool valid; bool tools; };
    const Case cases[] = {
        {"empty EOF", "", false, false},
        {"text EOF", text, false, false},
        {"finish without DONE", text + finish, false, false},
        {"DONE without finish", text + done, false, false},
        {"tool EOF", tool, false, false},
        {"incomplete tool despite terminal", tool + tool_end + done, false, false},
        {"malformed frame", text + "data: {broken}\n\n" + finish + done, false, false},
        {"unknown finish", text + "data: {\"choices\":[{\"finish_reason\":\"bogus\"}]}\n\n" + done, false, false},
        {"unterminated DONE", text + finish + "data: [DONE]", false, false},
        {"data after DONE", text + finish + done + text, false, false},
        {"choice after finish", text + finish + text + done, false, false},
        {"non-object JSON frame", text + "data: []\n\n" + finish + done, false, false},
        {"numeric tool arguments", "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"lookup\",\"arguments\":123}}]}}]}\n\n" + tool_end + done, false, false},
        {"missing tool arguments", "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"lookup\"}}]}}]}\n\n" + tool_end + done, false, false},
        {"malformed tool calls", "data: {\"choices\":[{\"delta\":{\"tool_calls\":{}}}]}\n\n" + finish + done, false, false},
        {"malformed delta", "data: {\"choices\":[{\"delta\":3}]}\n\n" + finish + done, false, false},
        {"error before terminal", text + "data: {\"error\":{\"message\":\"failed\"}}\n\n" + finish + done, false, false},
        {"normal text and usage", text + finish + usage + done, true, false},
        {"normal tool and usage", tool + tool_rest + tool_end + usage + done, true, true},
        {"CRLF stream", crlf, true, false},
        {"multiline data and comments", ": heartbeat\n\ndata: {\"choices\":\ndata: [{\"delta\":{\"content\":\"hi\"}}]}\n\n" + finish + usage + done, true, false},
    };
    for (const auto& test : cases) {
        httplib::Server upstream;
        upstream.Post("/v1/chat/completions", [&](const httplib::Request&, httplib::Response& res) {
            res.set_chunked_content_provider("text/event-stream", [&](size_t offset, httplib::DataSink& sink) {
                if (offset >= test.body.size()) { sink.done(); return true; }
                const size_t count = std::min<size_t>(7, test.body.size() - offset);
                return sink.write(test.body.data() + offset, count);
            });
        });
        const int port = upstream.bind_to_any_port("127.0.0.1");
        if (port <= 0) { result.details = "upstream bind failed"; return result; }
        std::thread server([&] { upstream.listen_after_bind(); });
        wally::harness::Endpoint endpoint;
        endpoint.base_url = "http://127.0.0.1:" + std::to_string(port) + "/v1";
        wally::anthropic::Shim shim;
        const bool started = wally::anthropic::Start(endpoint, "test-model", &shim);
        httplib::Client client(shim.base_url);
        client.set_read_timeout(10, 0);
        const auto reply = client.Post("/v1/messages", {{"Authorization", "Bearer " + shim.auth_token}},
            R"({"model":"test","stream":true,"max_tokens":16,"messages":[{"role":"user","content":"hi"}]})", "application/json");
        wally::anthropic::Stop(&shim);
        upstream.stop();
        server.join();
        const std::string body = reply ? reply->body : "";
        const auto stop = body.find("event: message_stop");
        const bool exactly_one_stop = stop != std::string::npos && body.find("event: message_stop", stop + 1) == std::string::npos;
        const bool error = body.find("event: error") != std::string::npos;
        const bool emitted_tool = body.find("\"type\":\"tool_use\"") != std::string::npos;
        if (!started || !reply || reply->status != 200 ||
            (test.valid ? (!exactly_one_stop || error || emitted_tool != test.tools || body.find("\"input_tokens\":12") == std::string::npos)
                        : (stop != std::string::npos || !error || emitted_tool))) {
            result.details += test.name + ": " + body + "\n";
        }
    }
    result.passed = result.details.empty();
    return result;
}
}
int main(int argc, char** argv) {
    TestSuite suite("wally_stream_terminal");
    suite.add("stream_terminal_contract", test_stream_terminal_contract);
    return suite.run(argc, argv);
}
