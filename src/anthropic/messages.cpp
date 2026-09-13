#include "anthropic/messages.h"

#include <algorithm>
#include <atomic>
#include <ctime>
#include <filesystem>
#include <functional>
#include <fstream>
#include <memory>
#include <string>
#include <system_error>
#include <thread>

#include <httplib.h>
#include <nlohmann/json.hpp>

#include "anthropic/translate.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "account/cancel_worker.h"
#include "net/loopback_auth.h"
#include "net/upstream_call.h"
#include "net/upstream_pool.h"

namespace wally::anthropic {
namespace {

using Json = nlohmann::json;

/// One timestamped line into shim.log; the error logger below and the
/// abandon path share it. Never the editor's terminal.
void ShimLog(const std::string& line) {
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
    log << when << ' ' << line << '\n';
}

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
    // Upstream connections, kept open across requests. Shared, not owned:
    // a streaming sink can still be running its request after Stop(), and
    // the lease it holds keeps the pool alive until it is done.
    std::shared_ptr<wally::net::UpstreamPool> pool;
    // Where a request the editor abandoned is cancelled by name (#81): the
    // session's control plane, or nothing for a local server. `stopping`
    // tells an in-flight watch to stop waiting for an id; the worker sends
    // the cancels off the request path, and Stop() drains it.
    std::string console_url;
    std::atomic<bool> stopping{false};
    std::unique_ptr<wally::account::CancelWorker> cancels;
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

/// What the abandon path does once the id is known (or known to be
/// unknowable): a shim.log line the person can find, and the cancel itself
/// through the worker -- never on this thread, which is the upstream watch or
/// the response handler.
void OnAbandoned(Runtime& runtime, bool streaming, const std::string& request_id, int status,
                 bool during_prefill) {
    std::string line = "abandoned during=" + std::string(during_prefill ? "prefill" : "stream") +
                       " id=" + (request_id.empty() ? std::string("unknown") : request_id) +
                       " status=" + std::to_string(status) + " stream=" + (streaming ? "1" : "0");
    if (request_id.empty()) {
        ShimLog(line + " cancel=none(no-id)");
        return;
    }
    if (!runtime.cancels) {
        // No worker: a local server, which has no console to tell.
        ShimLog(line + " cancel=skipped(local)");
        return;
    }
    ShimLog(line + " cancel=queued");
    runtime.cancels->Enqueue(request_id);
}

/// Sends `body` upstream on a pooled connection, watching the editor the
/// whole time (#81), and once more on a fresh connection if the first went out
/// on a stale keep-alive (see RetryOnFreshConnection) -- never after the
/// editor left: the stop that ended an abandoned call looks exactly like a
/// stale connection to that rule, and re-sending the prompt for a reader that
/// is gone is the waste this exists to end.
wally::net::WatchedResult PostUpstream(Runtime& runtime, bool streaming, const std::string& path,
                                       const std::string& body,
                                       const httplib::ContentReceiver& receiver,
                                       std::function<bool()> reader_gone) {
    for (int attempt = 0; attempt < 2; ++attempt) {
        wally::net::UpstreamLease lease = runtime.pool->acquire(runtime.api_key);
        if (runtime.verbose) {
            out::status_line(std::string("anthropic: upstream connection ") +
                             (lease.reused() ? "reused" : "fresh"));
        }
        wally::net::WatchedCall call;
        call.path = path;
        call.body = body;
        call.receiver = receiver;
        call.reader_gone = reader_gone;
        call.stopping = [&runtime] { return runtime.stopping.load(); };
        call.on_abandoned = [&runtime, streaming](const std::string& id, int status,
                                                  bool during_prefill) {
            OnAbandoned(runtime, streaming, id, status, during_prefill);
        };
        wally::net::WatchedResult result = wally::net::PostWatched(lease, call);
        if (result.reply) {
            // A complete reply, whatever its status, leaves the connection
            // clean; the lease goes back to the pool when it is destroyed.
            return result;
        }
        // No reply: the socket is in no state to reuse.
        lease.discard();
        if (result.abandoned) {
            if (runtime.verbose) {
                out::status_line("anthropic: the editor left; upstream request dropped");
            }
            return result;
        }
        if (attempt == 0 && wally::net::RetryOnFreshConnection(result.reply.error(), false,
                                                                result.received_any, lease.reused())) {
            if (runtime.verbose) {
                out::status_line("anthropic: upstream connection was stale; retrying once on a "
                                 "fresh one");
            }
            continue;
        }
        return result;
    }
    return wally::net::WatchedResult{};
}

void HandleNonStreaming(Runtime& runtime, const httplib::Request& editor, const Json& request,
                        httplib::Response& response) {
    const Json upstream = translate::RequestToOpenAI(request, runtime.model);
    const wally::net::WatchedResult result =
        PostUpstream(runtime, false, runtime.prefix + "/chat/completions", upstream.dump(),
                     nullptr, editor.is_connection_closed);
    const httplib::Result& reply = result.reply;
    if (result.abandoned) {
        // Nobody is reading; whatever is written here goes nowhere.
        response.status = 499;
        return;
    }
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

void HandleStreaming(Runtime& runtime, const httplib::Request& editor, const Json& request,
                     httplib::Response& response) {
    // The upstream body is built here rather than in the sink: the sink runs
    // after this function returns, and everything it touches has to outlive it.
    auto upstream = std::make_shared<std::string>(
        translate::RequestToOpenAI(request, runtime.model).dump());
    auto path = std::make_shared<std::string>(runtime.prefix + "/chat/completions");
    auto model = std::make_shared<std::string>(runtime.model);
    // The Runtime outlives every sink: Stop() stops the server and joins its
    // thread before the Runtime is destroyed, and the pool is shared besides.
    Runtime* owner = &runtime;
    // The editor's socket, peeked by the upstream watch: a copy of the
    // request's own closure, which captures the fd by value.
    std::function<bool()> reader_gone = editor.is_connection_closed;

    response.set_chunked_content_provider(
        "text/event-stream",
        [upstream, path, model, owner, reader_gone](size_t /*offset*/, httplib::DataSink& sink) {
            translate::StreamState state;
            state.model = *model;
            std::string pending;
            // The upstream status is only known once Post returns, so the start
            // of the body is kept regardless. On a non-2xx reply that is the
            // error body, which would otherwise be fed to the SSE frame parser
            // and silently dropped; capped so a real (2xx) stream of any size
            // costs only these few KB.
            std::string error_body;
            constexpr size_t kErrorBodyCap = 8192;

            const wally::net::WatchedResult result = PostUpstream(
                *owner, true, *path, *upstream,
                [&](const char* data, size_t length) {
                    if (error_body.size() < kErrorBodyCap) {
                        error_body.append(data,
                                          std::min(length, kErrorBodyCap - error_body.size()));
                    }
                    pending.append(data, length);
                    // SSE frames are separated by a blank line, and a chunk can
                    // split one in half, so only whole frames are consumed.
                    size_t split = 0;
                    while ((split = pending.find("\n\n")) != std::string::npos) {
                        const std::string frame = pending.substr(0, split);
                        pending.erase(0, split + 2);
                        const size_t field = frame.find("data:");
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
                            // A chunk in a shape the mapping did not expect is
                            // a chunk to skip, not a reason to kill the run.
                            continue;
                        }
                        if (!events.empty() && !sink.write(events.data(), events.size())) {
                            return false;
                        }
                    }
                    return true;
                },
                reader_gone);
            const httplib::Result& reply = result.reply;

            if (result.abandoned) {
                // The editor is gone; the cancel is on its way (or was named
                // as impossible). Nothing written here reaches anyone.
                sink.done();
                return false;
            }
            if (!reply || reply->status < 200 || reply->status >= 300) {
                const int status = reply ? reply->status : 0;
                LogUpstreamError(*model, true, status,
                                 reply ? error_body
                                       : std::string("transport error: ") +
                                             httplib::to_string(reply.error()) + " " + error_body);
                std::string type;
                std::string message;
                translate::UpstreamFailure(status, error_body, &type, &message);
                const std::string body =
                    "event: error\ndata: " + translate::ErrorBody(type, message) + "\n\n";
                sink.write(body.data(), body.size());
                sink.done();
                return false;
            }
            try {
                const std::string closing = translate::StreamCloseToAnthropic(&state);
                if (!closing.empty()) {
                    sink.write(closing.data(), closing.size());
                }
            } catch (const std::exception&) {
                // Nothing useful left to say; ending the stream cleanly beats
                // aborting the process holding the reader's editor open.
            }
            sink.done();
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
    runtime->console_url = upstream.console_url;
    runtime->model = model;
    runtime->advertised = advertised.empty() ? model : advertised;
    runtime->local_token = wally::net::GenerateLoopbackToken();
    runtime->verbose = verbose;
    wally::net::UpstreamOptions pool_options;
    pool_options.origin = runtime->origin;
    runtime->pool = std::make_shared<wally::net::UpstreamPool>(pool_options);
    if (!runtime->console_url.empty() && !runtime->api_key.empty()) {
        // Three seconds per cancel: fire-and-forget, and the bound on how
        // long an exiting wrapper waits for the last one to go out.
        const std::string bearer = runtime->api_key;  // fixed for the session
        runtime->cancels = std::make_unique<wally::account::CancelWorker>(
            runtime->console_url, [bearer] { return bearer; }, 3000,
            [](const std::string& id, wally::account::CancelOutcome outcome,
               const std::string& error) {
                const char* word = outcome == wally::account::CancelOutcome::Cancelled ? "202"
                                   : outcome == wally::account::CancelOutcome::NotFound ? "404"
                                                                                       : "failed";
                ShimLog("cancel id=" + id + " result=" + word +
                        (error.empty() ? std::string() : " error=" + error));
            });
    }

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
                HandleStreaming(*raw, request, parsed, response);
            } else {
                HandleNonStreaming(*raw, request, parsed, response);
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
        // Order matters. `stopping` first, so an upstream watch still waiting
        // for an id (the editor left during prefill) gives up on its next
        // poll instead of holding the server thread until the first token;
        // then the server, which joins every handler; then the cancel queue,
        // so the last abandon's cancel goes out before the process does --
        // bounded by 3 s per queued cancel, typically one.
        g_runtime->stopping.store(true);
        g_runtime->server.stop();
        if (g_runtime->thread.joinable()) {
            g_runtime->thread.join();
        }
        if (g_runtime->cancels) {
            if (g_runtime->cancels->pending() > 0) {
                out::status_line("telling the model endpoint to stop the abandoned request");
            }
            g_runtime->cancels->Stop();
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
