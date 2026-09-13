// The JetBrains proxy serves requests that carry no credential.
//
// It used to demand a per-session bearer token, and AI Assistant never sent
// one: a provider key can only be entered in the IDE's own settings dialog, so
// every chat request arrived bare and came back 401 — which the IDE reported to
// the reader as a licensing problem. `/v1/models` never checked the token, so
// the connection test passed and only chat failed, which is what made this hard
// to see. These pin the behaviour so the refusal cannot come back.

#include "test_common.h"

#include <string>
#include <thread>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "harness/harness.h"
#include "ide/openai_proxy.h"

namespace {

using Json = nlohmann::json;

/// A mock OpenAI endpoint on its own port, answering both shapes the proxy
/// forwards: one completion, and an event stream.
class Upstream {
   public:
    Upstream() {
        server_.Post("/v1/chat/completions",
                     [](const httplib::Request& request, httplib::Response& response) {
                         const bool streaming =
                             request.body.find("\"stream\":true") != std::string::npos;
                         if (!streaming) {
                             response.set_content(
                                 Json{{"id", "chatcmpl-test"},
                                      {"object", "chat.completion"},
                                      {"choices", Json::array({Json{
                                           {"index", 0},
                                           {"message", {{"role", "assistant"}, {"content", "pong"}}},
                                           {"finish_reason", "stop"}}})}}
                                     .dump(),
                                 "application/json");
                             return;
                         }
                         const Json chunk{
                             {"id", "chatcmpl-test"},
                             {"object", "chat.completion.chunk"},
                             {"choices", Json::array({Json{{"index", 0},
                                                           {"delta", {{"content", "pong"}}}}})}};
                         response.set_content("data: " + chunk.dump() + "\n\ndata: [DONE]\n\n",
                                              "text/event-stream");
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

/// A free port. StartProxy needs a real number: it writes the address into the
/// editor's settings, so 0 is not an option. The probe listens and stops,
/// because httplib closes its listening socket only while running.
int FreePort() {
    httplib::Server probe;
    const int port = probe.bind_to_any_port("127.0.0.1");
    std::thread listener([&probe] { probe.listen_after_bind(); });
    probe.wait_until_ready();
    probe.stop();
    listener.join();
    return port;
}

class RunningProxy {
   public:
    explicit RunningProxy(const std::string& upstream_base_url) {
        wally::harness::Endpoint endpoint;
        endpoint.base_url = upstream_base_url;
        endpoint.api_key = "test-upstream-key";
        started_ = wally::ide::StartProxy(endpoint, "glm-5.3-flash", FreePort(), &proxy_, false);
    }
    ~RunningProxy() { wally::ide::StopProxy(&proxy_); }

    bool started() const { return started_; }
    const std::string& base_url() const { return proxy_.base_url; }

   private:
    wally::ide::Proxy proxy_;
    bool started_ = false;
};

httplib::Result Ask(const RunningProxy& proxy, bool streaming, const httplib::Headers& headers) {
    httplib::Client client(proxy.base_url());
    client.set_read_timeout(10, 0);
    const Json body{{"model", "anything"},
                    {"stream", streaming},
                    {"messages", Json::array({Json{{"role", "user"}, {"content", "hi"}}})}};
    return client.Post("/v1/chat/completions", headers, body.dump(), "application/json");
}

TestResult test_serves_a_completion_with_no_credential() {
    TestResult result;
    result.test_name = "serves_a_completion_with_no_credential";
    Upstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const httplib::Result reply = Ask(proxy, false, {});
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "200";
    result.actual = std::to_string(reply->status) + " " + reply->body.substr(0, 120);
    if (reply->status == 401) {
        result.details = "the proxy refused the editor it exists to serve";
        return result;
    }
    if (reply->status != 200 || reply->body.find("pong") == std::string::npos) {
        result.details = "an unauthenticated completion was not served";
        return result;
    }
    result.passed = true;
    return result;
}

// Streaming is the path the editor actually uses, and it takes a different
// route through the handler.
TestResult test_streams_with_no_credential() {
    TestResult result;
    result.test_name = "streams_with_no_credential";
    Upstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const httplib::Result reply = Ask(proxy, true, {});
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "200 carrying events";
    result.actual = std::to_string(reply->status) + " " + reply->body.substr(0, 120);
    if (reply->status != 200) {
        result.details = "an unauthenticated stream was not served";
        return result;
    }
    if (reply->body.find("data:") == std::string::npos ||
        reply->body.find("pong") == std::string::npos) {
        result.details = "the stream carried no events";
        return result;
    }
    result.passed = true;
    return result;
}

// An editor that does send something — a key typed into its settings by hand,
// or a stale one from an earlier build — must not be refused either. Nothing
// here validates the value, so any of them is served.
TestResult test_serves_a_request_carrying_any_credential() {
    TestResult result;
    result.test_name = "serves_a_request_carrying_any_credential";
    Upstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const httplib::Headers stale{{"Authorization", "Bearer a-key-from-an-older-run"}};
    const httplib::Result reply = Ask(proxy, false, stale);
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "200";
    result.actual = std::to_string(reply->status);
    if (reply->status != 200) {
        result.details = "a request carrying an unrecognised key was refused";
        return result;
    }
    result.passed = true;
    return result;
}

// The model list is what the IDE's connection test calls. It answered before
// this change and must keep answering, or the provider reads as unreachable.
TestResult test_lists_models_with_no_credential() {
    TestResult result;
    result.test_name = "lists_models_with_no_credential";
    Upstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    httplib::Client client(proxy.base_url());
    client.set_read_timeout(10, 0);
    const httplib::Result reply = client.Get("/v1/models");
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "200 naming the served model";
    result.actual = std::to_string(reply->status) + " " + reply->body.substr(0, 120);
    if (reply->status != 200 || reply->body.find("glm-5.3-flash") == std::string::npos) {
        result.details = "the model list did not name the served model";
        return result;
    }
    result.passed = true;
    return result;
}

// The model the caller names is exactly what must not reach the endpoint: wally
// was told which model to serve. A body that cannot be rewritten cannot be
// safely forwarded, so it is refused rather than passed through carrying the
// caller's own model name.
TestResult test_refuses_a_body_it_cannot_retarget() {
    TestResult result;
    result.test_name = "refuses_a_body_it_cannot_retarget";
    Upstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    httplib::Client client(proxy.base_url());
    client.set_read_timeout(10, 0);
    const httplib::Result reply = client.Post(
        "/v1/chat/completions", "{\"model\":\"gpt-4\",\"messages\":[", "application/json");
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "400";
    result.actual = std::to_string(reply->status) + " " + reply->body.substr(0, 120);
    if (reply->status == 200) {
        result.details = "a body carrying the caller's own model was forwarded unchanged";
        return result;
    }
    if (reply->status != 400) {
        result.details = "a malformed body was not refused as a bad request";
        return result;
    }
    result.passed = true;
    return result;
}

// An upstream that answers 401 with an expiry message gets one more attempt on
// a renewed session. Without a refresh token there is nothing to renew, so the
// refusal is passed on — but it must be passed on as itself, not swallowed.
TestResult test_an_expired_session_is_not_reported_as_something_else() {
    TestResult result;
    result.test_name = "an_expired_session_is_not_reported_as_something_else";
    httplib::Server upstream;
    int attempts = 0;
    upstream.Post("/v1/chat/completions",
                  [&attempts](const httplib::Request&, httplib::Response& response) {
                      ++attempts;
                      response.status = 401;
                      response.set_content(
                          Json{{"error", {{"message", "access token expired"}}}}.dump(),
                          "application/json");
                  });
    const int port = upstream.bind_to_any_port("127.0.0.1");
    std::thread listener([&upstream] { upstream.listen_after_bind(); });
    upstream.wait_until_ready();

    {
        RunningProxy proxy("http://127.0.0.1:" + std::to_string(port) + "/v1");
        if (!proxy.started()) {
            upstream.stop();
            listener.join();
            result.details = "proxy did not start";
            return result;
        }
        const httplib::Result reply = Ask(proxy, false, {});
        upstream.stop();
        listener.join();
        if (!reply) {
            result.details = "the proxy did not answer";
            return result;
        }
        result.expected = "401 carrying the upstream's own reason";
        result.actual = std::to_string(reply->status) + " " + reply->body.substr(0, 120);
        if (reply->status != 401) {
            result.details = "the upstream's refusal was rewritten";
            return result;
        }
        if (reply->body.find("expired") == std::string::npos) {
            result.details = "the reason the session failed was lost";
            return result;
        }
        // One attempt when there is no session to renew; never a silent loop.
        if (attempts < 1 || attempts > 2) {
            result.details = "the upstream was called " + std::to_string(attempts) + " times";
            return result;
        }
    }
    result.passed = true;
    return result;
}

/// An upstream that refuses empty tool-call arguments the way the hosted one
/// does, and records what it was sent.
class ToolCallUpstream {
   public:
    ToolCallUpstream() {
        server_.Post("/v1/chat/completions",
                     [this](const httplib::Request& request, httplib::Response& response) {
                         last_body_ = request.body;
                         const Json parsed = Json::parse(request.body, nullptr, false);
                         bool empty_arguments = false;
                         if (parsed.is_object() && parsed.contains("messages")) {
                             for (const Json& message : parsed["messages"]) {
                                 if (!message.is_object() || !message.contains("tool_calls")) {
                                     continue;
                                 }
                                 for (const Json& call : message["tool_calls"]) {
                                     const Json& arguments = call["function"]["arguments"];
                                     if (!arguments.is_string() ||
                                         arguments.get<std::string>().empty()) {
                                         empty_arguments = true;
                                     }
                                 }
                             }
                         }
                         if (empty_arguments) {
                             response.status = 400;
                             response.set_content(
                                 Json{{"error",
                                       {{"message",
                                         "Assistant tool call function.arguments must be a "
                                         "JSON object."}}}}
                                     .dump(),
                                 "application/json");
                             return;
                         }
                         response.set_content(
                             Json{{"id", "chatcmpl-test"},
                                  {"object", "chat.completion"},
                                  {"choices", Json::array({Json{
                                       {"index", 0},
                                       {"message", {{"role", "assistant"}, {"content", "pong"}}},
                                       {"finish_reason", "stop"}}})}}
                                 .dump(),
                             "application/json");
                     });
        port_ = server_.bind_to_any_port("127.0.0.1");
        thread_ = std::thread([this] { server_.listen_after_bind(); });
        server_.wait_until_ready();
    }
    ~ToolCallUpstream() {
        server_.stop();
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    std::string base_url() const { return "http://127.0.0.1:" + std::to_string(port_) + "/v1"; }
    const std::string& last_body() const { return last_body_; }

   private:
    httplib::Server server_;
    std::thread thread_;
    std::string last_body_;
    int port_ = 0;
};

// A streamed tool call's first fragment carries `arguments: ""`. A client that
// keeps that fragment rather than joining them replays it on the next turn, and
// the endpoint refuses its own model's output — the reader sees whatever they
// asked for fail, with an error naming neither the call nor the message.
TestResult test_repairs_empty_tool_call_arguments() {
    TestResult result;
    result.test_name = "repairs_empty_tool_call_arguments";
    ToolCallUpstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }

    const Json body{
        {"model", "anything"},
        {"stream", false},
        {"messages",
         Json::array(
             {Json{{"role", "user"}, {"content", "make a ui"}},
              Json{{"role", "assistant"},
                   {"tool_calls",
                    Json::array({Json{{"id", "call_1"},
                                      {"type", "function"},
                                      {"function", {{"name", "list_files"}, {"arguments", ""}}}}})}},
              Json{{"role", "tool"}, {"tool_call_id", "call_1"}, {"content", "ok"}}})}};
    httplib::Client client(proxy.base_url());
    client.set_read_timeout(10, 0);
    const httplib::Result reply =
        client.Post("/v1/chat/completions", body.dump(), "application/json");
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "200, with empty arguments repaired to {}";
    result.actual = std::to_string(reply->status) + " sent=" + upstream.last_body().substr(0, 200);
    if (reply->status != 200) {
        result.details = "the endpoint refused a tool call the proxy should have repaired";
        return result;
    }
    if (upstream.last_body().find("\"arguments\":\"{}\"") == std::string::npos) {
        result.details = "the empty arguments were not repaired on the way through";
        return result;
    }
    result.passed = true;
    return result;
}

// Arguments that are already a JSON string are the ordinary case and must pass
// through exactly as written.
TestResult test_leaves_real_tool_call_arguments_alone() {
    TestResult result;
    result.test_name = "leaves_real_tool_call_arguments_alone";
    ToolCallUpstream upstream;
    RunningProxy proxy(upstream.base_url());
    if (!proxy.started()) {
        result.details = "proxy did not start";
        return result;
    }
    const Json body{
        {"model", "anything"},
        {"stream", false},
        {"messages",
         Json::array({Json{{"role", "assistant"},
                           {"tool_calls",
                            Json::array({Json{
                                {"id", "call_1"},
                                {"type", "function"},
                                {"function",
                                 {{"name", "write_file"},
                                  {"arguments", "{\"path\":\"src/main.rs\"}"}}}}})}}})}};
    httplib::Client client(proxy.base_url());
    client.set_read_timeout(10, 0);
    const httplib::Result reply =
        client.Post("/v1/chat/completions", body.dump(), "application/json");
    if (!reply) {
        result.details = "the proxy did not answer";
        return result;
    }
    result.expected = "the arguments forwarded unchanged";
    result.actual = upstream.last_body().substr(0, 200);
    if (upstream.last_body().find("src/main.rs") == std::string::npos) {
        result.details = "real tool call arguments were altered";
        return result;
    }
    result.passed = true;
    return result;
}

}  // namespace

int main(int argc, char** argv) {
    TestSuite suite("wally_ide_proxy_auth");
    suite.add("serves_a_completion_with_no_credential", test_serves_a_completion_with_no_credential);
    suite.add("streams_with_no_credential", test_streams_with_no_credential);
    suite.add("serves_a_request_carrying_any_credential",
              test_serves_a_request_carrying_any_credential);
    suite.add("lists_models_with_no_credential", test_lists_models_with_no_credential);
    suite.add("refuses_a_body_it_cannot_retarget", test_refuses_a_body_it_cannot_retarget);
    suite.add("repairs_empty_tool_call_arguments", test_repairs_empty_tool_call_arguments);
    suite.add("leaves_real_tool_call_arguments_alone",
              test_leaves_real_tool_call_arguments_alone);
    suite.add("an_expired_session_is_not_reported_as_something_else",
              test_an_expired_session_is_not_reported_as_something_else);
    return suite.run(argc, argv);
}
