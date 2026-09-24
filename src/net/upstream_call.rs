//! A watched upstream POST that notices the editor leaving and cancels by name
//! (port of src/net/upstream_call.cpp). Owner: the upstream / shim port.
//!
//! The translators sit between an editor and the model endpoint. When the
//! editor abandons a stream -- Esc in Claude Code, the app quitting -- the
//! only thing the endpoint used to see was a TCP close, and only once the
//! next upstream chunk failed to write; during a long prefill nothing
//! arrives, so nothing failed, and the managed edge in front of the endpoint
//! loses the close anyway (measured 22-41s of paid decode after the client
//! was gone). Two things fix that, and this call does both:
//!
//!   1. It notices the editor leaving without waiting for a chunk: a watch
//!      thread polls the server connection's liveness -- a peek on the
//!      editor's socket -- every `poll` interval.
//!   2. It captures the response's `x-request-id` the moment the headers
//!      arrive, before any body byte, which is the name the endpoint's
//!      cancel route wants. On abandon it hands that id to `on_abandoned`
//!      -- once -- and then stops the upstream socket.
//!
//! The limit, stated: the endpoint's gateway opens the response only at the
//! first token, so the id is unknown during prefill. An abandon during
//! prefill therefore keeps the upstream open until the first token arrives
//! (or `id_wait` runs out, or the wrapper is stopping), cancels at that
//! moment -- which ends the decode, the long part -- and discards everything
//! after.

use crate::net::http1::{self, ResponseHead};
use crate::net::upstream_pool::UpstreamLease;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Body-chunk callback: see `WatchedCall::receiver`.
type ReceiverFn<'a> = Box<dyn FnMut(&[u8]) -> bool + 'a>;
/// Response-headers callback: see `WatchedCall::on_headers`.
type OnHeadersFn<'a> = Box<dyn FnMut(&ResponseHead) + 'a>;
/// Abandon-notification callback: see `WatchedCall::on_abandoned`.
type OnAbandonedFn<'a> = Box<dyn FnMut(&str, i32, bool) + Send + 'a>;

/// One upstream POST plus everything `post_watched` needs to watch it. All
/// callback fields are optional; a caller that does not need one leaves it
/// `None`.
pub struct WatchedCall<'a> {
    pub path: String,
    pub body: Vec<u8>,
    pub content_type: String,
    /// Called with each response body chunk. Returning false means the
    /// reader is gone -- a write to it failed -- and is the usual way a
    /// leave is noticed while tokens are flowing: the next chunk fails to
    /// deliver before the poll below gets its turn. Treated as an abandon
    /// (names the cancel, then aborts the request) rather than a plain stop
    /// nobody follows up.
    pub receiver: Option<ReceiverFn<'a>>,
    /// True when the reader this stream is for has hung up. Called from the
    /// watch thread only.
    pub reader_gone: Option<Box<dyn Fn() -> bool + Sync + 'a>>,
    /// The response's headers, the moment they arrive and before any body
    /// byte: the status and any Retry-After, which a streaming caller needs
    /// to decide whether to commit a 200 stream or answer the pre-stream
    /// failure instead. Must not block.
    pub on_headers: Option<OnHeadersFn<'a>>,
    /// True when the wrapper is shutting down: stop waiting for an id.
    /// Called from the watch thread only.
    pub stopping: Option<Box<dyn Fn() -> bool + Sync + 'a>>,
    /// At most once, the moment the reader is known to be gone AND the
    /// request id is known (or the wait for it ended). `request_id` is empty
    /// when the headers never came; `status` is the upstream status, 0 when
    /// unknown; `during_prefill` says the reader left before the headers
    /// arrived. Must not block.
    pub on_abandoned: Option<OnAbandonedFn<'a>>,
    /// How often the reader is checked. The watch wakes early when the call
    /// ends, so this is never added to a call that completes.
    pub poll: Duration,
    /// After an abandon, how long to keep the upstream open waiting for the
    /// headers that carry the id.
    pub id_wait: Duration,
}

impl<'a> Default for WatchedCall<'a> {
    fn default() -> Self {
        WatchedCall {
            path: String::new(),
            body: Vec::new(),
            content_type: "application/json".to_string(),
            receiver: None,
            reader_gone: None,
            on_headers: None,
            stopping: None,
            on_abandoned: None,
            poll: Duration::from_millis(100),
            id_wait: Duration::from_secs(120),
        }
    }
}

#[derive(Debug)]
pub struct WatchedResult {
    pub reply: Result<http1::Reply, http1::Error>,
    /// Whether any response body bytes reached `receiver`. Forbids a retry.
    pub received_any: bool,
    /// The reader left before the reply completed. Forbids a retry too: the
    /// stop that ended the call looks like a stale connection to the retry
    /// rule, and re-sending the prompt for a reader that is gone is the
    /// exact waste this call exists to end.
    pub abandoned: bool,
    /// The response's x-request-id, or empty if the headers never came.
    pub request_id: String,
    /// The upstream status, 0 if the headers never came.
    pub status: i32,
}

/// Everything protected by `WatchState::shared`'s lock, including the
/// `on_abandoned` callback itself: it is invoked from either the watch
/// thread or the calling thread, but never both at once, because it is only
/// ever called with this lock held.
struct Shared<'a> {
    headers_seen: bool,
    during_prefill: bool,
    notified: bool,
    request_id: String,
    status: i32,
    on_abandoned: Option<OnAbandonedFn<'a>>,
}

struct WatchState<'a> {
    shared: Mutex<Shared<'a>>,
    ended: Condvar,
    /// Read outside the lock by the hot receiver path, per chunk.
    abandoned: AtomicBool,
    /// Stored the instant send() returns, before any lock: a completed call
    /// must not be mistaken for an abandon by a poll that lands in the gap.
    done: AtomicBool,
}

impl<'a> WatchState<'a> {
    fn new(on_abandoned: Option<OnAbandonedFn<'a>>) -> Self {
        WatchState {
            shared: Mutex::new(Shared {
                headers_seen: false,
                during_prefill: false,
                notified: false,
                request_id: String::new(),
                status: 0,
                on_abandoned,
            }),
            ended: Condvar::new(),
            abandoned: AtomicBool::new(false),
            done: AtomicBool::new(false),
        }
    }
}

/// Records the abandon. Caller holds `shared`'s lock.
fn abandon_locked(shared: &mut Shared<'_>, state: &WatchState<'_>) {
    if state.abandoned.load(Ordering::SeqCst) {
        return;
    }
    state.abandoned.store(true, Ordering::SeqCst);
    shared.during_prefill = !shared.headers_seen;
}

/// Fires `on_abandoned` exactly once, for a request that ran something: a
/// refusal (status >= 400) ran nothing and is not worth a cancel, so it is
/// marked as handled without a call. Caller holds `shared`'s lock.
fn name_locked(shared: &mut Shared<'_>) {
    if shared.notified {
        return;
    }
    shared.notified = true;
    if shared.headers_seen && shared.status >= 400 {
        return;
    }
    let request_id = shared.request_id.clone();
    let status = shared.status;
    let during_prefill = shared.during_prefill;
    if let Some(cb) = shared.on_abandoned.as_mut() {
        cb(&request_id, status, during_prefill);
    }
}

/// Ends the watch loop and notifies it however `post_watched` leaves --
/// `client.send()`/a callback panicking included. Without this, a panic
/// would unwind past the point that sets `done`, leaving the watch thread
/// polling forever and `thread::scope`'s implicit join hanging with it.
struct EndWatch<'a, 'b> {
    state: &'b WatchState<'a>,
}

impl Drop for EndWatch<'_, '_> {
    fn drop(&mut self) {
        self.state.done.store(true, Ordering::SeqCst);
        // An empty lock/unlock, deliberately: it makes sure the watch
        // thread, which may be inside a locked wait, observes `done` before
        // being notified.
        drop(self.state.shared.lock().unwrap());
        self.state.ended.notify_all();
    }
}

fn watch_loop(
    state: &WatchState<'_>,
    stop_handle: &http1::StopHandle,
    reader_gone: Option<&(dyn Fn() -> bool + Sync)>,
    stopping: Option<&(dyn Fn() -> bool + Sync)>,
    poll: Duration,
    id_wait: Duration,
) {
    let mut abandoned_at: Option<Instant> = None;
    let mut guard = state.shared.lock().unwrap();
    while !state.done.load(Ordering::SeqCst) {
        let (g, _) = state
            .ended
            .wait_timeout_while(guard, poll, |_| !state.done.load(Ordering::SeqCst))
            .unwrap();
        guard = g;
        if state.done.load(Ordering::SeqCst) {
            break;
        }
        if !state.abandoned.load(Ordering::SeqCst) {
            // The peek is a syscall on the reader's socket; no need to hold
            // the response handler and the receiver up for it.
            drop(guard);
            let gone = reader_gone.map(|f| f()).unwrap_or(false);
            guard = state.shared.lock().unwrap();
            if !gone || state.done.load(Ordering::SeqCst) {
                continue;
            }
            abandon_locked(&mut guard, state);
            abandoned_at = Some(Instant::now());
        }
        // Abandoned. Can the cancel be named yet?
        let mut stop_now = false;
        let waited_out = abandoned_at
            .map(|t| t.elapsed() >= id_wait)
            .unwrap_or(false);
        let is_stopping = stopping.map(|f| f()).unwrap_or(false);
        if guard.headers_seen {
            name_locked(&mut guard);
            stop_now = true;
        } else if waited_out || is_stopping {
            // No id will come in time. Say so once, then drop the socket:
            // the TCP close is all that is left to send.
            name_locked(&mut guard);
            stop_now = true;
        }
        if stop_now {
            drop(guard);
            stop_handle.stop();
            guard = state.shared.lock().unwrap();
        }
    }
}

/// Sends `call` on `lease`'s client, watching the reader throughout. Returns
/// once the reply completed, failed, or was stopped; the lease is the
/// caller's to return or discard (an abandoned or failed call's socket is in
/// no state to reuse).
pub fn post_watched(lease: &mut UpstreamLease, call: WatchedCall<'_>) -> WatchedResult {
    let WatchedCall {
        path,
        body,
        content_type,
        mut receiver,
        reader_gone,
        mut on_headers,
        stopping,
        on_abandoned,
        poll,
        id_wait,
    } = call;

    let state = WatchState::new(on_abandoned);
    let stop_handle = lease.client().stop_handle();
    let request = http1::Request {
        method: "POST".to_string(),
        path,
        headers: vec![("Content-Type".to_string(), content_type)],
        body,
    };
    let has_receiver = receiver.is_some();

    let state_ref: &WatchState<'_> = &state;
    let (reply, received_any) = std::thread::scope(|scope| {
        let reader_gone_ref = reader_gone.as_deref();
        let stopping_ref = stopping.as_deref();
        let watch_stop_handle = stop_handle.clone();
        let watch = scope.spawn(move || {
            watch_loop(
                state_ref,
                &watch_stop_handle,
                reader_gone_ref,
                stopping_ref,
                poll,
                id_wait,
            );
        });
        let _end_watch = EndWatch { state: state_ref };

        let mut on_headers_hook = |head: &ResponseHead| -> bool {
            let mut shared = state.shared.lock().unwrap();
            shared.headers_seen = true;
            shared.request_id = head.header("x-request-id").unwrap_or("").to_string();
            shared.status = head.status;
            // Hand the headers to the caller before any body byte, so a
            // streaming caller can preserve a pre-stream status instead of a
            // blind 200.
            if let Some(cb) = on_headers.as_mut() {
                cb(head);
            }
            if state.abandoned.load(Ordering::SeqCst) {
                // The reader left during prefill and the id has just
                // arrived: this is the moment the cancel can be named.
                // Nothing that follows the headers is for anyone, so the
                // request is canceled here.
                name_locked(&mut shared);
                return false;
            }
            true
        };

        let mut received_any = false;
        let mut receiver_hook = |data: &[u8]| -> bool {
            if state.abandoned.load(Ordering::SeqCst) {
                // Discard: the sink belongs to a reader that is gone, and
                // the stop() the watch thread issued will end this read
                // shortly.
                return true;
            }
            received_any = true;
            let accepted = receiver.as_mut().map(|r| r(data)).unwrap_or(true);
            if accepted {
                return true;
            }
            // The reader refused the bytes: it is gone, and this is the
            // first anyone here hears of it.
            let mut shared = state.shared.lock().unwrap();
            abandon_locked(&mut shared, &state);
            name_locked(&mut shared);
            false
        };

        let reply = lease.client().send(
            &request,
            Some(&mut on_headers_hook),
            if has_receiver {
                Some(&mut receiver_hook)
            } else {
                None
            },
        );

        drop(_end_watch);
        watch.join().expect("upstream_call watch thread panicked");
        (reply, received_any)
    });

    let shared = state.shared.lock().unwrap();
    WatchedResult {
        reply,
        received_any,
        abandoned: state.abandoned.load(Ordering::SeqCst),
        request_id: shared.request_id.clone(),
        status: shared.status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::http1::Server;
    use crate::net::upstream_pool::{UpstreamOptions, UpstreamPool};
    use std::sync::Arc;

    fn options(origin: String) -> UpstreamOptions {
        UpstreamOptions {
            origin,
            read_timeout: Duration::from_secs(5),
            connect_timeout: Duration::from_secs(5),
            idle_limit: 4,
        }
    }

    #[test]
    fn happy_path_delivers_headers_and_body_without_abandoning() {
        let mut server = Server::new();
        server.route("POST", "/chat", |_req, res, _peer| {
            res.begin_chunked(200, &[("x-request-id", "req-1")])
                .unwrap();
            res.write_chunk(b"hello ").unwrap();
            res.write_chunk(b"world").unwrap();
            res.end_chunked().unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");

        let mut collected = Vec::new();
        let mut headers_status = 0;
        let call = WatchedCall {
            path: "/chat".to_string(),
            body: b"{}".to_vec(),
            receiver: Some(Box::new(|data: &[u8]| -> bool {
                collected.extend_from_slice(data);
                true
            })),
            on_headers: Some(Box::new(|head: &ResponseHead| {
                headers_status = head.status;
            })),
            ..Default::default()
        };
        let result = post_watched(&mut lease, call);

        assert!(
            result.reply.is_ok(),
            "expected a completed reply, got {:?}",
            result.reply
        );
        assert!(!result.abandoned);
        assert!(result.received_any);
        assert_eq!(result.request_id, "req-1");
        assert_eq!(result.status, 200);
        assert_eq!(headers_status, 200);
        assert_eq!(collected, b"hello world");
        handle.stop();
    }

    #[test]
    fn receiver_refusing_bytes_names_the_abandon_once() {
        let mut server = Server::new();
        server.route("POST", "/chat", |_req, res, _peer| {
            res.begin_chunked(200, &[("x-request-id", "req-2")])
                .unwrap();
            res.write_chunk(b"first").unwrap();
            // The reader will have refused by the time this is attempted;
            // the write may fail, which is fine -- there is nobody left to see it.
            let _ = res.write_chunk(b"second");
            let _ = res.end_chunked();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");

        let abandoned_calls = Arc::new(Mutex::new(Vec::new()));
        let record = abandoned_calls.clone();
        let call = WatchedCall {
            path: "/chat".to_string(),
            body: b"{}".to_vec(),
            receiver: Some(Box::new(|_data: &[u8]| -> bool { false })),
            on_abandoned: Some(Box::new(
                move |request_id: &str, status: i32, during_prefill: bool| {
                    record
                        .lock()
                        .unwrap()
                        .push((request_id.to_string(), status, during_prefill));
                },
            )),
            ..Default::default()
        };
        let result = post_watched(&mut lease, call);

        assert!(
            result.abandoned,
            "a refused chunk must mark the call abandoned"
        );
        assert!(
            result.received_any,
            "the first chunk did reach the receiver"
        );
        let calls = abandoned_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "on_abandoned must fire exactly once");
        assert_eq!(calls[0].0, "req-2");
        assert_eq!(calls[0].1, 200);
        assert!(
            !calls[0].2,
            "headers had already arrived: not a prefill abandon"
        );
        handle.stop();
    }

    #[test]
    fn dead_reader_during_prefill_stops_after_id_wait() {
        // The server never answers; reader_gone is true from the very start
        // (prefill), and id_wait is short, so the watch must stop the
        // upstream and name the abandon with an empty request id.
        let mut server = Server::new();
        server.route("POST", "/chat", |_req, _res, _peer| {
            std::thread::sleep(Duration::from_secs(30));
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");

        let abandoned_calls = Arc::new(Mutex::new(Vec::new()));
        let record = abandoned_calls.clone();
        let call = WatchedCall {
            path: "/chat".to_string(),
            body: b"{}".to_vec(),
            reader_gone: Some(Box::new(|| true)),
            on_abandoned: Some(Box::new(
                move |request_id: &str, status: i32, during_prefill: bool| {
                    record
                        .lock()
                        .unwrap()
                        .push((request_id.to_string(), status, during_prefill));
                },
            )),
            poll: Duration::from_millis(20),
            id_wait: Duration::from_millis(100),
            ..Default::default()
        };
        let result = post_watched(&mut lease, call);

        assert!(result.abandoned);
        assert!(!result.received_any);
        let calls = abandoned_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "", "no headers ever arrived");
        assert!(
            calls[0].2,
            "the reader left before headers arrived: a prefill abandon"
        );
        handle.stop();
    }

    #[test]
    fn a_refusal_status_skips_on_abandoned() {
        // Headers carry a >=400 status; a reader leaving after that ran
        // nothing worth cancelling, so on_abandoned must not fire even
        // though the call is still marked abandoned.
        let mut server = Server::new();
        server.route("POST", "/chat", |_req, res, _peer| {
            res.send_full(429, &[("x-request-id", "req-3")], b"{}")
                .unwrap();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");

        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        let call = WatchedCall {
            path: "/chat".to_string(),
            body: b"{}".to_vec(),
            reader_gone: Some(Box::new(|| true)),
            on_abandoned: Some(Box::new(move |_id: &str, _status: i32, _prefill: bool| {
                flag.store(true, Ordering::SeqCst);
            })),
            poll: Duration::from_millis(20),
            ..Default::default()
        };
        let _ = post_watched(&mut lease, call);

        assert!(
            !fired.load(Ordering::SeqCst),
            "a >=400 refusal must not fire on_abandoned"
        );
        handle.stop();
    }

    #[test]
    fn a_panicking_receiver_does_not_leak_the_watch_thread() {
        // The watch thread must still be joined (via EndWatch's Drop) when
        // the receiver panics mid-call; if it were not, this test would hang
        // instead of returning promptly.
        let mut server = Server::new();
        server.route("POST", "/chat", |_req, res, _peer| {
            res.begin_chunked(200, &[]).unwrap();
            res.write_chunk(b"boom").unwrap();
            let _ = res.end_chunked();
        });
        let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
        let pool = UpstreamPool::new(options(format!("http://127.0.0.1:{port}")));
        let mut lease = pool.acquire("token");

        let call = WatchedCall {
            path: "/chat".to_string(),
            body: b"{}".to_vec(),
            receiver: Some(Box::new(|_data: &[u8]| -> bool {
                panic!("receiver exploded")
            })),
            ..Default::default()
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            post_watched(&mut lease, call)
        }));
        assert!(
            outcome.is_err(),
            "expected the panic to propagate out of post_watched"
        );
        handle.stop();
    }
}
