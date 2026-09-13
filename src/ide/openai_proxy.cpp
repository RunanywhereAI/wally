#include "ide/openai_proxy.h"

#include <httplib.h>
#include <nlohmann/json.hpp>

#include <atomic>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

#include <cctype>
#include <fstream>

#if !defined(_WIN32)
#include <fcntl.h>
#include <unistd.h>
#endif

#include "account/cancel_worker.h"
#include "account/console.h"
#include "account/credentials.h"
#include "io/output.h"
#include "net/loopback_auth.h"
#include "net/upstream_call.h"
#include "net/upstream_pool.h"

namespace wally::ide {
namespace {

/// Splits `http://host:port/v1` into `http://host:port` and `/v1`.
bool SplitBaseURL(const std::string& base_url, std::string* origin, std::string* prefix) {
    const size_t scheme = base_url.find("://");
    if (scheme == std::string::npos) {
        return false;
    }
    const size_t slash = base_url.find('/', scheme + 3);
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
    // The session's bearer. Renewed mid-session on a 401 (RenewToken) from a
    // sink thread while other sinks and the cancel worker read it, so it
    // goes through the guard below.
    std::string api_key;
    mutable std::mutex key_mutex;
    std::string Token() const {
        std::lock_guard<std::mutex> lock(key_mutex);
        return api_key;
    }
    void SetToken(std::string token) {
        std::lock_guard<std::mutex> lock(key_mutex);
        api_key = std::move(token);
    }
    std::string model;
    // The secret the editor must present. Loopback binding keeps the network
    // out; this keeps another local process out.
    std::string local_token;
    bool verbose = false;
    // Upstream connections kept open across requests (wally #80). Shared: a
    // streaming sink still running after StopProxy() holds a lease on it.
    std::shared_ptr<wally::net::UpstreamPool> pool;
    // Where an abandoned request is cancelled by name (#81); see the
    // Anthropic shim's Runtime for the same three fields.
    std::string console_url;
    std::atomic<bool> stopping{false};
    std::unique_ptr<wally::account::CancelWorker> cancels;
};

std::unique_ptr<Runtime> g_runtime;

void Trace(bool verbose, const std::string& note);

/// Whether a refusal is about the credential rather than the request.
///
/// The console words this more than one way — "access token expired" when it
/// lapses, "not authenticated" when it is rejected outright — so matching a
/// single phrase catches only half the cases, and the half it misses ends the
/// session.
bool LooksLikeAuthFailure(const std::string& body) {
    std::string lowered;
    lowered.reserve(body.size());
    for (const char c : body) {
        lowered.push_back(static_cast<char>(std::tolower(static_cast<unsigned char>(c))));
    }
    return lowered.find("expired") != std::string::npos ||
           lowered.find("authenticat") != std::string::npos ||
           lowered.find("unauthorized") != std::string::npos ||
           lowered.find("invalid_token") != std::string::npos ||
           lowered.find("401") != std::string::npos;
}

/// Trades the stored refresh token for a new access token.
///
/// The one held at startup is a snapshot, and an editor session outlives it.
/// Without this the whole run dies on `access token expired` partway through,
/// with nothing but a 401 to explain itself.
bool RenewToken(Runtime& runtime) {
    account::Credentials credentials = account::Load();
    if (credentials.refresh_token.empty()) {
        return false;
    }
    account::Grant grant;
    std::string error;
    if (!account::Refresh(credentials.console_url, credentials.refresh_token, &grant, &error)) {
        Trace(runtime.verbose, "REFRESH-FAILED " + error);
        return false;
    }
    credentials.access_token = grant.access_token;
    if (!grant.refresh_token.empty()) {
        credentials.refresh_token = grant.refresh_token;
    }
    std::string ignored;
    account::Save(credentials, &ignored);
    runtime.SetToken(grant.access_token);
    Trace(runtime.verbose, "REFRESHED");
    return true;
}

/// Where the proxy writes its trace, when one was asked for.
///
/// Not `/tmp`. That directory is world-writable, so any other local account can
/// pre-create the path as a symlink and `std::ofstream` follows it; and the
/// mode comes from the umask, which on a normal machine means world-readable.
/// The profile directory is the credential store's, already created 0700.
std::string TracePath() {
    const std::string directory = account::ProfileDirectory();
    return directory.empty() ? std::string() : directory + "/proxy-trace.log";
}

/// Appends one line, and only when the reader asked for tracing.
///
/// Never the request body. That carries the developer's prompt and whatever
/// source their editor attached to it, and this file outlives the session.
void Trace(bool verbose, const std::string& note) {
    if (!verbose) {
        return;
    }
    const std::string path = TracePath();
    if (path.empty()) {
        return;
    }
#if defined(_WIN32)
    // %LOCALAPPDATA% is per-user and there is no O_NOFOLLOW to reach for here.
    std::ofstream log(path, std::ios::app);
    log << note << "\n";
#else
    // O_NOFOLLOW so a symlink planted at the path is an error rather than a
    // redirect, and 0600 so the mode does not depend on the caller's umask.
    const int fd =
        ::open(path.c_str(), O_WRONLY | O_CREAT | O_APPEND | O_NOFOLLOW | O_CLOEXEC, 0600);
    if (fd < 0) {
        return;
    }
    const std::string line = note + "\n";
    const ssize_t written = ::write(fd, line.data(), line.size());
    static_cast<void>(written);
    ::close(fd);
#endif
}

using Json = nlohmann::json;

/// A chunk carrying `message` as the whole answer.
///
/// Built by hand rather than forwarded, for the case where the upstream said
/// something the editor cannot read.
std::string ChunkSaying(const std::string& message) {
    Json chunk;
    chunk["id"] = "chatcmpl-wally";
    chunk["object"] = "chat.completion.chunk";
    chunk["created"] = 0;
    chunk["model"] = "wally";
    Json choice;
    choice["index"] = 0;
    choice["delta"] = Json{{"role", "assistant"}, {"content", message}};
    choice["finish_reason"] = "stop";
    chunk["choices"] = Json::array({choice});
    return "data: " + chunk.dump() + "\n\n";
}

/// Numbers the tool calls in a streamed delta, and says whether it had to.
///
/// Each tool call in a stream carries an `index` saying which call the fragment
/// belongs to, because arguments arrive split across frames. Gemini's
/// OpenAI-compatible layer leaves it out, and a strict client refuses the whole
/// frame — so a model that answers by calling a tool fails where the same model
/// answering in prose succeeds. The position in the array is the index it
/// should have had.
bool NumberToolCalls(Json& chunk) {
    bool changed = false;
    if (!chunk["choices"].is_array()) {
        return false;
    }
    for (Json& choice : chunk["choices"]) {
        if (!choice.is_object() || !choice.contains("delta") || !choice["delta"].is_object()) {
            continue;
        }
        Json& delta = choice["delta"];
        if (!delta.contains("tool_calls") || !delta["tool_calls"].is_array()) {
            continue;
        }
        size_t position = 0;
        for (Json& call : delta["tool_calls"]) {
            if (call.is_object() && !call.contains("index")) {
                call["index"] = position;
                changed = true;
            }
            ++position;
        }
    }
    return changed;
}

/// Passes an event through, rewriting the ones the editor would choke on.
///
/// An upstream error arrives inside the stream, correctly framed, as an object
/// with an `error` member and no `choices`. The editor deserialises every frame
/// into one fixed shape and rejects anything missing its required fields, so
/// that frame surfaces as a deserialiser complaint and the actual message —
/// which is the thing worth reading — never reaches anybody. Turning it into an
/// ordinary chunk puts it in the chat instead.
std::string Normalise(const std::string& frame, bool verbose) {
    const size_t field = frame.find("data:");
    if (field == std::string::npos) {
        return frame + "\n\n";
    }
    std::string payload = frame.substr(field + 5);
    while (!payload.empty() && (payload.front() == ' ' || payload.front() == '\r')) {
        payload.erase(payload.begin());
    }
    if (payload == "[DONE]") {
        return frame + "\n\n";
    }
    Json parsed = Json::parse(payload, nullptr, false);
    if (parsed.is_discarded() || !parsed.is_object()) {
        return frame + "\n\n";
    }
    if (parsed.contains("choices")) {
        return NumberToolCalls(parsed) ? "data: " + parsed.dump() + "\n\n" : frame + "\n\n";
    }
    if (!parsed.contains("error")) {
        return frame + "\n\n";
    }
    const Json& error = parsed["error"];
    std::string message = error.is_object() && error.contains("message") &&
                                  error["message"].is_string()
                              ? error["message"].get<std::string>()
                              : error.dump();
    Trace(verbose, "UPSTREAM-ERROR-FRAME " + message);
    return ChunkSaying(message);
}

/// Points a request at the model wally is serving, whatever it named.
///
/// A stale selection saved in the editor's own settings outlives any change to
/// the list we advertise, so the name in the request cannot be trusted even
/// when only one is on offer.
std::string Retarget(const Runtime& runtime, const std::string& body) {
    Json request = Json::parse(body, nullptr, false);
    if (request.is_discarded() || !request.is_object()) {
        return body;
    }
    request["model"] = runtime.model;
    return request.dump();
}

void Fail(httplib::Response& response, int status, const std::string& message) {
    response.status = status;
    // Built with Json, not concatenated. `message` is upstream text or an
    // exception's what(), and one quote or backslash in it produced a body the
    // editor could not parse, so the reader saw a parse failure instead of the
    // cause.
    response.set_content(Json{{"error", {{"message", message}}}}.dump(), "application/json");
}

/// The abandon path (#81): trace it, and queue the cancel off this thread.
void OnAbandoned(Runtime& runtime, bool streaming, const std::string& request_id, int status,
                 bool during_prefill) {
    std::string line = std::string("ABANDONED during=") + (during_prefill ? "prefill" : "stream") +
                       " id=" + (request_id.empty() ? std::string("unknown") : request_id) +
                       " status=" + std::to_string(status) + " stream=" + (streaming ? "1" : "0");
    if (request_id.empty()) {
        Trace(runtime.verbose, line + " cancel=none(no-id)");
        return;
    }
    if (!runtime.cancels) {
        // No worker: a local server, which has no console to tell.
        Trace(runtime.verbose, line + " cancel=skipped(local)");
        return;
    }
    Trace(runtime.verbose, line + " cancel=queued");
    runtime.cancels->Enqueue(request_id);
}

/// One watched upstream POST on a lease (#81). The stale-retry decision stays
/// with the callers, which also have the auth-renew retry to compose it with;
/// both are vetoed once the editor left.
wally::net::WatchedResult PostWatched(Runtime& runtime, wally::net::UpstreamLease& lease,
                                      bool streaming, const std::string& path,
                                      const std::string& body,
                                      const httplib::ContentReceiver& receiver,
                                      const std::function<bool()>& reader_gone) {
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
    return wally::net::PostWatched(lease, call);
}

/// A non-streaming request on a pooled connection, once more on a fresh one
/// if a reused connection turned out to be stale. Nothing has been answered
/// to the editor when that decision is made. A null result after the editor
/// left is deliberate: the caller's 502 goes to nobody, and nothing is
/// re-sent.
httplib::Result PostOnce(Runtime& runtime, const std::string& path, const std::string& body,
                         const std::function<bool()>& reader_gone) {
    for (int attempt = 0; attempt < 2; ++attempt) {
        wally::net::UpstreamLease lease = runtime.pool->acquire(runtime.Token());
        Trace(runtime.verbose,
              lease.reused() ? "UPSTREAM reused connection" : "UPSTREAM fresh connection");
        wally::net::WatchedResult result =
            PostWatched(runtime, lease, false, path, body, nullptr, reader_gone);
        if (result.reply) {
            return std::move(result.reply);
        }
        lease.discard();
        if (result.abandoned) {
            Trace(runtime.verbose, "UPSTREAM request dropped: the editor left");
            return std::move(result.reply);
        }
        if (attempt == 0 &&
            wally::net::RetryOnFreshConnection(result.reply.error(), false, false, lease.reused())) {
            Trace(runtime.verbose, "UPSTREAM stale connection; retrying once on a fresh one");
            continue;
        }
        return std::move(result.reply);
    }
    return httplib::Result{nullptr, httplib::Error::Unknown};
}

/// Forwards a streaming completion, byte for byte.
///
/// Nothing is parsed. Both ends speak the same wire format, so reframing SSE
/// here would only add a place for it to go wrong — and did, the first time.
void Stream(Runtime& runtime, const std::function<bool()>& reader_gone, const std::string& body,
            httplib::Response& response) {
    auto request = std::make_shared<std::string>(body);
    auto path = std::make_shared<std::string>(runtime.prefix + "/chat/completions");
    auto api_key = std::make_shared<std::string>(runtime.Token());

    auto verbose = std::make_shared<bool>(runtime.verbose);
    Runtime* owner = &runtime;
    // Size only. The body is the developer's prompt and their source.
    Trace(runtime.verbose, "REQUEST " + std::to_string(body.size()) + " bytes");

    response.set_chunked_content_provider(
        "text/event-stream",
        [request, path, api_key, verbose, owner, reader_gone](size_t, httplib::DataSink& sink) {
          // Two kinds of second attempt, each at most once: after a token
          // the console has just renewed (a 401 came back), and on a fresh
          // connection when a REUSED one turned out to be stale (nothing
          // came back at all -- RetryOnFreshConnection). Nothing reaches the
          // sink until an event stream is recognised, so neither retry can
          // duplicate output.
          int auth_attempt = 0;
          bool stale_retried = false;
          for (;;) {
            const std::string token = auth_attempt == 0 ? *api_key : owner->Token();
            wally::net::UpstreamLease lease = owner->pool->acquire(token);
            Trace(*verbose, lease.reused() ? "UPSTREAM reused connection"
                                           : "UPSTREAM fresh connection");
            bool received_any = false;

            // An upstream that refuses the request answers with a JSON error and
            // no SSE framing at all. Forwarding those bytes as if they were
            // events puts an object into the stream that carries none of the
            // fields a chunk must have, and the editor blames the stream rather
            // than the refusal. So the body is held until the status is known.
            // An upstream that refuses the request answers with a JSON error
            // and no SSE framing at all. Forwarding those bytes as events puts
            // an object into the stream carrying none of the fields a chunk
            // must have, and the editor then blames the stream rather than the
            // refusal. So the opening bytes are held back until they identify
            // themselves: an event stream starts with a `data:` field, and an
            // error does not.
            bool streaming = false;
            bool decided = false;
            std::string head;
            // Whole events only: a chunk can split one in half, and half an
            // event cannot be judged.
            std::string pending;
            const auto forward = [&sink, &pending, verbose]() {
                size_t split = 0;
                while ((split = pending.find("\n\n")) != std::string::npos) {
                    const std::string frame = pending.substr(0, split);
                    pending.erase(0, split + 2);
                    const std::string out = Normalise(frame, *verbose);
                    if (!sink.write(out.data(), out.size())) {
                        return false;
                    }
                }
                return true;
            };
            const wally::net::WatchedResult watched = PostWatched(
                *owner, lease, true, *path, *request,
                [&](const char* data, size_t length) {
                                received_any = true;
                                if (decided) {
                                    if (!streaming) {
                                        head.append(data, length);
                                        return true;
                                    }
                                    pending.append(data, length);
                                    return forward();
                                }
                                head.append(data, length);
                                const size_t start = head.find_first_not_of(" \r\n");
                                if (start == std::string::npos) {
                                    return true;
                                }
                                if (head.compare(start, 5, "data:") == 0) {
                                    streaming = true;
                                    decided = true;
                                    pending.append(head);
                                    return forward();
                                }
                                // Enough to know it is not an event stream.
                                if (head.size() - start >= 5) {
                                    decided = true;
                                }
                                return true;
                            },
                reader_gone);
            const httplib::Result& reply = watched.reply;

            if (watched.abandoned) {
                // The editor is gone. Neither retry may run for it: the stop
                // that ended this call looks like a stale connection, and a
                // renewed token would only re-send the prompt to nobody.
                lease.discard();
                sink.done();
                return false;
            }

            if (streaming) {
                sink.done();
                return true;
            }

            if (!reply) {
                // The socket is in no state to reuse.
                lease.discard();
                if (!stale_retried &&
                    wally::net::RetryOnFreshConnection(reply.error(), false, received_any,
                                                       lease.reused())) {
                    stale_retried = true;
                    Trace(*verbose, "UPSTREAM stale connection; retrying once on a fresh one");
                    continue;
                }
            } else if (auth_attempt == 0 && LooksLikeAuthFailure(head) && RenewToken(*owner)) {
                ++auth_attempt;
                head.clear();
                pending.clear();
                decided = false;
                continue;
            }

            {
                const std::string detail =
                    !reply ? std::string("the model endpoint did not answer") : head;
                Trace(*verbose, "UPSTREAM-REFUSED " + detail);
                // Carried as an ordinary chunk, not as an `error` object. The
                // editor deserialises every frame into one fixed shape and
                // rejects anything without its required fields, so an error
                // object here fails to parse and the reader is shown a
                // deserialiser complaint instead of what actually went wrong.
                std::string message = detail;
                for (char& c : message) {
                    if (c == '"' || c == '\\' || c == '\n' || c == '\r' || c == '\t') {
                        c = ' ';
                    }
                }
                const std::string frame =
                    "data: {\"id\":\"chatcmpl-wally\",\"object\":\"chat.completion.chunk\","
                    "\"created\":0,\"model\":\"wally\",\"choices\":[{\"index\":0,\"delta\":"
                    "{\"role\":\"assistant\",\"content\":\"" + message +
                    "\"},\"finish_reason\":\"stop\"}]}\n\n";
                sink.write(frame.data(), frame.size());
                sink.write("data: [DONE]\n\n", 14);
            }
            sink.done();
            return true;
          }
        });
}

}  // namespace

bool StartProxy(const harness::Endpoint& endpoint, const std::string& model, int port,
                Proxy* proxy, bool verbose) {
    if (proxy == nullptr) {
        return false;
    }
    StopProxy(proxy);

    auto runtime = std::make_unique<Runtime>();
    if (!SplitBaseURL(endpoint.base_url, &runtime->origin, &runtime->prefix)) {
        out::error_line("cannot make sense of the endpoint " + endpoint.base_url);
        return false;
    }
    runtime->api_key = endpoint.api_key;
    runtime->console_url = endpoint.console_url;
    runtime->model = model;
    runtime->local_token = wally::net::GenerateLoopbackToken();
    runtime->verbose = verbose;
    wally::net::UpstreamOptions pool_options;
    pool_options.origin = runtime->origin;
    runtime->pool = std::make_shared<wally::net::UpstreamPool>(pool_options);
    if (!runtime->console_url.empty() && !runtime->api_key.empty()) {
        Runtime* for_log = runtime.get();
        // The bearer is read when each cancel goes out, so one sent after a
        // renewal carries the renewed token.
        runtime->cancels = std::make_unique<wally::account::CancelWorker>(
            runtime->console_url, [for_log] { return for_log->Token(); }, 3000,
            [for_log](const std::string& id, wally::account::CancelOutcome outcome,
                      const std::string& error) {
                const char* word = outcome == wally::account::CancelOutcome::Cancelled ? "202"
                                   : outcome == wally::account::CancelOutcome::NotFound ? "404"
                                                                                       : "failed";
                Trace(for_log->verbose, "CANCEL id=" + id + " result=" + word +
                                            (error.empty() ? std::string() : " error=" + error));
            });
    }

    Runtime* raw = runtime.get();
    // Every handler catches. An exception thrown into cpp-httplib takes the
    // process down with it, and a dead wally takes the model with it too.
    raw->server.Get("/v1/models", [raw](const httplib::Request&, httplib::Response& response) {
        try {
            // Not forwarded. The one model wally was asked to serve is the one
            // offered, so there is nothing in the picker that cannot answer.
            Json entry;
            entry["id"] = raw->model;
            entry["object"] = "model";
            entry["owned_by"] = "runanywhere";
            Json list;
            list["object"] = "list";
            list["data"] = Json::array({entry});
            response.set_content(list.dump(), "application/json");
        } catch (const std::exception& error) {
            Fail(response, 500, error.what());
        }
    });

    raw->server.Post("/v1/chat/completions",
                     [raw](const httplib::Request& request, httplib::Response& response) {
                         // This endpoint spends the signed-in user's credit, so
                         // it serves only the editor wally configured. Bearer
                         // token, from the provider key stored in the IDE.
                         std::string presented;
                         const std::string authorization = request.get_header_value("Authorization");
                         constexpr const char* kBearer = "Bearer ";
                         if (authorization.rfind(kBearer, 0) == 0) {
                             presented = authorization.substr(std::string(kBearer).size());
                         }
                         if (!wally::net::ConstantTimeEquals(presented, raw->local_token)) {
                             Fail(response, 401,
                                  "this local endpoint only serves the editor wally configured");
                             return;
                         }
                         try {
                             const std::string body = Retarget(*raw, request.body);
                             // The editor decides whether to stream; we only
                             // have to keep the answer in the shape it asked for.
                             if (body.find("\"stream\":true") != std::string::npos ||
                                 body.find("\"stream\": true") != std::string::npos) {
                                 Stream(*raw, request.is_connection_closed, body, response);
                                 return;
                             }
                             const httplib::Result reply =
                                 PostOnce(*raw, raw->prefix + "/chat/completions", body,
                                          request.is_connection_closed);
                             if (!reply) {
                                 Fail(response, 502, "the model endpoint did not answer");
                                 return;
                             }
                             response.status = reply->status;
                             // An overloaded upstream answers 429 with a
                             // Retry-After the wrapped tool is expected to back
                             // off on. httplib drops response headers unless we
                             // copy them, so forward this one explicitly.
                             if (reply->status == 429 && reply->has_header("Retry-After")) {
                                 response.set_header("Retry-After",
                                                     reply->get_header_value("Retry-After"));
                             }
                             response.set_content(reply->body, "application/json");
                         } catch (const std::exception& error) {
                             Fail(response, 500, error.what());
                         }
                     });

    // The usual port, or any free one when a second editor already holds it.
    // The address is written into that editor's settings either way, so the two
    // do not have to agree on a number.
    int bound = port;
    if (!raw->server.bind_to_port("127.0.0.1", port)) {
        bound = raw->server.bind_to_any_port("127.0.0.1");
        if (bound <= 0) {
            out::error_line("could not find a port to serve " + model + " on");
            return false;
        }
    }
    runtime->thread = std::thread([raw] { raw->server.listen_after_bind(); });
    g_runtime = std::move(runtime);

    proxy->running = true;
    proxy->base_url = "http://127.0.0.1:" + std::to_string(bound) + "/v1";
    proxy->auth_token = raw->local_token;
    return true;
}

void StopProxy(Proxy* proxy) {
    if (g_runtime) {
        // Same order as the Anthropic shim's Stop(): let an in-flight watch
        // give up waiting for an id, stop the server (joins every handler),
        // then drain the cancel queue so the last abandon's cancel goes out.
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
    if (proxy != nullptr) {
        proxy->running = false;
        proxy->base_url.clear();
    }
}

}  // namespace wally::ide
