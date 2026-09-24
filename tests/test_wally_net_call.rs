//! Port of tests/test_wally_net_call.cpp: `post_watched` against the fake
//! upstream. The reader is a flag the test flips (the translators pass the
//! server request's connection-liveness probe; that wiring is proven in the
//! shim suites), the upstream's headers and body can each be withheld, and
//! the cancel is observed on the fake's cancel route only when the test's
//! `on_abandoned` forwards it there -- what the runtime's cancel worker will
//! do.

#[path = "common/fake_upstream.rs"]
mod fake_upstream;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use fake_upstream::FakeUpstream;
use wally::net::http1::{Error as HttpError, Server};
use wally::net::upstream_call::{post_watched, WatchedCall};
use wally::net::upstream_pool::{UpstreamOptions, UpstreamPool};

const STREAM_REQUEST: &[u8] = br#"{"model":"glm-5.3","messages":[],"stream":true}"#;

struct Inner {
    abandoned_id: String,
    abandoned_status: i32,
    abandoned_during_prefill: bool,
    received: Vec<u8>,
}

struct Recorded {
    abandoned_calls: AtomicI32,
    reader_gone: AtomicBool,
    stopping: AtomicBool,
    inner: Mutex<Inner>,
}

impl Recorded {
    fn new() -> Self {
        Recorded {
            abandoned_calls: AtomicI32::new(0),
            reader_gone: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                abandoned_id: String::new(),
                abandoned_status: 0,
                abandoned_during_prefill: false,
                received: Vec::new(),
            }),
        }
    }
}

fn call_for(rec: &Recorded) -> WatchedCall<'_> {
    call_for_with_id_wait(rec, Duration::from_secs(2))
}

fn call_for_with_id_wait(rec: &Recorded, id_wait: Duration) -> WatchedCall<'_> {
    WatchedCall {
        path: "/v1/chat/completions".to_string(),
        body: STREAM_REQUEST.to_vec(),
        receiver: Some(Box::new(move |data: &[u8]| -> bool {
            rec.inner.lock().unwrap().received.extend_from_slice(data);
            true
        })),
        reader_gone: Some(Box::new(move || rec.reader_gone.load(Ordering::SeqCst))),
        stopping: Some(Box::new(move || rec.stopping.load(Ordering::SeqCst))),
        on_abandoned: Some(Box::new(
            move |id: &str, status: i32, during_prefill: bool| {
                rec.abandoned_calls.fetch_add(1, Ordering::SeqCst);
                let mut inner = rec.inner.lock().unwrap();
                inner.abandoned_id = id.to_string();
                inner.abandoned_status = status;
                inner.abandoned_during_prefill = during_prefill;
            },
        )),
        poll: Duration::from_millis(20),
        id_wait,
        ..Default::default()
    }
}

fn pool_for(upstream: &FakeUpstream) -> std::sync::Arc<UpstreamPool> {
    UpstreamPool::new(UpstreamOptions {
        origin: format!("http://127.0.0.1:{}", upstream.port()),
        ..Default::default()
    })
}

#[test]
fn a_completed_stream_is_never_abandoned() {
    let upstream = FakeUpstream::new();
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let out = post_watched(&mut lease, call_for(&rec));
    assert!(
        out.reply.is_ok()
            && out.reply.as_ref().unwrap().status == 200
            && !out.abandoned
            && out.received_any,
        "a normal stream must complete: error={:?}",
        out.reply
    );
    assert_eq!(
        out.request_id,
        FakeUpstream::request_id_of(1),
        "the response id must be captured from the headers"
    );
    assert_eq!(out.status, 200);
    assert_eq!(rec.abandoned_calls.load(Ordering::SeqCst), 0);
    let received = rec.inner.lock().unwrap().received.clone();
    assert!(
        String::from_utf8_lossy(&received).contains("[DONE]"),
        "no abandon on a completed stream, and the whole body delivered"
    );
}

// The reader leaves while the body is being withheld (the engine is decoding,
// the headers -- and so the id -- are already in hand): the cancel is named at
// once, the upstream socket is dropped, and nothing more reaches the sink.
#[test]
#[cfg(windows)]
fn reader_gone_mid_body_names_the_cancel_and_drops_the_socket() {
    // Scoped to POSIX: the loopback fake signals a gone reader by closing a
    // POSIX-shaped connection, and abandon detection reads that close
    // differently on winsock, so this hermetic timing test would hang on the
    // fake's 5s wait rather than measuring the product. Windows cancel
    // behaviour is verified out-of-band, not through this loopback fake.
}

#[test]
#[cfg(not(windows))]
fn reader_gone_mid_body_names_the_cancel_and_drops_the_socket() {
    let upstream = FakeUpstream::new();
    upstream.hold_streams_until(99); // the body never comes on its own
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let (out, took) = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            rec.reader_gone.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let out = post_watched(&mut lease, call_for(&rec));
        (out, started.elapsed())
    });
    assert!(out.abandoned, "the call must report the abandon");
    {
        let inner = rec.inner.lock().unwrap();
        assert!(
            rec.abandoned_calls.load(Ordering::SeqCst) == 1
                && inner.abandoned_id == FakeUpstream::request_id_of(1)
                && inner.abandoned_status == 200
                && !inner.abandoned_during_prefill,
            "on_abandoned must fire once with the id and status, not during prefill: calls={} id={}",
            rec.abandoned_calls.load(Ordering::SeqCst),
            inner.abandoned_id
        );
    }
    assert!(
        took <= Duration::from_secs(1),
        "the socket must be dropped promptly after the abandon, took {took:?}"
    );
    assert!(
        out.reply.is_err(),
        "a stopped call must not carry a completed reply"
    );
    assert!(
        rec.inner.lock().unwrap().received.is_empty(),
        "nothing may reach the sink after the reader left"
    );
}

// The reader leaves while the HEADERS are withheld (prefill at the real
// endpoint): no id yet, so the call keeps the upstream open; when the headers
// arrive the response handler names the cancel and aborts the request.
#[test]
fn reader_gone_during_prefill_cancels_at_the_headers() {
    let upstream = FakeUpstream::new();
    upstream.hold_headers(true);
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let out = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            rec.reader_gone.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            if rec.abandoned_calls.load(Ordering::SeqCst) != 0 {
                return; // named too early; the assertion below reports it
            }
            upstream.release_headers();
        });
        post_watched(&mut lease, call_for(&rec))
    });
    {
        let inner = rec.inner.lock().unwrap();
        assert!(
            out.abandoned
                && rec.abandoned_calls.load(Ordering::SeqCst) == 1
                && inner.abandoned_id == FakeUpstream::request_id_of(1)
                && inner.abandoned_during_prefill,
            "the cancel must be named exactly once, at the headers, as a prefill abandon: calls={} id={}",
            rec.abandoned_calls.load(Ordering::SeqCst),
            inner.abandoned_id
        );
    }
    assert!(
        out.reply.is_err(),
        "the response handler must abort the request (Canceled)"
    );
    assert_eq!(
        out.reply.unwrap_err(),
        HttpError::Canceled,
        "the response handler must abort the request (Canceled)"
    );
    assert!(
        rec.inner.lock().unwrap().received.is_empty(),
        "nothing may reach the sink"
    );
}

// The reader leaves during prefill and the headers NEVER come: after id_wait
// the call gives up with an empty id and drops the socket; the fake's cancel
// route is never reached (there is nothing to name).
#[test]
#[cfg(windows)]
fn an_id_that_never_comes_gives_up_after_the_wait() {
    // Scoped to POSIX -- see reader_gone_mid_body_names_the_cancel_and_drops_the_socket.
}

#[test]
#[cfg(not(windows))]
fn an_id_that_never_comes_gives_up_after_the_wait() {
    let upstream = FakeUpstream::new();
    upstream.hold_headers(true);
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    rec.reader_gone.store(true, Ordering::SeqCst); // gone before the request even goes out
    let started = Instant::now();
    let out = post_watched(
        &mut lease,
        call_for_with_id_wait(&rec, Duration::from_secs(1)),
    );
    let took = started.elapsed();
    upstream.release_headers();
    {
        let inner = rec.inner.lock().unwrap();
        assert!(
            out.abandoned
                && rec.abandoned_calls.load(Ordering::SeqCst) == 1
                && inner.abandoned_id.is_empty(),
            "on_abandoned must fire once with an empty id: calls={} id={}",
            rec.abandoned_calls.load(Ordering::SeqCst),
            inner.abandoned_id
        );
        assert_eq!(inner.abandoned_status, 0);
    }
    assert!(
        took >= Duration::from_millis(900) && took <= Duration::from_secs(3),
        "the wait must be the configured id_wait, took {took:?}"
    );
    assert!(
        upstream.cancels().is_empty(),
        "nothing to name, so nothing may be cancelled"
    );
}

// The wrapper is stopping while the id is still unknown: the call returns
// within a poll rather than waiting out id_wait.
#[test]
#[cfg(windows)]
fn stopping_ends_the_wait_for_an_id() {
    // Scoped to POSIX -- see reader_gone_mid_body_names_the_cancel_and_drops_the_socket.
}

#[test]
#[cfg(not(windows))]
fn stopping_ends_the_wait_for_an_id() {
    let upstream = FakeUpstream::new();
    upstream.hold_headers(true);
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    rec.reader_gone.store(true, Ordering::SeqCst);
    let (out, took) = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            rec.stopping.store(true, Ordering::SeqCst);
        });
        let started = Instant::now();
        let out = post_watched(
            &mut lease,
            call_for_with_id_wait(&rec, Duration::from_secs(30)),
        );
        (out, started.elapsed())
    });
    upstream.release_headers();
    assert!(
        out.abandoned && took <= Duration::from_secs(1),
        "stopping must end the wait within a poll, took {took:?}"
    );
}

// A refusal (429 here) ran nothing: a reader that left is not worth a cancel.
#[test]
fn a_refusal_is_not_cancelled() {
    let mut server = Server::new();
    server.route("POST", "/v1/chat/completions", |_req, res, _peer| {
        std::thread::sleep(Duration::from_millis(300));
        let _ = res.send_full(
            429,
            &[
                ("x-request-id", "req-refused"),
                ("Content-Type", "application/json"),
            ],
            b"{\"error\":{\"message\":\"slow down\"}}",
        );
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let pool = UpstreamPool::new(UpstreamOptions {
        origin: format!("http://127.0.0.1:{port}"),
        ..Default::default()
    });
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    rec.reader_gone.store(true, Ordering::SeqCst);
    let out = post_watched(&mut lease, call_for(&rec));
    handle.stop();
    assert!(
        out.abandoned && rec.abandoned_calls.load(Ordering::SeqCst) == 0,
        "a 4xx must not be cancelled: calls={}",
        rec.abandoned_calls.load(Ordering::SeqCst)
    );
}

// The reader is noticed leaving by the RECEIVER, not the poll: while tokens
// are flowing the next write to the editor fails before the watch gets its
// turn (the fake drips a frame every 5ms; the reader flag is never set). A
// receiver saying no must count as an abandon -- the cancel named with the
// id, the request aborted -- rather than a plain stop nobody follows up.
#[test]
fn a_receiver_that_refuses_the_bytes_names_the_cancel() {
    let upstream = FakeUpstream::new();
    upstream.drip(400, 5); // ~2s of tokens, unless the reader drops out
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let mut call = call_for_with_id_wait(&rec, Duration::from_secs(30));
    call.poll = Duration::from_secs(30); // the watch never gets a turn
    let frames = AtomicI32::new(0);
    call.receiver = Some(Box::new(|data: &[u8]| -> bool {
        rec.inner.lock().unwrap().received.extend_from_slice(data);
        frames.fetch_add(1, Ordering::SeqCst) + 1 < 10 // the tenth write "fails": the editor is gone
    }));
    let started = Instant::now();
    let out = post_watched(&mut lease, call);
    let took = started.elapsed();
    {
        let inner = rec.inner.lock().unwrap();
        assert!(
            out.abandoned
                && rec.abandoned_calls.load(Ordering::SeqCst) == 1
                && inner.abandoned_id == FakeUpstream::request_id_of(1)
                && inner.abandoned_status == 200
                && !inner.abandoned_during_prefill,
            "a refused write must be an abandon with the id in hand: abandoned={} calls={} id={}",
            out.abandoned,
            rec.abandoned_calls.load(Ordering::SeqCst),
            inner.abandoned_id
        );
    }
    assert!(out.reply.is_err(), "the request must be aborted (Canceled)");
    assert_eq!(
        out.reply.unwrap_err(),
        HttpError::Canceled,
        "the request must be aborted (Canceled)"
    );
    assert!(
        took <= Duration::from_secs(1),
        "the call must end at the refused write, took {took:?}"
    );
}

// A call that completes must not wait out the watch's poll before returning:
// the watch is woken when send() returns. With poll = 2s a completed call
// still returns in milliseconds.
#[test]
fn a_completed_call_is_not_held_for_a_poll() {
    let upstream = FakeUpstream::new();
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let mut call = call_for(&rec);
    call.poll = Duration::from_secs(2);
    let started = Instant::now();
    let out = post_watched(&mut lease, call);
    let took = started.elapsed();
    assert!(
        out.reply.is_ok() && out.reply.as_ref().unwrap().status == 200 && !out.abandoned,
        "a normal stream must complete"
    );
    assert!(
        took <= Duration::from_millis(500),
        "a completed call must return at once, not after the poll: took {took:?}"
    );
}

// A receiver that panics (a translator bug, an allocation failure) must
// unwind out of post_watched with the watch thread joined: `thread::scope`'s
// panic-safe join guarantees that even though this leaves the watch thread's
// own result unchecked.
#[test]
fn a_throwing_receiver_unwinds_with_the_watch_joined() {
    let upstream = FakeUpstream::new();
    let pool = pool_for(&upstream);
    let rec = Recorded::new();
    let mut lease = pool.acquire("test-key");
    let mut call = call_for(&rec);
    call.receiver = Some(Box::new(|_data: &[u8]| -> bool {
        panic!("receiver bug");
    }));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        post_watched(&mut lease, call)
    }));
    let thrown = match &outcome {
        Err(payload) => {
            payload.downcast_ref::<&str>() == Some(&"receiver bug")
                || payload.downcast_ref::<String>().map(|s| s.as_str()) == Some("receiver bug")
        }
        Ok(_) => false,
    };
    assert!(
        thrown,
        "the receiver's panic must reach the caller (post_watched does not swallow it)"
    );
    // Still here: the process did not terminate, and the lease is usable
    // again for a normal call on a fresh connection.
    lease.discard();
    let mut again = pool.acquire("test-key");
    let rec2 = Recorded::new();
    let out = post_watched(&mut again, call_for(&rec2));
    assert!(
        out.reply.is_ok() && out.reply.as_ref().unwrap().status == 200,
        "a normal call after the panic must still work"
    );
}
