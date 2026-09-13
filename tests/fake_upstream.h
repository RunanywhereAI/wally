#ifndef WALLY_TESTS_FAKE_UPSTREAM_H
#define WALLY_TESTS_FAKE_UPSTREAM_H

// Fake OpenAI-shaped upstreams for the loopback translators' tests. Every
// assertion built on these is about which TCP connection a request arrived
// on, read from the server's side as the peer's ephemeral port: the same port
// across requests means the same connection was reused; a different port
// means a new connect (and, against the real endpoint, a new TLS handshake).
// No network beyond 127.0.0.1, no models.

#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <httplib.h>
#include <nlohmann/json.hpp>

#if !defined(_WIN32)
#include <arpa/inet.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

namespace wally_tests {

using Json = nlohmann::json;


constexpr const char* kStreamBody =
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
    "\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
    "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],"
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n"
    "data: [DONE]\n\n";

constexpr const char* kJsonBody =
    "{\"id\":\"chatcmpl-1\",\"object\":\"chat.completion\",\"choices\":[{\"index\":0,"
    "\"message\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}],"
    "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}";

/// An OpenAI-shaped upstream that remembers which connection each request
/// arrived on. `hold_streams_until` makes streaming responses wait until that
/// many requests have arrived before emitting anything, so two concurrent
/// requests are provably in flight together rather than one finishing and
/// lending its connection to the next.
class FakeUpstream {
   public:
    FakeUpstream() {
        server_.Post("/v1/chat/completions",
                     [this](const httplib::Request& request, httplib::Response& response) {
                         const int arrival = Record(request);
                         // Every response names itself the way the real
                         // endpoint does, so a client can cancel it by name.
                         const std::string request_id = "req-" + std::to_string(arrival);
                         response.set_header("x-request-id", request_id);
                         // Withholding the HEADERS: block here, before the
                         // response is written -- the endpoint's gateway
                         // during prefill, which opens the response only at
                         // the first token.
                         WaitForHeaderHold();
                         const bool streaming = Json::parse(request.body).value("stream", false);
                         if (!streaming) {
                             WaitForHold();
                             response.set_content(kJsonBody, "application/json");
                             return;
                         }
                         response.set_chunked_content_provider(
                             "text/event-stream",
                             [this](size_t, httplib::DataSink& sink) {
                                 // Withholding the BODY: headers are already
                                 // on the wire when this runs.
                                 WaitForHold();
                                 const std::string body = kStreamBody;
                                 if (drip_chunks_.load() > 0) {
                                     return Drip(sink);
                                 }
                                 if (die_.load()) {
                                     size_t cut = body.find("\n\n");
                                     cut = body.find("\n\n", cut + 2) + 2;
                                     sink.write(body.data(), cut);
                                     return false;  // httplib closes the connection
                                 }
                                 sink.write(body.data(), body.size());
                                 sink.done();
                                 return true;
                             });
                     });
        // The endpoint's cancel route (InferenceInfra #440), on the same
        // origin the shim talks to: records who cancelled what, and can hold
        // its answer so a test can prove the caller waited for it.
        server_.Post("/v1/requests/:id/cancel",
                     [this](const httplib::Request& request, httplib::Response& response) {
                         // The delay comes FIRST, and the cancel is recorded
                         // only once it is about to be answered: a caller that
                         // did not wait for the answer has not "sent" it as far
                         // as any test here is concerned.
                         const int delay = cancel_delay_ms_.load();
                         if (delay > 0) {
                             std::this_thread::sleep_for(std::chrono::milliseconds(delay));
                         }
                         {
                             std::lock_guard<std::mutex> lock(mutex_);
                             cancels_.push_back({request.path_params.at("id"),
                                                 request.get_header_value("Authorization")});
                         }
                         cancelled_.notify_all();
                         response.status = 202;
                         response.set_content(
                             Json{{"request_id", request.path_params.at("id")},
                                  {"status", "cancelling"}}
                                 .dump(),
                             "application/json");
                     });
        port_ = server_.bind_to_any_port("127.0.0.1");
        thread_ = std::thread([this] { server_.listen_after_bind(); });
        server_.wait_until_ready();
    }

    ~FakeUpstream() {
        // Let go of everything held first, so a handler parked on a hold
        // ends now rather than at its own 5 s timeout.
        closing_.store(true);
        hold_until_.store(0);
        release_headers();
        server_.stop();
        if (thread_.joinable()) {
            thread_.join();
        }
    }

    std::string base_url() const { return "http://127.0.0.1:" + std::to_string(port_) + "/v1"; }

    std::vector<int> ports() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_;
    }

    void hold_streams_until(int arrivals) { hold_until_.store(arrivals); }

    /// Withhold the response HEADERS (not just the body) until `release_headers`
    /// or the timeout: a request still in prefill at the real endpoint.
    void hold_headers(bool on) { hold_headers_.store(on); }
    void release_headers() {
        hold_headers_.store(false);
        arrived_.notify_all();
    }

    /// Send the first two SSE frames, then drop the connection without
    /// finishing the stream: an upstream that died mid-generation.
    void die_mid_stream(bool on) { die_.store(on); }

    /// Stream like a decoding engine: a content frame every `interval_ms`,
    /// `chunks` of them, then the finish frames and [DONE] -- unless a write
    /// fails first (the reader dropped the connection), which ends the drip.
    void drip(int chunks, int interval_ms) {
        drip_interval_ms_.store(interval_ms);
        drip_chunks_.store(chunks);
    }
    /// How many drip frames were written before the drip ended.
    int dripped() const { return dripped_.load(); }

    struct Cancel {
        std::string request_id;
        std::string authorization;
    };
    /// Every cancel the shim sent, in order.
    std::vector<Cancel> cancels() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return cancels_;
    }
    /// Blocks until at least `n` cancels arrived, or the timeout. False on timeout.
    bool wait_for_cancels(int n, std::chrono::milliseconds within = std::chrono::seconds(3)) {
        std::unique_lock<std::mutex> lock(mutex_);
        return cancelled_.wait_for(lock, within,
                                   [&] { return static_cast<int>(cancels_.size()) >= n; });
    }
    /// How long the cancel route sits on its answer before replying 202.
    void delay_cancel_reply(int ms) { cancel_delay_ms_.store(ms); }
    /// The request id the fake gave arrival `n` (1-based).
    static std::string request_id_of(int arrival) { return "req-" + std::to_string(arrival); }
    int arrivals() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return arrivals_;
    }

   private:
    bool Drip(httplib::DataSink& sink) {
        const std::string role =
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
            "\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";
        if (!sink.write(role.data(), role.size())) {
            return false;
        }
        const int chunks = drip_chunks_.load();
        const auto interval = std::chrono::milliseconds(drip_interval_ms_.load());
        for (int i = 0; i < chunks && !closing_.load(); ++i) {
            std::this_thread::sleep_for(interval);
            const std::string frame =
                "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
                "\"delta\":{\"content\":\"tok" +
                std::to_string(i) + " \"},\"finish_reason\":null}]}\n\n";
            if (!sink.write(frame.data(), frame.size())) {
                return false;  // the reader is gone; httplib closes the connection
            }
            ++dripped_;
        }
        const std::string tail =
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,"
            "\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],"
            "\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}\n\n"
            "data: [DONE]\n\n";
        if (!sink.write(tail.data(), tail.size())) {
            return false;
        }
        sink.done();
        return true;
    }

    int Record(const httplib::Request& request) {
        std::lock_guard<std::mutex> lock(mutex_);
        ports_.push_back(request.remote_port);
        ++arrivals_;
        arrived_.notify_all();
        return arrivals_;
    }

    void WaitForHold() {
        std::unique_lock<std::mutex> lock(mutex_);
        arrived_.wait_for(lock, std::chrono::seconds(5),
                          [this] { return arrivals_ >= hold_until_.load(); });
    }

    void WaitForHeaderHold() {
        std::unique_lock<std::mutex> lock(mutex_);
        arrived_.wait_for(lock, std::chrono::seconds(5), [this] { return !hold_headers_.load(); });
    }

    httplib::Server server_;
    std::thread thread_;
    int port_ = 0;
    mutable std::mutex mutex_;
    std::condition_variable arrived_;
    std::vector<int> ports_;
    std::vector<Cancel> cancels_;
    std::condition_variable cancelled_;
    int arrivals_ = 0;
    std::atomic<int> hold_until_{0};
    std::atomic<bool> hold_headers_{false};
    std::atomic<int> cancel_delay_ms_{0};
    std::atomic<bool> die_{false};
    std::atomic<int> drip_chunks_{0};
    std::atomic<int> drip_interval_ms_{5};
    std::atomic<int> dripped_{0};
    std::atomic<bool> closing_{false};
};

#if !defined(_WIN32)
/// An upstream that leaves a keep-alive connection HALF-OPEN: it answers the
/// first request on each connection and keeps the socket, then on the second
/// request of that connection reads it and closes without answering. That is
/// the stale keep-alive shape a client cannot detect before sending -- the
/// socket looks alive right up to the read that gets nothing back -- and the
/// one case RetryOnFreshConnection exists for. A server that closes with FIN
/// after answering would not do: httplib notices that before it sends.
///
/// Raw sockets because httplib's server has no way to drop a connection
/// without answering. POSIX only; the retry rule itself is covered on every
/// platform by the table test.
class HalfOpenUpstream {
   public:
    HalfOpenUpstream() {
        listen_fd_ = ::socket(AF_INET, SOCK_STREAM, 0);
        sockaddr_in address{};
        address.sin_family = AF_INET;
        address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        address.sin_port = 0;
        int one = 1;
        ::setsockopt(listen_fd_, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
        if (::bind(listen_fd_, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0 ||
            ::listen(listen_fd_, 8) != 0) {
            return;
        }
        socklen_t length = sizeof(address);
        ::getsockname(listen_fd_, reinterpret_cast<sockaddr*>(&address), &length);
        port_ = ntohs(address.sin_port);
        acceptor_ = std::thread([this] { Accept(); });
    }

    ~HalfOpenUpstream() {
        ::shutdown(listen_fd_, SHUT_RDWR);
        ::close(listen_fd_);
        if (acceptor_.joinable()) {
            acceptor_.join();
        }
        for (std::thread& t : handlers_) {
            if (t.joinable()) {
                t.join();
            }
        }
    }

    bool ok() const { return port_ > 0; }
    std::string base_url() const { return "http://127.0.0.1:" + std::to_string(port_) + "/v1"; }

    /// Peer port of every request read, answered or not, in arrival order.
    std::vector<int> ports() const {
        std::lock_guard<std::mutex> lock(mutex_);
        return ports_;
    }

   private:
    void Accept() {
        for (;;) {
            sockaddr_in peer{};
            socklen_t length = sizeof(peer);
            const int fd = ::accept(listen_fd_, reinterpret_cast<sockaddr*>(&peer), &length);
            if (fd < 0) {
                return;
            }
            const int port = ntohs(peer.sin_port);
            std::lock_guard<std::mutex> lock(mutex_);
            handlers_.emplace_back([this, fd, port] { Serve(fd, port); });
        }
    }

    // Reads one HTTP request (headers, then Content-Length bytes of body).
    static bool ReadRequest(int fd) {
        std::string buffer;
        char chunk[1024];
        size_t header_end = std::string::npos;
        while (header_end == std::string::npos) {
            const ssize_t n = ::recv(fd, chunk, sizeof(chunk), 0);
            if (n <= 0) {
                return false;
            }
            buffer.append(chunk, static_cast<size_t>(n));
            header_end = buffer.find("\r\n\r\n");
        }
        size_t content_length = 0;
        const size_t at = buffer.find("Content-Length:");
        if (at != std::string::npos) {
            content_length = static_cast<size_t>(std::atol(buffer.c_str() + at + 15));
        }
        size_t have = buffer.size() - (header_end + 4);
        while (have < content_length) {
            const ssize_t n = ::recv(fd, chunk, sizeof(chunk), 0);
            if (n <= 0) {
                return false;
            }
            have += static_cast<size_t>(n);
        }
        return true;
    }

    void Serve(int fd, int port) {
        int answered = 0;
        while (ReadRequest(fd)) {
            {
                std::lock_guard<std::mutex> lock(mutex_);
                ports_.push_back(port);
            }
            if (answered > 0) {
                // The half-open moment: the request was read, nothing comes
                // back, the connection just ends.
                break;
            }
            const std::string body = kJsonBody;
            const std::string response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                                         "Content-Length: " + std::to_string(body.size()) +
                                         "\r\nConnection: keep-alive\r\n\r\n" + body;
            ::send(fd, response.data(), response.size(), 0);
            ++answered;
        }
        ::close(fd);
    }

    int listen_fd_ = -1;
    int port_ = 0;
    std::thread acceptor_;
    mutable std::mutex mutex_;
    std::vector<std::thread> handlers_;
    std::vector<int> ports_;
};
#endif

std::string Describe(const std::vector<int>& ports) {
    std::string out = "[";
    for (size_t i = 0; i < ports.size(); ++i) {
        out += (i ? ", " : "") + std::to_string(ports[i]);
    }
    return out + "]";
}

}  // namespace wally_tests

#endif  // WALLY_TESTS_FAKE_UPSTREAM_H
