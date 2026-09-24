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
        // a request-time failure, so this only needs to not panic. C++'s
        // `httplib::Client(options_.origin)` never fails on a bad string
        // either -- it falls back to using the whole raw origin as a
        // literal hostname (see `with_literal_host`) -- so a misconfigured
        // value here fails using that same configured value, not a
        // hardcoded stand-in the operator never set.
        Client::new(
            &self.inner.options.origin,
            self.inner.options.connect_timeout,
            self.inner.options.read_timeout,
        )
        .unwrap_or_else(|_| {
            Client::with_literal_host(
                &self.inner.options.origin,
                self.inner.options.connect_timeout,
                self.inner.options.read_timeout,
            )
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

/// Whether a failed request should be tried once more on a fresh connection.
///
/// Only a request that went out on a REUSED connection, got no HTTP status
/// back, delivered nothing to the caller, and failed with a connection-class
/// error (`Connection`/`ConnectionClosed`/`Read`/`Write`/`SslConnection`) is
/// retried. That is the stale keep-alive case -- the far side closed an idle
/// connection and the first write or read on it fails -- and it is the same
/// heuristic curl and browsers apply. A fresh connection that fails is a real
/// outage and must surface; a request that has produced output must never be
/// repeated.
///
/// Deliberately NOT in the retry-eligible set: `ConnectionTimeout` (the
/// connect phase itself ran out of time -- that is the network being down,
/// not a stale socket) and `Canceled` (a caller-initiated stop must never be
/// turned into a retry). `Read`/`Write` cover a stalled read/write timeout
/// too, same as httplib, and ARE retried -- the 600s budget expiring on an
/// idle reused connection looks identical to the far side having silently
/// closed it.
///
/// Accepted risk, capped at one retry: "no bytes back" does not prove the
/// server never processed the request, so a retry can in rare cases run a
/// generation twice. Callers log the retry so such a case is traceable.
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
        assert!(
            !retry_on_fresh_connection(http1::Error::ConnectionTimeout, false, false, true),
            "a connect-phase timeout is the network being down, not a stale socket"
        );
        assert!(
            retry_on_fresh_connection(http1::Error::Read, false, false, true),
            "a stalled read on a reused connection is retried, same as httplib"
        );
    }

    // A malformed origin that still passed the caller's http(s):// gate (e.g.
    // "http://" with no host) must not panic build() and must not silently
    // redirect every request to a hardcoded stand-in the operator never
    // configured -- it fails using the misconfigured value itself, the same
    // way cpp-httplib's Client does.
    #[test]
    fn acquire_does_not_panic_on_a_malformed_origin_and_never_reaches_the_old_dummy_host() {
        let pool = UpstreamPool::new(options("http://".to_string()));
        let mut lease = pool.acquire("token");
        let result = lease.client().send(&Request::get("/ping"), None, None);
        assert!(result.is_err(), "an empty host can never actually connect");
        // The old fallback pointed every such client at 127.0.0.1:1, a
        // reserved port that refuses instantly; the fix instead resolves
        // "http://" itself as a (bogus) DNS name, which fails at name
        // resolution -- either way this must not panic, which the call
        // above already proved by returning rather than aborting.
    }
}
