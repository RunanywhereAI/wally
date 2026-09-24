//! Exclusive client leases to the hosted upstream (port of
//! src/net/upstream_pool.cpp). Owner: the upstream / shim port.
//!
//! A fresh TCP + TLS handshake to a hosted endpoint measures around 541ms;
//! paying that on every request would dominate latency. `httplib::Client`
//! serializes requests on one socket, so the pool cannot hand out a single
//! shared client either -- it hands out exclusive, keep-alive connections
//! and takes them back when the caller is done.

use crate::net::http1::{self, Client};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a pooled client is built and how long its idle connections are kept.
#[derive(Debug, Clone)]
pub struct UpstreamOptions {
    /// Scheme and authority only, e.g. `https://inference.runanywhere.ai`.
    /// Any path prefix (e.g. `/api-dev`) is the caller's job to prepend to
    /// each request path -- the pool only owns the connection.
    pub origin: String,
    pub read_timeout: Duration,
    pub connect_timeout: Duration,
    /// How many idle connections `give_back` will keep for reuse; the rest
    /// are simply dropped (closing the socket) when returned.
    pub idle_limit: usize,
}

impl Default for UpstreamOptions {
    fn default() -> Self {
        UpstreamOptions {
            origin: String::new(),
            read_timeout: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(10),
            idle_limit: 4,
        }
    }
}

struct Inner {
    options: UpstreamOptions,
    idle: Mutex<Vec<Client>>,
}

/// Hands out exclusive, keep-alive connections to one upstream origin and
/// takes them back when a caller is done with them.
pub struct UpstreamPool {
    inner: Inner,
}

impl UpstreamPool {
    pub fn new(options: UpstreamOptions) -> Arc<UpstreamPool> {
        Arc::new(UpstreamPool {
            inner: Inner {
                options,
                idle: Mutex::new(Vec::new()),
            },
        })
    }

    fn build(&self) -> Client {
        // Origin is validated by the caller (it comes from a pinned
        // Endpoint); a malformed one here would be a configuration bug, not
        // a request-time failure, so this only needs to not panic.
        Client::new(
            &self.inner.options.origin,
            self.inner.options.connect_timeout,
            self.inner.options.read_timeout,
        )
        .unwrap_or_else(|_| {
            // Fall back to a client that will fail on first use rather
            // than panic; `http://` always parses.
            Client::new(
                "http://127.0.0.1:1",
                self.inner.options.connect_timeout,
                self.inner.options.read_timeout,
            )
            .expect("http://127.0.0.1:1 always parses as an origin")
        })
    }

    /// Hands back an exclusive lease. Reuses the most recently returned idle
    /// connection when one is available (LIFO -- most likely to still be
    /// warm), else builds a fresh one. Either way the bearer token is set on
    /// the client: never baked in once, since a translator can renew its
    /// token and retry with the new one on the same pool.
    pub fn acquire(self: &Arc<Self>, bearer: &str) -> UpstreamLease {
        let (mut client, reused) = {
            let mut idle = self.inner.idle.lock().unwrap();
            match idle.pop() {
                Some(c) => (c, true),
                None => (self.build(), false),
            }
        };
        client.set_bearer_token(bearer);
        UpstreamLease {
            pool: Some(self.clone()),
            client: Some(client),
            reused,
            discard: false,
        }
    }

    fn give_back(&self, client: Client) {
        let mut idle = self.inner.idle.lock().unwrap();
        if idle.len() < self.inner.options.idle_limit {
            idle.push(client);
        }
        // Else: `client` drops here, closing its socket.
    }

    pub fn idle(&self) -> usize {
        self.inner.idle.lock().unwrap().len()
    }

    pub fn origin(&self) -> &str {
        &self.inner.options.origin
    }
}

/// One exclusive connection, checked out of the pool. Goes back to the idle
/// stack on drop unless `discard()` was called (a connection known to be in
/// a bad state must not be reused).
pub struct UpstreamLease {
    pool: Option<Arc<UpstreamPool>>,
    client: Option<Client>,
    reused: bool,
    discard: bool,
}

impl UpstreamLease {
    pub fn reused(&self) -> bool {
        self.reused
    }

    pub fn discard(&mut self) {
        self.discard = true;
    }

    pub fn client(&mut self) -> &mut Client {
        self.client
            .as_mut()
            .expect("UpstreamLease used after being taken apart")
    }
}

impl Drop for UpstreamLease {
    fn drop(&mut self) {
        if let (Some(pool), Some(client)) = (self.pool.take(), self.client.take()) {
            if !self.discard {
                pool.give_back(client);
            }
            // Else: `client` drops here, closing its socket.
        }
    }
}

/// Whether a failed call should be retried once, on a brand-new connection,
/// without paying for it in every other case. Retrying only makes sense when
/// ALL of these hold:
/// - the connection was reused (a fresh one failing is a real outage, not
///   staleness -- nothing to retry against);
/// - no response was ever seen (a partial reply must not be replayed);
/// - the error is connection-class (`Connection`/`ConnectionClosed`/
///   `Read`/`Write`/`SslConnection`).
///
/// Deliberately NOT retried even when reused: timeouts and cancellation.
/// A 600s read timeout means a slow or hung upstream, not a stale socket;
/// a connect timeout means the network is down. Neither should be paid
/// twice, and a caller-initiated cancel must never be turned into a retry.
pub fn retry_on_fresh_connection(
    error: http1::Error,
    has_response: bool,
    received_any: bool,
    reused: bool,
) -> bool {
    if !reused || has_response || received_any {
        return false;
    }
    matches!(
        error,
        http1::Error::Connection
            | http1::Error::ConnectionClosed
            | http1::Error::Read
            | http1::Error::Write
            | http1::Error::SslConnection
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::http1::{Request, Server};

    fn options(origin: String) -> UpstreamOptions {
        UpstreamOptions {
            origin,
            read_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            idle_limit: 4,
        }
    }

    #[test]
    fn acquire_builds_fresh_when_idle_is_empty() {
        let mut server = Server::new();
        server.route("GET", "/ping", |_req, res, _peer| {
            res.send_full(200, &[], b"pong").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token-a");
        assert!(
            !lease.reused(),
            "the very first lease must not be marked reused"
        );
        let reply = lease
            .client()
            .send(&Request::get("/ping"), None, None)
            .unwrap();
        assert_eq!(reply.status, 200);
        drop(lease);

        assert_eq!(pool.idle(), 1, "a clean lease must go back to idle on drop");
        let lease2 = pool.acquire("token-b");
        assert!(
            lease2.reused(),
            "a second acquire must reuse the idle connection"
        );
        drop(lease2);
        handle.stop();
    }

    #[test]
    fn discarded_lease_is_not_returned_to_idle() {
        let mut server = Server::new();
        server.route("GET", "/ping", |_req, res, _peer| {
            res.send_full(200, &[], b"pong").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");
        let _ = lease
            .client()
            .send(&Request::get("/ping"), None, None)
            .unwrap();
        lease.discard();
        drop(lease);

        assert_eq!(pool.idle(), 0, "a discarded lease must not be reused");
        handle.stop();
    }

    #[test]
    fn idle_limit_caps_how_many_connections_are_kept() {
        let mut server = Server::new();
        server.route("GET", "/ping", |_req, res, _peer| {
            res.send_full(200, &[], b"pong").unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

        let mut opts = options(format!("http://127.0.0.1:{port}"));
        opts.idle_limit = 1;
        let pool = UpstreamPool::new(opts);

        let mut a = pool.acquire("t");
        let _ = a.client().send(&Request::get("/ping"), None, None).unwrap();
        let mut b = pool.acquire("t");
        let _ = b.client().send(&Request::get("/ping"), None, None).unwrap();

        drop(a);
        drop(b);
        assert_eq!(
            pool.idle(),
            1,
            "idle_limit=1 must cap the idle stack at one connection"
        );
        handle.stop();
    }

    #[test]
    fn retry_only_fires_for_a_reused_no_response_connection_error() {
        assert!(retry_on_fresh_connection(
            http1::Error::ConnectionClosed,
            false,
            false,
            true
        ));
        assert!(retry_on_fresh_connection(
            http1::Error::Write,
            false,
            false,
            true
        ));
        assert!(
            !retry_on_fresh_connection(http1::Error::ConnectionClosed, false, false, false),
            "fresh connections are never retried"
        );
        assert!(
            !retry_on_fresh_connection(http1::Error::ConnectionClosed, true, false, true),
            "a seen response must not be replayed"
        );
        assert!(
            !retry_on_fresh_connection(http1::Error::ConnectionClosed, false, true, true),
            "received body bytes must not be replayed"
        );
        assert!(
            !retry_on_fresh_connection(http1::Error::Canceled, false, false, true),
            "cancellation is never a stale-connection retry"
        );
        assert!(
            !retry_on_fresh_connection(http1::Error::InvalidResponse, false, false, true),
            "a malformed response is not a staleness signal"
        );
    }
}
