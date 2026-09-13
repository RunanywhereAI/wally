#include "anthropic/messages.h"

#include <httplib.h>

#include <algorithm>
#include <atomic>
#include <condition_variable>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <memory>
#include <nlohmann/json.hpp>
#include <string>
#include <system_error>
#include <thread>

#include "anthropic/translate.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "net/loopback_auth.h"
#include "net/upstream_pool.h"

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
    log << when << " model=" << model << " stream=" << (streaming ? 1 : 0) << " status=" << status
        << " body=" << snippet << '\n';
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
    // Upstream connections, kept open across requests. Shared, not owned:
    // a streaming sink can still be running its request after Stop(), and
    // the lease it holds keeps the pool alive until it is done.
    std::shared_ptr<wally::net::UpstreamPool> pool;
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

/// Sends `body` upstream on a pooled connection, once more on a fresh one if
/// the first went out on a stale keep-alive (see RetryOnFreshConnection).
/// `received_any` reports whether any response bytes reached `receiver`, which
/// is what forbids the retry once output has started.
httplib::Result
PostUpstream(Runtime& runtime, const std::string& path, const std::string& body,
             const httplib::ContentReceiver& receiver, bool* received_any,
             const httplib::ResponseHandler& headers = nullptr,
             const std::function<void(httplib::Client*)>& client_changed = nullptr) {
    for (int attempt = 0; attempt < 2; ++attempt) {
        wally::net::UpstreamLease lease = runtime.pool->acquire(runtime.api_key);
        if (runtime.verbose) {
            out::status_line(std::string("anthropic: upstream connection ") +
                             (lease.reused() ? "reused" : "fresh"));
        }
        *received_any = false;
        // Clear the published pointer before the lease dies, including when
        // the HTTP library throws, so cancellation never sees a freed client.
        struct Registration {
            const std::function<void(httplib::Client*)>& changed;
            ~Registration() {
                if (changed)
                    changed(nullptr);
            }
        } registration{client_changed};
        if (client_changed)
            client_changed(&lease.client());
        bool received_headers = false;
        httplib::Request outbound;
        outbound.method = "POST";
        outbound.path = path;
        outbound.body = body;
        outbound.set_header("Content-Type", "application/json");
        outbound.response_handler = [&](const httplib::Response& response) {
            received_headers = true;
            return !headers || headers(response);
        };
        if (receiver) {
            outbound.content_receiver = [&](const char* data, size_t length, uint64_t, uint64_t) {
                *received_any = true;
                return receiver(data, length);
            };
        }
        httplib::Result reply = lease.client().send(outbound);
        if (reply) {
            // A complete reply, whatever its status, leaves the connection
            // clean; the lease goes back to the pool when it is destroyed.
            return reply;
        }
        // No reply: the socket is in no state to reuse.
        lease.discard();
        if (attempt == 0 && wally::net::RetryOnFreshConnection(reply.error(), received_headers,
                                                               *received_any, lease.reused())) {
            if (runtime.verbose) {
                out::status_line(
                    "anthropic: upstream connection was stale; retrying once on a "
                    "fresh one");
            }
            continue;
        }
        return reply;
    }
    return httplib::Result{nullptr, httplib::Error::Unknown};
}

void HandleNonStreaming(Runtime& runtime, const Json& request, httplib::Response& response) {
    const Json upstream = translate::RequestToOpenAI(request, runtime.model);
    bool received_any = false;
    const httplib::Result reply = PostUpstream(runtime, runtime.prefix + "/chat/completions",
                                               upstream.dump(), nullptr, &received_any);
    if (!reply || reply->status < 200 || reply->status >= 300) {
        const int status = reply ? reply->status : 0;
        const std::string body = reply ? reply->body : std::string();
        LogUpstreamError(runtime.model, false, status, body);
        response.status = reply ? reply->status : 502;
        // A 429 from the hosted API carries a Retry-After the wrapped tool
        // should honor; httplib drops upstream headers unless we copy them.
        if (reply && reply->has_header("Retry-After")) {
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

// Read headers before the server commits its downstream response. A single
// chunk slot bounds read-ahead and preserves backpressure on long streams.
struct StreamingReply {
    std::mutex mutex;
    std::condition_variable changed;
    std::thread worker;
    httplib::Client* client = nullptr;
    bool headers_ready = false;
    bool finished = false;
    bool stopped = false;
    bool successful = false;
    int status = 0;
    std::string retry_after;
    std::string chunk;
    std::string error_body;

    ~StreamingReply() {
        {
            std::lock_guard<std::mutex> lock(mutex);
            stopped = true;
            if (client)
                client->stop();
            changed.notify_all();
        }
        if (worker.joinable())
            worker.join();
    }

    bool Read(std::string* next) {
        std::unique_lock<std::mutex> lock(mutex);
        changed.wait(lock, [&] { return !chunk.empty() || finished; });
        if (chunk.empty())
            return false;
        *next = std::move(chunk);
        chunk.clear();
        changed.notify_all();
        return true;
    }
};

void HandleStreaming(Runtime& runtime, const Json& request, httplib::Response& response) {
    // The upstream body is built here rather than in the sink: the sink runs
    // after this function returns, and everything it touches has to outlive it.
    auto upstream =
        std::make_shared<std::string>(translate::RequestToOpenAI(request, runtime.model).dump());
    auto path = std::make_shared<std::string>(runtime.prefix + "/chat/completions");
    auto model = std::make_shared<std::string>(runtime.model);
    // The Runtime outlives every sink: Stop() stops the server and joins its
    // thread before the Runtime is destroyed, and the pool is shared besides.
    Runtime* owner = &runtime;

    auto stream = std::make_shared<StreamingReply>();
    StreamingReply* transfer = stream.get();
    transfer->worker = std::thread([transfer, owner, upstream, path] {
        bool received_any = false;
        try {
            auto reply = PostUpstream(
                *owner, *path, *upstream,
                [&](const char* data, size_t length) {
                    std::unique_lock<std::mutex> lock(transfer->mutex);
                    if (transfer->status < 200 || transfer->status >= 300) {
                        constexpr size_t cap = 8192;
                        transfer->error_body.append(
                            data, std::min(length, cap - transfer->error_body.size()));
                        return !transfer->stopped;
                    }
                    transfer->changed.wait(
                        lock, [&] { return transfer->chunk.empty() || transfer->stopped; });
                    if (transfer->stopped)
                        return false;
                    transfer->chunk.assign(data, length);
                    transfer->changed.notify_all();
                    return true;
                },
                &received_any,
                [&](const httplib::Response& headers) {
                    std::lock_guard<std::mutex> lock(transfer->mutex);
                    transfer->status = headers.status;
                    transfer->retry_after = headers.get_header_value("Retry-After");
                    transfer->headers_ready = true;
                    transfer->changed.notify_all();
                    return !transfer->stopped;
                },
                [&](httplib::Client* client) {
                    std::lock_guard<std::mutex> lock(transfer->mutex);
                    transfer->client = client;
                    if (client && transfer->stopped)
                        client->stop();
                });
            std::lock_guard<std::mutex> lock(transfer->mutex);
            transfer->successful = reply && reply->status >= 200 && reply->status < 300;
            transfer->finished = true;
            transfer->changed.notify_all();
        } catch (const std::exception&) {
            std::lock_guard<std::mutex> lock(transfer->mutex);
            transfer->finished = true;
            transfer->changed.notify_all();
        }
    });
    {
        std::unique_lock<std::mutex> lock(stream->mutex);
        stream->changed.wait(lock, [&] { return stream->headers_ready || stream->finished; });
        if (stream->status < 200 || stream->status >= 300) {
            stream->changed.wait(lock, [&] { return stream->finished; });
            response.status = stream->status ? stream->status : 502;
            if (!stream->retry_after.empty())
                response.set_header("Retry-After", stream->retry_after);
            std::string type, message;
            translate::UpstreamFailure(stream->status, stream->error_body, &type, &message);
            LogUpstreamError(*model, true, stream->status, stream->error_body);
            response.set_content(translate::ErrorBody(type, message), "application/json");
            return;
        }
    }

    response.set_chunked_content_provider(
        "text/event-stream", [stream, model](size_t /*offset*/, httplib::DataSink& sink) {
            translate::StreamState state;
            state.model = *model;
            std::string pending;
            std::string payload;
            bool has_data = false;
            bool saw_done = false;
            // Retain a bounded prefix for diagnosing a transport failure
            // after successful response headers have already been forwarded.
            std::string error_body;
            constexpr size_t kErrorBodyCap = 8192;

            auto receive = [&](const char* data, size_t length) {
                if (error_body.size() < kErrorBodyCap) {
                    error_body.append(data, std::min(length, kErrorBodyCap - error_body.size()));
                }
                pending.append(data, length);
                // Consume complete lines across arbitrary transport chunks.
                // CRLF and multi-line data fields are valid SSE too.
                size_t split = 0;
                while ((split = pending.find('\n')) != std::string::npos) {
                    std::string line = pending.substr(0, split);
                    pending.erase(0, split + 1);
                    if (!line.empty() && line.back() == '\r') line.pop_back();
                    if (!line.empty()) {
                        if (line == "data" || line.rfind("data:", 0) == 0) {
                            std::string value = line == "data" ? "" : line.substr(5);
                            if (!value.empty() && value.front() == ' ') value.erase(0, 1);
                            if (has_data) payload += '\n';
                            payload += value;
                            has_data = true;
                        }
                        continue;
                    }
                    if (!has_data) continue;  // comments/keepalives
                    has_data = false;
                    std::string events;
                    if (saw_done) {
                        events = translate::StreamErrorToAnthropic(
                            &state, "the model endpoint sent data after [DONE]");
                    } else if (payload == "[DONE]") {
                        saw_done = true;
                    } else {
                        try {
                            events = translate::StreamChunkToAnthropic(Json::parse(payload), &state);
                        } catch (const std::exception&) {
                            events = translate::StreamErrorToAnthropic(
                                &state, "the model endpoint sent a malformed stream frame");
                        }
                    }
                    payload.clear();
                    if (!events.empty() && !sink.write(events.data(), events.size())) {
                        return false;
                    }
                }
                return true;
            };
            std::string bytes;
            while (stream->Read(&bytes)) {
                if (!receive(bytes.data(), bytes.size()))
                    return false;
            }

            if (!stream->successful) {
                const int status = 0;
                LogUpstreamError(*model, true, status, error_body);
                std::string type;
                std::string message;
                translate::UpstreamFailure(status, error_body, &type, &message);
                const std::string body = translate::StreamErrorToAnthropic(&state, message);
                if (!body.empty()) sink.write(body.data(), body.size());
                sink.done();
                return false;
            }
            try {
                // Transport EOF is not inference completion. Our OpenAI
                // upstream must send a finish reason followed by [DONE].
                const std::string closing = (!saw_done || has_data || !pending.empty())
                    ? translate::StreamErrorToAnthropic(
                          &state, "the model endpoint ended an incomplete stream before [DONE]")
                    : translate::StreamCloseToAnthropic(&state);
                if (!closing.empty()) {
                    sink.write(closing.data(), closing.size());
                }
            } catch (const std::exception&) {
                const std::string error = translate::StreamErrorToAnthropic(
                    &state, "the model endpoint returned an invalid stream completion");
                if (!error.empty()) sink.write(error.data(), error.size());
            }
            sink.done();
            return true;
        });
}

}  // namespace

bool Start(const harness::Endpoint& upstream, const std::string& model, Shim* shim, bool verbose,
           const std::string& advertised) {
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
    wally::net::UpstreamOptions pool_options;
    pool_options.origin = runtime->origin;
    runtime->pool = std::make_shared<wally::net::UpstreamPool>(pool_options);

    Runtime* raw = runtime.get();
    raw->server.Post(
        "/v1/messages", [raw](const httplib::Request& request, httplib::Response& response) {
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
        response.set_content(Json{{"object", "list"}, {"data", Json::array({entry})}}.dump(),
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
    raw->server.set_error_handler(
        [raw](const httplib::Request& request, httplib::Response& response) {
            if (raw->verbose) {
                out::status_line("anthropic: " + request.method + " " + request.path + " -> " +
                                 std::to_string(response.status));
            }
            if (response.body.empty()) {
                response.set_content(translate::ErrorBody("not_found_error",
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
