#ifndef WALLY_NET_UPSTREAM_POOL_H
#define WALLY_NET_UPSTREAM_POOL_H

#include <memory>
#include <mutex>
#include <string>
#include <vector>

#include <httplib.h>

/// Reusable connections to one upstream origin.
///
/// The translators that sit between an editor and a model endpoint used to
/// build an `httplib::Client` per request, which opens a TCP connection and
/// completes a TLS handshake every time — measured at ~541 ms against the
/// hosted endpoint, paid before a byte of the request is sent, on every turn
/// (wally #80). A pool keeps clients with keep-alive on and lends them out.
///
/// Why a pool and not one shared client: an `httplib::Client` serialises
/// requests on its single socket, so one shared client would queue an
/// editor's parallel calls behind a stream that lasts minutes. Each request
/// in flight gets its own client; idle ones are kept for the next request.
///
/// A lease goes back to the pool only when its request completed cleanly. A
/// stream the editor abandoned, or a transport error, leaves the socket in a
/// state nothing should reuse, so those leases are discarded and the socket
/// closed with them.
namespace wally::net {

struct UpstreamOptions {
    /// "https://host" or "http://127.0.0.1:port" — scheme and authority only.
    std::string origin;
    /// How long a single read may block. Long prefills are not hangs.
    int read_timeout_seconds = 600;
    /// How long a TCP connect may take. httplib's default is 300 s, which
    /// leaves an editor's request hanging for five minutes when a connect is
    /// black-holed (a laptop changing networks, a VPN flipping); ten seconds
    /// is twenty times the worst handshake measured against the endpoint.
    int connect_timeout_seconds = 10;
    /// Idle clients kept for reuse. Leases beyond this are still created; a
    /// returned lease past the limit is closed instead of kept.
    size_t idle_limit = 4;
};

class UpstreamPool;

/// One client checked out of a pool. Movable, not copyable. Returned to the
/// pool on destruction unless `discard()` was called.
class UpstreamLease {
   public:
    UpstreamLease(UpstreamLease&& other) noexcept;
    UpstreamLease& operator=(UpstreamLease&& other) noexcept;
    UpstreamLease(const UpstreamLease&) = delete;
    UpstreamLease& operator=(const UpstreamLease&) = delete;
    ~UpstreamLease();

    httplib::Client& client() { return *client_; }

    /// True when this client came from the idle set and may be holding a
    /// connection the far side has since closed. The retry rule reads this.
    bool reused() const { return reused_; }

    /// Do not return this client to the pool; its socket is closed with it.
    void discard() { discard_ = true; }

   private:
    friend class UpstreamPool;
    UpstreamLease(std::shared_ptr<UpstreamPool> pool, std::unique_ptr<httplib::Client> client,
                  bool reused);

    std::shared_ptr<UpstreamPool> pool_;
    std::unique_ptr<httplib::Client> client_;
    bool reused_ = false;
    bool discard_ = false;
};

/// Must be owned by a `std::shared_ptr`: leases hold a reference so a pool
/// outlives every request still using one of its clients, even after the
/// translator that created it has stopped.
class UpstreamPool : public std::enable_shared_from_this<UpstreamPool> {
   public:
    explicit UpstreamPool(UpstreamOptions options);

    /// A client for one request, with `bearer` set as its Authorization
    /// (none when empty). Reuses an idle client when there is one.
    UpstreamLease acquire(const std::string& bearer);

    /// Idle clients currently held. For tests and the verbose trace.
    size_t idle() const;

    const UpstreamOptions& options() const { return options_; }

   private:
    friend class UpstreamLease;
    void give_back(std::unique_ptr<httplib::Client> client);
    std::unique_ptr<httplib::Client> build() const;

    UpstreamOptions options_;
    mutable std::mutex mutex_;
    std::vector<std::unique_ptr<httplib::Client>> idle_;
};

/// Whether a failed request should be tried once more on a fresh connection.
///
/// Only a request that went out on a REUSED connection, got no HTTP status
/// back, delivered nothing to the caller, and failed with a connection-class
/// error is retried. That is the stale keep-alive case — the far side closed
/// an idle connection and the first write or read on it fails — and it is the
/// same heuristic curl and browsers apply. A fresh connection that fails is a
/// real outage and must surface; a request that has produced output must
/// never be repeated.
///
/// Accepted risk, capped at one retry: "no bytes back" does not prove the
/// server never processed the request, so a retry can in rare cases run a
/// generation twice. Callers log the retry so such a case is traceable.
bool RetryOnFreshConnection(httplib::Error error, bool has_response, bool received_any,
                            bool reused);

}  // namespace wally::net

#endif  // WALLY_NET_UPSTREAM_POOL_H
