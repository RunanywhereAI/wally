#include "anthropic/messages.h"

#include <httplib.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <ctime>
#include <filesystem>
#include <fstream>
#include <functional>
#include <memory>
#include <mutex>
#include <nlohmann/json.hpp>
#include <string>
#include <string_view>
#include <system_error>
#include <thread>
#include <vector>

#include "account/cancel_worker.h"
#include "account/model_cache.h"
#include "anthropic/translate.h"
#include "config/cli_paths.h"
#include "io/output.h"
#include "net/loopback_auth.h"
#include "net/upstream_call.h"
#include "net/upstream_pool.h"

namespace wally::anthropic {
namespace {

using Json = nlohmann::json;

/// One timestamped line into shim.log; the error logger below and the abandon
/// path share it. Never the editor's terminal, which the wrapped tool owns.
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
    // The real model ids the console advertises (never the advertised alias): a
    // request naming one of these is forwarded as-is, which lets Claude Code's
    // family-slot picker route each slot to its own model.
    std::vector<std::string> catalog;
    // (Anthropic family name -> real id) for Claude Desktop, whose picker is
    // family-based: a request naming a family is routed to the mapped id, and the
    // discovery endpoint advertises the family names.
    ModelAliases aliases;
    // The secret handed to the wrapped tool, and required back on every request.
    // Binding to 127.0.0.1 keeps the network out; this keeps other local
    // processes out.
    std::string local_token;
    bool verbose = false;
    // Upstream connections, kept open across requests (#80). Shared, not owned:
    // a streaming worker can still be running its request after Stop(), and the
    // lease it holds keeps the pool alive until it is done.
    std::shared_ptr<wally::net::UpstreamPool> pool;
    // Where a request the editor abandoned is cancelled by name (#81): the
    // session's control plane, or nothing for a local server. `stopping` tells
    // an in-flight watch to stop waiting for an id; the worker sends the cancels
    // off the request path, and Stop() drains it.
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

/// Sends `body` upstream on a pooled connection (#80), watching the editor the
/// whole time (#81), and once more on a fresh connection if the first went out
/// on a stale keep-alive (see RetryOnFreshConnection) -- never after the editor
/// left: the stop that ended an abandoned call looks exactly like a stale
/// connection to that rule, and re-sending the prompt for a reader that is gone
/// is the waste this exists to end. `on_headers`, when set, receives the
/// upstream response headers before any body byte, so a streaming caller can
/// preserve a pre-stream failure status instead of a blind 200 (#83).
wally::net::WatchedResult PostUpstream(Runtime& runtime, bool streaming, const std::string& path,
                                       const std::string& body,
                                       const httplib::ContentReceiver& receiver,
                                       std::function<bool()> reader_gone,
                                       std::function<void(const httplib::Response&)> on_headers = {}) {
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
        call.on_headers = on_headers;
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

/// The model a request runs against: the one the client asked for when the
/// console advertises it, otherwise the launched default. Claude Code maps its
/// Anthropic-family picker onto catalog models, so a request can name any of
/// them; honouring it lets each picker slot reach its own model instead of
/// collapsing onto one.
std::string EffectiveModel(const Runtime& runtime, const Json& request) {
    if (request.is_object()) {
        const auto found = request.find("model");
        if (found != request.end() && found->is_string()) {
            const std::string requested = found->get<std::string>();
            if (std::find(runtime.catalog.begin(), runtime.catalog.end(), requested) !=
                runtime.catalog.end()) {
                return requested;
            }
            for (const auto& alias : runtime.aliases) {
                if (alias.first == requested) {
                    return alias.second;
                }
            }
        }
    }
    return runtime.model;
}

void HandleNonStreaming(Runtime& runtime, const httplib::Request& editor, const Json& request,
                        httplib::Response& response) {
    const std::string effective = EffectiveModel(runtime, request);
    const std::string upstream = translate::RequestToOpenAI(request, effective).dump();
    // Claude Desktop probes every picker model at startup and errors the whole
    // gateway if one is refused, so a bursty rate-limited model (gemma-4 on Vertex
    // answers ~1 request in 3) breaks it. Only there — where `aliases` is set — a
    // few quick retries ride out a transient 429 without hiding a genuine outage:
    // if every attempt is refused, the error still surfaces. The CLI path keeps
    // the forward-once contract, so the editor sees the Retry-After and backs off.
    const int attempts = runtime.aliases.empty() ? 1 : 4;
    wally::net::WatchedResult result;
    for (int attempt = 0; attempt < attempts; ++attempt) {
        result = PostUpstream(runtime, false, runtime.prefix + "/chat/completions", upstream, nullptr,
                              editor.is_connection_closed);
        if (result.abandoned || !result.reply || result.reply->status != 429) {
            break;
        }
        if (attempt + 1 < attempts) {
            std::this_thread::sleep_for(std::chrono::milliseconds(400));
        }
    }
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
        // A 429 or 503 from the hosted API carries a Retry-After the wrapped
        // tool should honor; httplib drops upstream headers unless we copy them.
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
    response.set_content(translate::ResponseToAnthropic(parsed, effective).dump(),
                         "application/json");
}

/// Carries an upstream stream from the worker thread that runs the upstream
/// call to the sink that writes to the editor.
///
/// A worker is needed because the upstream status is only known once its
/// headers arrive, and the pre-stream decision (commit a 200 event-stream, or
/// answer a 429/503 as a normal reply, #83) has to be made before the sink is
/// installed. The worker peeks the headers and hands whole transport chunks
/// across a single slot, which bounds read-ahead and keeps backpressure on a
/// long stream.
struct StreamPipe {
    enum class ReadResult {
        Chunk,
        KeepAlive,
        Finished,
    };

    std::mutex mutex;
    std::condition_variable changed;
    std::thread worker;
    bool headers_ready = false;
    bool finished = false;
    bool stopped = false;
    bool successful = false;
    bool abandoned = false;
    int status = 0;
    std::string retry_after;
    std::string chunk;
    std::string error_body;

    ~StreamPipe() {
        {
            std::lock_guard<std::mutex> lock(mutex);
            stopped = true;
            changed.notify_all();
        }
        if (worker.joinable()) {
            worker.join();
        }
    }

    ReadResult Read(std::string* next) {
        std::unique_lock<std::mutex> lock(mutex);
        if (!changed.wait_for(lock, std::chrono::seconds(1),
                              [&] { return !chunk.empty() || finished; })) {
            return ReadResult::KeepAlive;
        }
        if (chunk.empty()) {
            return ReadResult::Finished;
        }
        *next = std::move(chunk);
        chunk.clear();
        changed.notify_all();
        return ReadResult::Chunk;
    }
};

void HandleStreaming(Runtime& runtime, const httplib::Request& editor, const Json& request,
                     httplib::Response& response) {
    // The upstream body is built here rather than in the worker: the worker
    // outlives this function, and everything it touches has to outlive it too.
    const std::string effective = EffectiveModel(runtime, request);
    auto upstream = std::make_shared<std::string>(
        translate::RequestToOpenAI(request, effective).dump());
    auto path = std::make_shared<std::string>(runtime.prefix + "/chat/completions");
    auto model = std::make_shared<std::string>(effective);
    // Computed once, up front: message_start's usage estimate is built from the
    // request and must be ready before the first upstream chunk arrives.
    const int input_estimate = translate::EstimateRequestTokens(request);
    // The Runtime outlives every worker: Stop() sets `stopping`, stops the
    // server and joins its thread before the Runtime is destroyed, and the pool
    // is shared besides.
    Runtime* owner = &runtime;
    // The editor's socket, peeked by the upstream watch: a copy of the request's
    // own closure, which captures the fd by value.
    std::function<bool()> reader_gone = editor.is_connection_closed;

    auto pipe = std::make_shared<StreamPipe>();
    StreamPipe* transfer = pipe.get();
    transfer->worker = std::thread([transfer, owner, upstream, path, reader_gone] {
        try {
            const wally::net::WatchedResult result = PostUpstream(
                *owner, true, *path, *upstream,
                [&](const char* data, size_t length) {
                    std::unique_lock<std::mutex> lock(transfer->mutex);
                    if (transfer->status < 200 || transfer->status >= 300) {
                        // A non-2xx body is the error body, kept bounded so a
                        // real (2xx) stream of any size costs only these few KB.
                        constexpr size_t cap = 8192;
                        if (transfer->error_body.size() < cap) {
                            transfer->error_body.append(
                                data, std::min(length, cap - transfer->error_body.size()));
                        }
                        return !transfer->stopped;
                    }
                    // Backpressure: hold the next transport chunk until the sink
                    // has taken the last one.
                    transfer->changed.wait(
                        lock, [&] { return transfer->chunk.empty() || transfer->stopped; });
                    if (transfer->stopped) {
                        return false;
                    }
                    transfer->chunk.assign(data, length);
                    transfer->changed.notify_all();
                    return true;
                },
                reader_gone,
                [&](const httplib::Response& headers) {
                    std::lock_guard<std::mutex> lock(transfer->mutex);
                    transfer->status = headers.status;
                    transfer->retry_after = headers.get_header_value("Retry-After");
                    transfer->headers_ready = true;
                    transfer->changed.notify_all();
                });
            std::lock_guard<std::mutex> lock(transfer->mutex);
            transfer->successful =
                result.reply && result.reply->status >= 200 && result.reply->status < 300;
            transfer->abandoned = result.abandoned;
            transfer->finished = true;
            transfer->changed.notify_all();
        } catch (const std::exception&) {
            std::lock_guard<std::mutex> lock(transfer->mutex);
            transfer->finished = true;
            transfer->changed.notify_all();
        }
    });

    {
        std::unique_lock<std::mutex> lock(pipe->mutex);
        pipe->changed.wait(lock, [&] { return pipe->headers_ready || pipe->finished; });
        if (pipe->status < 200 || pipe->status >= 300) {
            // Pre-stream failure: known before the sink is committed, so it can
            // be answered as a normal reply that keeps the status and any
            // Retry-After (#83), rather than a 200 stream carrying an error.
            pipe->changed.wait(lock, [&] { return pipe->finished; });
            if (pipe->abandoned) {
                response.status = 499;
                return;
            }
            response.status = pipe->status ? pipe->status : 502;
            if (!pipe->retry_after.empty()) {
                response.set_header("Retry-After", pipe->retry_after);
            }
            std::string type;
            std::string message;
            translate::UpstreamFailure(pipe->status, pipe->error_body, &type, &message);
            LogUpstreamError(*model, true, pipe->status, pipe->error_body);
            response.set_content(translate::ErrorBody(type, message), "application/json");
            return;
        }
    }

    response.set_chunked_content_provider(
        "text/event-stream",
        [pipe, model, input_estimate](size_t /*offset*/, httplib::DataSink& sink) {
            translate::StreamState state;
            state.model = *model;
            state.input_estimate = input_estimate;
            std::string pending;
            std::string payload;
            bool has_data = false;
            bool saw_done = false;

            auto receive = [&](const char* data, size_t length) {
                pending.append(data, length);
                // Consume complete lines across arbitrary transport chunks.
                // CRLF and multi-line data fields are valid SSE too.
                size_t split = 0;
                while ((split = pending.find('\n')) != std::string::npos) {
                    std::string line = pending.substr(0, split);
                    pending.erase(0, split + 1);
                    if (!line.empty() && line.back() == '\r') {
                        line.pop_back();
                    }
                    if (!line.empty()) {
                        if (line == "data" || line.rfind("data:", 0) == 0) {
                            std::string value = line == "data" ? "" : line.substr(5);
                            if (!value.empty() && value.front() == ' ') {
                                value.erase(0, 1);
                            }
                            if (has_data) {
                                payload += '\n';
                            }
                            payload += value;
                            has_data = true;
                        }
                        continue;
                    }
                    if (!has_data) {
                        continue;  // comments/keepalives
                    }
                    has_data = false;
                    std::string events;
                    if (saw_done) {
                        events = translate::StreamErrorToAnthropic(
                            &state, "the model endpoint sent data after [DONE]");
                    } else if (payload == "[DONE]") {
                        saw_done = true;
                    } else {
                        try {
                            events =
                                translate::StreamChunkToAnthropic(Json::parse(payload), &state);
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
            for (;;) {
                const StreamPipe::ReadResult read = pipe->Read(&bytes);
                if (read == StreamPipe::ReadResult::Finished) {
                    break;
                }
                if (read == StreamPipe::ReadResult::KeepAlive) {
                    static constexpr std::string_view keepalive = ": keepalive\n\n";
                    if (!sink.write(keepalive.data(), keepalive.size())) {
                        return false;
                    }
                    continue;
                }
                if (!receive(bytes.data(), bytes.size())) {
                    return false;
                }
            }

            if (pipe->abandoned) {
                // The editor is gone; the cancel is on its way (or was named as
                // impossible). Nothing written here reaches anyone.
                sink.done();
                return false;
            }
            if (!pipe->successful) {
                const int status = 0;
                LogUpstreamError(*model, true, status, pipe->error_body);
                std::string type;
                std::string message;
                translate::UpstreamFailure(status, pipe->error_body, &type, &message);
                const std::string body = translate::StreamErrorToAnthropic(&state, message);
                if (!body.empty()) {
                    sink.write(body.data(), body.size());
                }
                sink.done();
                return false;
            }
            try {
                // Transport EOF is not inference completion. Our OpenAI upstream
                // must send a finish reason followed by [DONE]; anything else is
                // an incomplete stream and must not read as a clean turn (#84).
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
                if (!error.empty()) {
                    sink.write(error.data(), error.size());
                }
            }
            sink.done();
            return true;
        });
}

}  // namespace

bool Start(const harness::Endpoint& upstream, const std::string& model, Shim* shim, bool verbose,
           const std::string& advertised, const ModelAliases& aliases) {
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
    // The real routable ids, for EffectiveModel. Reads a local file, no network.
    runtime->catalog = account::CachedModelIds();
    runtime->aliases = aliases;
    runtime->local_token = wally::net::GenerateLoopbackToken();
    runtime->verbose = verbose;
    wally::net::UpstreamOptions pool_options;
    pool_options.origin = runtime->origin;
    runtime->pool = std::make_shared<wally::net::UpstreamPool>(pool_options);
    if (!runtime->console_url.empty() && !runtime->api_key.empty()) {
        // Three seconds per cancel: fire-and-forget, and the bound on how long
        // an exiting wrapper waits for the last one to go out.
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
        Json parsed;
        try {
            parsed = Json::parse(request.body);
        } catch (const Json::exception& error) {
            response.status = 400;
            response.set_content(translate::ErrorBody("invalid_request_error", error.what()),
                                 "application/json");
            return;
        }
        if (raw->verbose) {
            // The id the app asked for and the one that will answer, side by
            // side: this is the line that shows a picker choice being honoured
            // or silently collapsing onto the launched default.
            const std::string requested = parsed.value("model", std::string("<none>"));
            out::status_line("anthropic: POST /v1/messages, " +
                             std::to_string(request.body.size()) + " bytes, model " + requested +
                             " -> " + EffectiveModel(*raw, parsed));
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
        // Claude Desktop reconciles its picker against discovery, so advertise the
        // family names it will list; the CLI path advertises the one real id.
        Json data = Json::array();
        if (raw->aliases.empty()) {
            data.push_back(Json{{"id", raw->advertised}, {"object", "model"}});
        } else {
            for (const auto& alias : raw->aliases) {
                data.push_back(Json{{"id", alias.first}, {"object", "model"}});
            }
        }
        response.set_content(Json{{"object", "list"}, {"data", std::move(data)}}.dump(),
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
        // for an id (the editor left during prefill) gives up on its next poll
        // instead of holding the server thread until the first token; then the
        // server, which joins every handler; then the cancel queue, so the last
        // abandon's cancel goes out before the process does -- bounded by 3 s
        // per queued cancel, typically one.
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
