#include "ide/openai_proxy.h"

#include <httplib.h>
#include <nlohmann/json.hpp>

#include <memory>
#include <string>
#include <thread>

#include <cctype>
#include <cstdlib>
#include <fstream>

#if !defined(_WIN32)
#include <fcntl.h>
#include <unistd.h>
#endif

#include "account/console.h"
#include "account/credentials.h"
#include "io/output.h"
#include "net/loopback_auth.h"

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
    std::string api_key;
    std::string model;
    // The secret the editor must present. Loopback binding keeps the network
    // out; this keeps another local process out.
    std::string local_token;
    bool verbose = false;
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
    runtime.api_key = grant.access_token;
    Trace(runtime.verbose, "REFRESHED");
    return true;
}

httplib::Client Upstream(const Runtime& runtime) {
    httplib::Client client(runtime.origin);
    client.set_read_timeout(600, 0);
    if (!runtime.api_key.empty()) {
        client.set_bearer_token_auth(runtime.api_key);
    }
    return client;
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
/// The request with its model replaced by the one wally was told to serve.
///
/// Returns false when the body is not a JSON object, and the caller refuses the
/// request. Forwarding it unchanged was the old behaviour and it defeated the
/// point: the model named by the caller is exactly what must not reach the
/// endpoint, so a body we cannot rewrite is one we cannot safely send.
/// Repairs tool calls the endpoint will not accept back.
///
/// A streamed tool call arrives as fragments, and this endpoint's first one
/// carries `arguments: ""` before the real value follows. A client that keeps
/// that first fragment instead of joining them replays the call with empty
/// arguments on the next turn, and the endpoint refuses its own model's output
/// with "Assistant tool call function.arguments must be a JSON object" — which
/// names neither the message nor the call, and reads as a failure of whatever
/// the person just asked for.
///
/// Empty arguments mean a call with no arguments, so that is what is sent.
void RepairToolCalls(Json* request) {
    const auto messages = request->find("messages");
    if (messages == request->end() || !messages->is_array()) {
        return;
    }
    for (Json& message : *messages) {
        if (!message.is_object()) {
            continue;
        }
        const auto calls = message.find("tool_calls");
        if (calls == message.end() || !calls->is_array()) {
            continue;
        }
        for (Json& call : *calls) {
            if (!call.is_object()) {
                continue;
            }
            const auto function = call.find("function");
            if (function == call.end() || !function->is_object()) {
                continue;
            }
            const auto arguments = function->find("arguments");
            if (arguments == function->end()) {
                continue;
            }
            if (arguments->is_string() &&
                arguments->get<std::string>().find_first_not_of(" \t\r\n") == std::string::npos) {
                *arguments = "{}";
            } else if (arguments->is_object()) {
                // The wire format carries arguments as a string holding JSON,
                // and an object here is refused outright.
                *arguments = arguments->dump();
            }
        }
    }
}

bool Retarget(const Runtime& runtime, const std::string& body, std::string* retargeted) {
    Json request = Json::parse(body, nullptr, false);
    if (request.is_discarded() || !request.is_object()) {
        return false;
    }
    request["model"] = runtime.model;
    RepairToolCalls(&request);
    *retargeted = request.dump();
    return true;
}

void Fail(httplib::Response& response, int status, const std::string& message) {
    response.status = status;
    // Built with Json, not concatenated. `message` is upstream text or an
    // exception's what(), and one quote or backslash in it produced a body the
    // editor could not parse, so the reader saw a parse failure instead of the
    // cause.
    response.set_content(Json{{"error", {{"message", message}}}}.dump(), "application/json");
}

/// Forwards a streaming completion, byte for byte.
///
/// Nothing is parsed. Both ends speak the same wire format, so reframing SSE
/// here would only add a place for it to go wrong — and did, the first time.
void Stream(Runtime& runtime, const std::string& body, httplib::Response& response) {
    auto request = std::make_shared<std::string>(body);
    auto origin = std::make_shared<std::string>(runtime.origin);
    auto path = std::make_shared<std::string>(runtime.prefix + "/chat/completions");
    auto api_key = std::make_shared<std::string>(runtime.api_key);

    auto verbose = std::make_shared<bool>(runtime.verbose);
    Runtime* owner = &runtime;
    // Size only. The body is the developer's prompt and their source.
    Trace(runtime.verbose, "REQUEST " + std::to_string(body.size()) + " bytes");

    response.set_chunked_content_provider(
        "text/event-stream",
        [request, origin, path, api_key, verbose, owner](size_t, httplib::DataSink& sink) {
          // Two attempts at most: the second only after a token the console
          // has just renewed. Nothing reaches the sink until an event stream
          // is recognised, so a retry cannot duplicate output.
          for (int attempt = 0; attempt < 2; ++attempt) {
            httplib::Client client(*origin);
            client.set_read_timeout(600, 0);
            const std::string token = attempt == 0 ? *api_key : owner->api_key;
            if (!token.empty()) {
                client.set_bearer_token_auth(token);
            }

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
            const httplib::Result reply =
                client.Post(*path, httplib::Headers(), *request, "application/json",
                            [&](const char* data, size_t length) {
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
                            });

            if (streaming) {
                sink.done();
                return true;
            }

            if (attempt == 0 && reply && LooksLikeAuthFailure(head) && RenewToken(*owner)) {
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
          sink.done();
          return true;
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
    runtime->model = model;
    runtime->local_token = wally::net::GenerateLoopbackToken();
    runtime->verbose = verbose;

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
                         // No credential is required, and that is a deliberate
                         // trade rather than an oversight.
                         //
                         // This used to demand a per-session bearer token, and
                         // the IDE never sent one: AI Assistant's third-party
                         // provider has no way to be handed a key from outside
                         // its own settings dialog, so every chat request
                         // arrived bare and was refused with a 401 that read as
                         // a licensing problem. A guard that refuses the only
                         // caller it exists to serve is not a guard.
                         //
                         // What remains is the bind: 127.0.0.1, so nothing off
                         // this machine can reach it, and the port lives only
                         // as long as the editor does. Another process on this
                         // machine could spend the signed-in user's credit
                         // through it while it is running, which is the cost.
                         try {
                             std::string body;
                             if (!Retarget(*raw, request.body, &body)) {
                                 Fail(response, 400,
                                      "the request body is not a JSON object; wally could not "
                                      "name the model it serves");
                                 return;
                             }
                             // The editor decides whether to stream; we only
                             // have to keep the answer in the shape it asked for.
                             if (body.find("\"stream\":true") != std::string::npos ||
                                 body.find("\"stream\": true") != std::string::npos) {
                                 Stream(*raw, body, response);
                                 return;
                             }
                             // Once more after a renewed token, the same as the
                             // streaming path. A session that expired while the
                             // editor was open otherwise hands the reader a bare
                             // 401, which reads as a licensing problem rather
                             // than as "sign in again" (wally #86).
                             httplib::Result reply;
                             for (int attempt = 0; attempt < 2; ++attempt) {
                                 httplib::Client client = Upstream(*raw);
                                 reply = client.Post(raw->prefix + "/chat/completions", body,
                                                     "application/json");
                                 const bool expired = reply && reply->status == 401 &&
                                                      LooksLikeAuthFailure(reply->body);
                                 if (attempt == 0 && expired && RenewToken(*raw)) {
                                     continue;
                                 }
                                 break;
                             }
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
        g_runtime->server.stop();
        if (g_runtime->thread.joinable()) {
            g_runtime->thread.join();
        }
        g_runtime.reset();
    }
    if (proxy != nullptr) {
        proxy->running = false;
        proxy->base_url.clear();
    }
}

}  // namespace wally::ide
