#include "anthropic/messages.h"

#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <cstddef>
#include <ctime>
#include <deque>
#include <filesystem>
#include <fstream>
#include <memory>
#include <mutex>
#include <string>
#include <system_error>
#include <thread>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "anthropic/translate.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "net/loopback_auth.h"

namespace wally::anthropic {
namespace {

using Json = nlohmann::json;

/// Appends one line about a failed upstream call to a log file, best effort.
///
/// A file, not stderr: the wrapped tool (Claude Code) owns the terminal, and a
/// line printed into its TUI corrupts the display — which is why a real error
/// used to vanish into a blind "API error, retrying" with nowhere to look. The
/// upstream response body is recorded; the bearer token never is (it is only
/// ever on the request, never echoed here).
void LogUpstreamError(const std::string& model, bool streaming, int status,
                      const std::string& body) {
    const std::string dir = paths::state_dir();
    if (dir.empty()) {
        return;
    }
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);
    std::ofstream log(dir + "/shim.log", std::ios::app);
    if (!log.good()) {
        return;
    }
    const std::time_t now = std::time(nullptr);
    std::tm utc{};
#if defined(_WIN32)
    gmtime_s(&utc, &now);
#else
    gmtime_r(&now, &utc);
#endif
    char when[32] = {0};
    std::strftime(when, sizeof(when), "%Y-%m-%dT%H:%M:%SZ", &utc);
    std::string snippet = body.substr(0, 2000);
    for (char& character : snippet) {
        if (character == '\n' || character == '\r') {
            character = ' ';
        }
    }
    log << when << " model=" << model << " stream=" << (streaming ? 1 : 0)
        << " status=" << status << " body=" << snippet << '\n';
}

/// Split "http://host:port/v1" into the host root and the path prefix httplib
/// wants separately.
bool SplitBaseUrl(const std::string& base_url, std::string* origin, std::string* prefix) {
    const std::string scheme = base_url.rfind("https://", 0) == 0 ? "https://" : "http://";
    const size_t start = base_url.find(scheme);
    if (start != 0) {
        return false;
    }
    const size_t slash = base_url.find('/', scheme.size());
    if (slash == std::string::npos) {
        *origin = base_url;
        *prefix = "";
    } else {
        *origin = base_url.substr(0, slash);
        *prefix = base_url.substr(slash);
    }
    return !origin->empty();
}

struct Runtime {
    httplib::Server server;
    std::thread thread;
    std::string origin;
    std::string prefix;
    std::string api_key;
    std::string model;
    std::string advertised;
    // The secret handed to the wrapped tool, and required back on every request.
    // Binding to 127.0.0.1 keeps the network out; this keeps other local
    // processes out.
    std::string local_token;
    bool verbose = false;
};

// The token the wrapped tool presents, read from either header Claude Code may
// send it in: Authorization: Bearer <t> (from ANTHROPIC_AUTH_TOKEN) or
// x-api-key: <t> (from ANTHROPIC_API_KEY). Both carry the same value.
std::string PresentedToken(const httplib::Request& request) {
    if (request.has_header("x-api-key")) {
        return request.get_header_value("x-api-key");
    }
    const std::string authorization = request.get_header_value("Authorization");
    constexpr const char* kBearer = "Bearer ";
    if (authorization.rfind(kBearer, 0) == 0) {
        return authorization.substr(std::string(kBearer).size());
    }
    return std::string();
}

std::unique_ptr<Runtime> g_runtime;

void ApplyAuth(httplib::Client& client, const std::string& api_key) {
    if (!api_key.empty()) {
        client.set_bearer_token_auth(api_key);
    }
}

void HandleNonStreaming(Runtime& runtime, const Json& request, httplib::Response& response) {
    httplib::Client client(runtime.origin);
    client.set_read_timeout(600, 0);
    ApplyAuth(client, runtime.api_key);

    const Json upstream = translate::RequestToOpenAI(request, runtime.model);
    const httplib::Result reply =
        client.Post(runtime.prefix + "/chat/completions", upstream.dump(), "application/json");
    if (!reply || reply->status < 200 || reply->status >= 300) {
        const int status = reply ? reply->status : 0;
        const std::string body = reply ? reply->body : std::string();
        LogUpstreamError(runtime.model, false, status, body);
        response.status = reply ? reply->status : 502;
        // A 429 from the hosted API carries a Retry-After the wrapped tool
        // should honor; httplib drops upstream headers unless we copy them.
        if (reply && reply->status == 429 && reply->has_header("Retry-After")) {
            response.set_header("Retry-After", reply->get_header_value("Retry-After"));
        }
        std::string type;
        std::string message;
        translate::UpstreamFailure(status, body, &type, &message);
        response.set_content(translate::ErrorBody(type, message), "application/json");
        return;
    }
    Json parsed;
    try {
        parsed = Json::parse(reply->body);
    } catch (const Json::exception& error) {
        LogUpstreamError(runtime.model, false, reply->status, reply->body);
        response.status = 502;
        response.set_content(translate::ErrorBody("api_error", error.what()), "application/json");
        return;
    }
    std::string failure_type;
    std::string failure;
    if (translate::PayloadError(parsed, &failure_type, &failure)) {
        LogUpstreamError(runtime.model, false, reply->status, reply->body);
        response.status = failure_type == "rate_limit_error" ? 429 : 502;
        // A rate-limit error can arrive as a 200 body rather than a 429 status;
        // forward the upstream Retry-After either way so the tool backs off.
        if (response.status == 429 && reply->has_header("Retry-After")) {
            response.set_header("Retry-After", reply->get_header_value("Retry-After"));
        }
        response.set_content(translate::ErrorBody(failure_type, failure), "application/json");
        return;
    }
    response.set_content(translate::ResponseToAnthropic(parsed, runtime.model).dump(),
                         "application/json");
}

// A streaming upstream call whose HTTP status is known before anything has been
// written downstream.
//
// The shim used to commit a 200 and only then make the call, because the chunked
// content provider runs after the request handler returns. An upstream 429 with a
// Retry-After therefore reached the editor as a 200 carrying an SSE error, with the
// delay gone (#83). Running the call on its own thread lets the handler wait for the
// upstream headers, which is the only thing it needs in order to answer honestly, and
// still start writing on the first token rather than buffering the whole reply.
struct StreamPump {
    std::mutex mutex;
    // Headers have landed, an event is queued, or the worker is finished.
    std::condition_variable ready;
    // The queue has fallen back under the cap.
    std::condition_variable drained;

    // Translated Anthropic SSE, in the order it is to be written.
    std::deque<std::string> events;
    std::size_t queued_bytes = 0;

    bool headers_known = false;
    bool finished = false;
    // The reader went away; the worker stops at its next chunk.
    bool cancelled = false;

    int status = 0;
    std::string retry_after;
    // Kept only for a non-2xx reply, where the body is the error rather than a
    // stream. Capped so a real stream of any size costs nothing here.
    std::string error_body;
};

constexpr std::size_t kErrorBodyCap = 8192;
// A slow editor must not let a fast endpoint grow the queue without bound.
constexpr std::size_t kQueuedBytesCap = 4 * 1024 * 1024;

void PumpEvents(const std::shared_ptr<StreamPump>& pump, std::string events) {
    if (events.empty()) {
        return;
    }
    std::unique_lock<std::mutex> lock(pump->mutex);
    pump->drained.wait(lock, [&] { return pump->cancelled || pump->queued_bytes < kQueuedBytesCap; });
    if (pump->cancelled) {
        return;
    }
    pump->queued_bytes += events.size();
    pump->events.push_back(std::move(events));
    pump->ready.notify_all();
}

// Runs on the worker thread. Everything the translator touches — the stream state
// and the half-frame buffer — lives here, so the mapping is still single-threaded.
void RunUpstream(const std::shared_ptr<StreamPump>& pump, const std::string& origin,
                 const std::string& path, const std::string& api_key, const std::string& model,
                 const std::string& body) {
    httplib::Client client(origin);
    client.set_read_timeout(600, 0);
    ApplyAuth(client, api_key);

    translate::StreamState state;
    state.model = model;
    std::string pending;
    bool upstream_ok = false;

    httplib::Request request;
    request.method = "POST";
    request.path = path;
    request.body = body;
    request.set_header("Content-Type", "application/json");

    request.response_handler = [&](const httplib::Response& reply) {
        {
            std::lock_guard<std::mutex> lock(pump->mutex);
            pump->status = reply.status;
            if (reply.has_header("Retry-After")) {
                pump->retry_after = reply.get_header_value("Retry-After");
            }
            pump->headers_known = true;
            pump->ready.notify_all();
        }
        upstream_ok = reply.status >= 200 && reply.status < 300;
        return true;
    };

    request.content_receiver = [&](const char* data, std::size_t length, std::size_t, std::size_t) {
        if (!upstream_ok) {
            std::lock_guard<std::mutex> lock(pump->mutex);
            if (pump->error_body.size() < kErrorBodyCap) {
                pump->error_body.append(data,
                                        std::min(length, kErrorBodyCap - pump->error_body.size()));
            }
            return true;
        }
        pending.append(data, length);
        // SSE frames are separated by a blank line, and a chunk can split one in
        // half, so only whole frames are consumed.
        std::size_t split = 0;
        while ((split = pending.find("\n\n")) != std::string::npos) {
            const std::string frame = pending.substr(0, split);
            pending.erase(0, split + 2);
            const std::size_t field = frame.find("data:");
            if (field == std::string::npos) {
                continue;
            }
            std::string payload = frame.substr(field + 5);
            while (!payload.empty() && (payload.front() == ' ' || payload.front() == '\r')) {
                payload.erase(payload.begin());
            }
            if (payload == "[DONE]") {
                continue;
            }
            Json chunk;
            try {
                chunk = Json::parse(payload);
            } catch (const Json::exception&) {
                continue;
            }
            std::string events;
            try {
                events = translate::StreamChunkToAnthropic(chunk, &state);
            } catch (const std::exception&) {
                // A chunk in a shape the mapping did not expect is a chunk to
                // skip, not a reason to kill the run.
                continue;
            }
            PumpEvents(pump, std::move(events));
        }
        std::lock_guard<std::mutex> lock(pump->mutex);
        return !pump->cancelled;
    };

    const httplib::Result reply = client.send(request);
    const bool transport_ok = static_cast<bool>(reply);

    {
        std::lock_guard<std::mutex> lock(pump->mutex);
        if (!pump->headers_known) {
            // The call failed before any reply: no status was ever seen.
            pump->status = reply ? reply->status : 0;
            pump->headers_known = true;
        }
    }

    if (upstream_ok) {
        if (transport_ok) {
            try {
                std::string closing = translate::StreamCloseToAnthropic(&state);
                PumpEvents(pump, std::move(closing));
            } catch (const std::exception&) {
                // Nothing useful left to say; ending the stream cleanly beats
                // aborting the process holding the reader's editor open.
            }
        } else {
            // The status is already spent on a 200, so a failure this late can
            // only be a typed SSE error. It must not read as a normal stop.
            LogUpstreamError(model, true, 0, "upstream stream ended early");
            std::string type;
            std::string message;
            translate::UpstreamFailure(0, std::string(), &type, &message);
            PumpEvents(pump, "event: error\ndata: " + translate::ErrorBody(type, message) + "\n\n");
        }
    }

    std::lock_guard<std::mutex> lock(pump->mutex);
    pump->finished = true;
    pump->ready.notify_all();
}

// Stops the worker and joins it however the provider ends: drained, refused by the
// reader, or dropped by httplib without another call.
class PumpJoiner {
   public:
    PumpJoiner(std::shared_ptr<StreamPump> pump, std::thread worker)
        : pump_(std::move(pump)), worker_(std::move(worker)) {}
    ~PumpJoiner() {
        {
            std::lock_guard<std::mutex> lock(pump_->mutex);
            pump_->cancelled = true;
        }
        pump_->drained.notify_all();
        if (worker_.joinable()) {
            worker_.join();
        }
    }

    PumpJoiner(const PumpJoiner&) = delete;
    PumpJoiner& operator=(const PumpJoiner&) = delete;

   private:
    std::shared_ptr<StreamPump> pump_;
    std::thread worker_;
};

void HandleStreaming(Runtime& runtime, const Json& request, httplib::Response& response) {
    const std::string body = translate::RequestToOpenAI(request, runtime.model).dump();
    const std::string path = runtime.prefix + "/chat/completions";
    const std::string model = runtime.model;

    auto pump = std::make_shared<StreamPump>();
    std::thread worker([pump, origin = runtime.origin, path, api_key = runtime.api_key, model,
                        body] { RunUpstream(pump, origin, path, api_key, model, body); });
    auto joiner = std::make_shared<PumpJoiner>(pump, std::move(worker));

    int status = 0;
    {
        std::unique_lock<std::mutex> lock(pump->mutex);
        pump->ready.wait(lock, [&] { return pump->headers_known || pump->finished; });
        status = pump->status;
    }

    if (status < 200 || status >= 300) {
        // Nothing has been written downstream yet, so the refusal can be answered
        // as itself: the upstream status, and the delay it asked for.
        std::string error_body;
        std::string retry_after;
        {
            std::unique_lock<std::mutex> lock(pump->mutex);
            pump->ready.wait(lock, [&] { return pump->finished; });
            error_body = pump->error_body;
            retry_after = pump->retry_after;
        }
        LogUpstreamError(model, true, status, error_body);
        response.status = status != 0 ? status : 502;
        if (!retry_after.empty()) {
            response.set_header("Retry-After", retry_after);
        }
        std::string type;
        std::string message;
        translate::UpstreamFailure(status, error_body, &type, &message);
        response.set_content(translate::ErrorBody(type, message), "application/json");
        return;
    }

    response.set_chunked_content_provider(
        "text/event-stream", [pump, joiner](std::size_t /*offset*/, httplib::DataSink& sink) {
            std::string events;
            {
                std::unique_lock<std::mutex> lock(pump->mutex);
                pump->ready.wait(lock, [&] { return !pump->events.empty() || pump->finished; });
                if (pump->events.empty()) {
                    sink.done();
                    return true;
                }
                events = std::move(pump->events.front());
                pump->events.pop_front();
                pump->queued_bytes -= events.size();
                pump->drained.notify_all();
            }
            if (!sink.write(events.data(), events.size())) {
                return false;
            }
            return true;
        });
}

}  // namespace

bool Start(const harness::Endpoint& upstream, const std::string& model, Shim* shim,
           bool verbose, const std::string& advertised) {
    if (shim == nullptr) {
        return false;
    }
    Stop(shim);

    auto runtime = std::make_unique<Runtime>();
    if (!SplitBaseUrl(upstream.base_url, &runtime->origin, &runtime->prefix)) {
        out::error_line("could not read the model endpoint: " + upstream.base_url);
        return false;
    }
    runtime->api_key = upstream.api_key;
    runtime->model = model;
    runtime->advertised = advertised.empty() ? model : advertised;
    runtime->local_token = wally::net::GenerateLoopbackToken();
    runtime->verbose = verbose;

    Runtime* raw = runtime.get();
    raw->server.Post("/v1/messages", [raw](const httplib::Request& request,
                                           httplib::Response& response) {
        if (!wally::net::ConstantTimeEquals(PresentedToken(request), raw->local_token)) {
            response.status = 401;
            response.set_content(
                translate::ErrorBody("authentication_error",
                                     "this local endpoint only serves the tool wally launched"),
                "application/json");
            return;
        }
        if (raw->verbose) {
            out::status_line("anthropic: POST /v1/messages, " +
                        std::to_string(request.body.size()) + " bytes");
        }
        Json parsed;
        try {
            parsed = Json::parse(request.body);
        } catch (const Json::exception& error) {
            response.status = 400;
            response.set_content(translate::ErrorBody("invalid_request_error", error.what()),
                                 "application/json");
            return;
        }
        try {
            if (parsed.value("stream", false)) {
                HandleStreaming(*raw, parsed, response);
            } else {
                HandleNonStreaming(*raw, parsed, response);
            }
        } catch (const std::exception& error) {
            // httplib does not catch, and an exception leaving here reaches
            // std::terminate: the editor's model call would abort wally.
            if (raw->verbose) {
                out::status_line(std::string("anthropic: request failed: ") + error.what());
            }
            response.status = 500;
            response.set_content(translate::ErrorBody("api_error", error.what()),
                                 "application/json");
        }
    });

    // Discovery, in Anthropic's shape rather than OpenAI's.
    //
    // Claude Desktop probes this before it will use a gateway at all, and an
    // OpenAI-shaped list fails it with "Gateway returned no usable models":
    // the entries need `display_name` and `created_at`, and the envelope needs
    // the paging fields, or nothing in the list counts as usable.
    raw->server.Get("/v1/models", [raw](const httplib::Request&, httplib::Response& response) {
        if (raw->verbose) {
            out::status_line("anthropic: GET /v1/models -> " + raw->advertised + " (serving " +
                        raw->model + ")");
        }
        // The shape claude.com/docs/third-party/claude-desktop documents for a
        // gateway, which is OpenAI's list envelope rather than Anthropic's.
        // Guessing the Anthropic shape here is what produced "Gateway returned
        // no usable models".
        const Json entry{{"id", raw->advertised}, {"object", "model"}};
        response.set_content(
            Json{{"object", "list"}, {"data", Json::array({entry})}}.dump(),
            "application/json");
    });

    // Claude Code probes this before it sends anything and treats a failure as
    // an endpoint that is not there. Answering it is what makes the translator
    // look like a gateway rather than a wrong address.
    raw->server.Get("/api/hello", [](const httplib::Request&, httplib::Response& response) {
        response.set_content(Json{{"ok", true}}.dump(), "application/json");
    });
    // httplib has no HEAD route, and Claude Code probes with HEAD, so it is
    // answered ahead of routing rather than left to fall through to the 404.
    raw->server.set_pre_routing_handler(
        [](const httplib::Request& request, httplib::Response& response) {
            if (request.method == "HEAD" && request.path == "/api/hello") {
                response.status = 200;
                return httplib::Server::HandlerResponse::Handled;
            }
            return httplib::Server::HandlerResponse::Unhandled;
        });

    // A route we do not translate should say so, not 404 into a silence the
    // reader has to guess at.
    raw->server.set_error_handler([raw](const httplib::Request& request,
                                        httplib::Response& response) {
        if (raw->verbose) {
            out::status_line("anthropic: " + request.method + " " + request.path + " -> " +
                        std::to_string(response.status));
        }
        if (response.body.empty()) {
            response.set_content(
                translate::ErrorBody("not_found_error",
                                     request.method + " " + request.path +
                                         " is not something wally translates"),
                "application/json");
        }
    });

    const int port = raw->server.bind_to_any_port("127.0.0.1");
    if (port <= 0) {
        out::error_line("could not open a port for the Anthropic translator");
        return false;
    }

    g_runtime = std::move(runtime);
    Runtime* started = g_runtime.get();
    started->thread = std::thread([started] { started->server.listen_after_bind(); });

    shim->base_url = "http://127.0.0.1:" + std::to_string(port);
    // A per-session secret, never the upstream key: handing the tool a real
    // console token would put it in that process's environment where it does
    // not belong, and a fixed value would let any local process spend the
    // signed-in user's credit. The server checks this back on every request.
    shim->auth_token = started->local_token;
    shim->running = true;
    return true;
}

void Stop(Shim* shim) {
    if (g_runtime) {
        g_runtime->server.stop();
        if (g_runtime->thread.joinable()) {
            g_runtime->thread.join();
        }
        g_runtime.reset();
    }
    if (shim != nullptr) {
        shim->running = false;
        shim->base_url.clear();
        shim->auth_token.clear();
    }
}

}  // namespace wally::anthropic
