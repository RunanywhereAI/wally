#include "net/upstream_pool.h"

#include <utility>

namespace wally::net {

UpstreamLease::UpstreamLease(std::shared_ptr<UpstreamPool> pool,
                             std::unique_ptr<httplib::Client> client, bool reused)
    : pool_(std::move(pool)), client_(std::move(client)), reused_(reused) {}

UpstreamLease::UpstreamLease(UpstreamLease&& other) noexcept
    : pool_(std::move(other.pool_)),
      client_(std::move(other.client_)),
      reused_(other.reused_),
      discard_(other.discard_) {}

UpstreamLease& UpstreamLease::operator=(UpstreamLease&& other) noexcept {
    if (this != &other) {
        pool_ = std::move(other.pool_);
        client_ = std::move(other.client_);
        reused_ = other.reused_;
        discard_ = other.discard_;
    }
    return *this;
}

UpstreamLease::~UpstreamLease() {
    if (pool_ && client_ && !discard_) {
        pool_->give_back(std::move(client_));
    }
    // A discarded client is destroyed here, which closes its socket.
}

UpstreamPool::UpstreamPool(UpstreamOptions options) : options_(std::move(options)) {}

std::unique_ptr<httplib::Client> UpstreamPool::build() const {
    auto client = std::make_unique<httplib::Client>(options_.origin);
    client->set_keep_alive(true);
    client->set_read_timeout(options_.read_timeout_seconds, 0);
    client->set_connection_timeout(options_.connect_timeout_seconds, 0);
    return client;
}

UpstreamLease UpstreamPool::acquire(const std::string& bearer) {
    std::unique_ptr<httplib::Client> client;
    bool reused = false;
    {
        std::lock_guard<std::mutex> lock(mutex_);
        if (!idle_.empty()) {
            // The most recently returned client: its connection is the one
            // least likely to have been closed by an idle timeout.
            client = std::move(idle_.back());
            idle_.pop_back();
            reused = true;
        }
    }
    if (!client) {
        client = build();
    }
    // Set on every lease, never baked in: the JetBrains proxy renews its
    // token on a 401 and retries with the new one on the same pool.
    client->set_bearer_token_auth(bearer);
    return UpstreamLease(shared_from_this(), std::move(client), reused);
}

void UpstreamPool::give_back(std::unique_ptr<httplib::Client> client) {
    std::lock_guard<std::mutex> lock(mutex_);
    if (idle_.size() < options_.idle_limit) {
        idle_.push_back(std::move(client));
    }
    // Past the limit the client is destroyed on return, closing its socket.
}

size_t UpstreamPool::idle() const {
    std::lock_guard<std::mutex> lock(mutex_);
    return idle_.size();
}

bool RetryOnFreshConnection(httplib::Error error, bool has_response, bool received_any,
                            bool reused) {
    if (!reused || has_response || received_any) {
        return false;
    }
    switch (error) {
        case httplib::Error::Connection:
        case httplib::Error::ConnectionClosed:
        case httplib::Error::Read:
        case httplib::Error::Write:
        case httplib::Error::SSLConnection:
            return true;
        default:
            // Timeouts are not stale connections: a read timeout after 600 s
            // is a slow or hung upstream, and a connect timeout is the
            // network, and neither should be paid twice.
            return false;
    }
}

}  // namespace wally::net
