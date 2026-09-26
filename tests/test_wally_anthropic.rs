//! Port of tests/test_wally_anthropic.cpp: the Anthropic translator's
//! upstream connection behaviour, against the fake upstreams in
//! tests/common/fake_upstream.rs.

#[path = "common/fake_upstream.rs"]
mod fake_upstream;
#[path = "common/shim_lock.rs"]
mod shim_lock;

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fake_upstream::{describe, FakeUpstream, HalfOpenUpstream};
use wally::anthropic::{self, ModelAliases, Shim};
use wally::harness::{DeclaredHarness, Endpoint};
use wally::net::http1::{Client, Request, Server, StopHandle};
use wally::net::upstream_pool::{retry_on_fresh_connection, UpstreamOptions, UpstreamPool};

/// A translator started against `upstream`, stopped on drop.
struct RunningShim {
    shim: Shim,
    started: bool,
}

impl RunningShim {
    /// `console_url` is where the shim cancels an abandoned request: the fake
    /// upstream serves the cancel route on its own origin, so tests pass its
    /// base without the `/v1`. Empty means a local server -- no cancel is
    /// ever sent.
    fn new(upstream_base_url: &str, console_url: &str) -> Self {
        Self::declaring(upstream_base_url, console_url, DeclaredHarness::KClaudeCode)
    }

    fn declaring(upstream_base_url: &str, console_url: &str, declared: DeclaredHarness) -> Self {
        let endpoint = Endpoint {
            base_url: upstream_base_url.to_string(),
            api_key: "test-upstream-key".to_string(),
            console_url: console_url.to_string(),
            serving: false,
            context_window: 0,
            max_output: 0,
        };
        match anthropic::start(
            &endpoint,
            "glm-5.3",
            declared,
            false,
            "",
            &ModelAliases::new(),
        ) {
            Some(shim) => RunningShim {
                shim,
                started: true,
            },
            None => RunningShim {
                shim: Shim::default(),
                started: false,
            },
        }
    }

    fn local(upstream_base_url: &str) -> Self {
        Self::new(upstream_base_url, "")
    }

    /// Stops the translator now (what the wrapper does when the editor
    /// exits) and returns how long that took.
    fn stop_now(&mut self) -> Duration {
        let started = Instant::now();
        anthropic::stop(&mut self.shim);
        started.elapsed()
    }

    fn started(&self) -> bool {
        self.started
    }

    fn shim(&self) -> &Shim {
        &self.shim
    }

    /// One Anthropic-shaped request through the translator; the status it
    /// answered with, or 0 when nothing came back.
    fn send(&self, streaming: bool) -> i32 {
        let mut client = match Client::new(
            &self.shim.base_url,
            Duration::from_secs(10),
            Duration::from_secs(10),
        ) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let body = serde_json::json!({
            "model": "claude-x",
            "max_tokens": 16,
            "stream": streaming,
            "messages": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let request = Request::post("/v1/messages", body.into_bytes())
            .header("Authorization", format!("Bearer {}", self.shim.auth_token))
            .header("Content-Type", "application/json");
        match client.send(&request, None, None) {
            Ok(reply) => reply.status,
            Err(_) => 0,
        }
    }
}

impl Drop for RunningShim {
    fn drop(&mut self) {
        anthropic::stop(&mut self.shim);
    }
}

/// The origin of a fake upstream's base URL: "http://127.0.0.1:port/v1" ->
/// "http://127.0.0.1:port". What the shim treats as the console for cancels.
fn origin_of(base_url: &str) -> String {
    match base_url.rfind("/v1") {
        Some(idx) => base_url[..idx].to_string(),
        None => base_url.to_string(),
    }
}

/// An editor that opens a streaming request to the shim on its own thread and
/// can leave in the middle of it -- `leave()` force-closes its socket via the
/// client's `StopHandle`, which is what Claude Code's abort does (undici
/// destroys the socket: a FIN).
struct Editor {
    client: Option<Client>,
    stop_handle: StopHandle,
    token: String,
    thread: Option<std::thread::JoinHandle<()>>,
    received: Arc<Mutex<Vec<u8>>>,
    status: Arc<AtomicI32>,
}

impl Editor {
    fn new(shim: &Shim) -> Self {
        let client = Client::new(
            &shim.base_url,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .expect("build editor client");
        let stop_handle = client.stop_handle();
        Editor {
            client: Some(client),
            stop_handle,
            token: shim.auth_token.clone(),
            thread: None,
            received: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(AtomicI32::new(0)),
        }
    }

    fn start_streaming(&mut self) {
        let mut client = self.client.take().expect("editor client already taken");
        let token = self.token.clone();
        let received = self.received.clone();
        let status = self.status.clone();
        self.thread = Some(std::thread::spawn(move || {
            let body = serde_json::json!({
                "model": "claude-x",
                "max_tokens": 16,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}],
            })
            .to_string();
            let request = Request::post("/v1/messages", body.into_bytes())
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json");
            let mut receiver = |data: &[u8]| -> bool {
                received.lock().unwrap().extend_from_slice(data);
                true
            };
            let reply = client.send(&request, None, Some(&mut receiver));
            status.store(reply.map(|r| r.status).unwrap_or(0), Ordering::SeqCst);
        }));
    }

    /// Leaves the way an editor does: the socket is force-closed (Claude Code
    /// aborts the fetch; a quitting app closes everything), so the shim's
    /// next write to it fails.
    fn leave(&mut self) {
        self.stop_handle.stop();
        self.join();
        self.client = None;
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn received(&self) -> Vec<u8> {
        self.received.lock().unwrap().clone()
    }

    fn status(&self) -> i32 {
        self.status.load(Ordering::SeqCst)
    }
}

// A pre-stream overload (429/503) must reach the editor as that status, with
// the upstream Retry-After intact, whether the request streamed or not --
// never a blind 200 event-stream carrying the error inside it. The
// header-peek worker learns the status before the sink is committed and
// answers it as a normal reply.
#[test]
fn overload_headers_survive_streaming() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    let calls = Arc::new(AtomicI32::new(0));
    let calls_route = calls.clone();
    server.route("POST", "/v1/chat/completions", move |req, res, _peer| {
        calls_route.fetch_add(1, Ordering::SeqCst);
        let max_tokens = serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v.get("max_tokens").and_then(|m| m.as_i64()))
            .unwrap_or(429) as i32;
        let mut status = max_tokens;
        let retry_after = if status == 429 {
            "7"
        } else {
            "Wed, 21 Oct 2037 07:28:00 GMT"
        };
        if req.header("Authorization") != Some("Bearer test-upstream-key") {
            status = 401;
        }
        let _ = res.send_full(
            status,
            &[
                ("Retry-After", retry_after),
                ("Content-Type", "application/json"),
            ],
            br#"{"error":{"message":"capacity exhausted"}}"#,
        );
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let mut okay = started.is_some();
    if let Some(shim) = &started {
        let mut client = Client::new(
            &shim.base_url,
            Duration::from_secs(10),
            Duration::from_secs(10),
        )
        .unwrap();
        for streaming in [false, true] {
            for status in [429, 503] {
                let body = serde_json::json!({
                    "model": "test",
                    "stream": streaming,
                    "max_tokens": status,
                    "messages": [{"role": "user", "content": "hi"}],
                })
                .to_string();
                let request = Request::post("/v1/messages", body.into_bytes())
                    .header("x-api-key", shim.auth_token.clone())
                    .header("Content-Type", "application/json");
                let reply = client.send(&request, None, None);
                let expected_retry = if status == 429 {
                    "7"
                } else {
                    "Wed, 21 Oct 2037 07:28:00 GMT"
                };
                let ok_this = match &reply {
                    Ok(r) => {
                        r.status == status
                            && r.header("Retry-After") == Some(expected_retry)
                            && r.header("Content-Type")
                                .map(|c| c.starts_with("application/json"))
                                .unwrap_or(false)
                            && String::from_utf8_lossy(&r.body).contains("capacity exhausted")
                    }
                    Err(_) => false,
                };
                okay = okay && ok_this;
            }
        }
    }
    if let Some(mut shim) = started {
        anthropic::stop(&mut shim);
    }
    handle.stop();
    assert!(
        okay && calls.load(Ordering::SeqCst) == 4,
        "stream/nonstream preserve 429/503 and numeric/date Retry-After; authenticated once each"
    );
}

// A local model can spend longer than an editor's idle timeout in prefill.
// The shim must write harmless SSE comments while it waits for the first
// upstream token so Claude Code does not report a network failure and retry.
#[test]
fn prefill_sends_keepalive_comments() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_streams_until(99);
    let shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(1300));
    let received = editor.received();
    editor.leave();
    editor.join();
    assert!(
        String::from_utf8_lossy(&received).contains(": keepalive\n\n"),
        "no SSE keepalive reached the editor during prefill"
    );
}

// Two requests, one after the other, must arrive at the upstream on the same
// connection. Building a client per request (rather than reusing the pool)
// would open a new connection each time, so the ports would differ.
#[test]
fn sequential_requests_reuse_the_upstream_connection() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    let shim = RunningShim::local(&upstream.base_url());
    assert!(shim.started(), "translator did not start");
    let first = shim.send(true);
    let second = shim.send(false);
    let ports = upstream.ports();
    assert!(
        first == 200 && second == 200 && ports.len() == 2,
        "two 200s and two upstream requests: {first}, {second}, {}",
        describe(&ports)
    );
    assert_eq!(
        ports[0],
        ports[1],
        "same peer port on both upstream requests (one connection): {}",
        describe(&ports)
    );
}

// Two requests in flight at once must NOT share a connection: a single
// shared client serialises requests on its socket, so a single shared client
// would queue the second stream behind the first. The upstream holds both
// streams until both have arrived, so a finished stream cannot lend its
// connection and make the test pass by legitimate reuse.
#[test]
fn concurrent_requests_use_separate_connections() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_streams_until(2);
    let shim = Arc::new(RunningShim::local(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    let a_shim = shim.clone();
    let b_shim = shim.clone();
    let a = std::thread::spawn(move || a_shim.send(true));
    let b = std::thread::spawn(move || b_shim.send(true));
    let first = a.join().unwrap();
    let second = b.join().unwrap();
    let ports = upstream.ports();
    assert!(
        first == 200 && second == 200 && ports.len() == 2,
        "two 200s and two upstream requests: {first}, {second}, {}",
        describe(&ports)
    );
    assert_ne!(
        ports[0],
        ports[1],
        "different peer ports (two connections in flight): {}",
        describe(&ports)
    );
}

// The pool on its own, no translator in front of it. Needs a real listener
// (unlike the other pool-bookkeeping tests below, which never call send()):
// since the pool.rs fix for comment #55, `reused()` reports whether the
// idle client's connection is actually still live, so a client that never
// connected at all -- which is all `http://127.0.0.1:9` (nothing listening)
// would ever produce -- would never register as reused either, and this
// test would stop meaning anything.
#[test]
fn pool_returns_a_clean_lease_and_drops_a_discarded_one() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    server.route("GET", "/ping", |_req, res, _peer| {
        res.send_full(200, &[], b"pong").unwrap();
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();

    let pool = UpstreamPool::new(UpstreamOptions {
        origin: format!("http://127.0.0.1:{port}"),
        idle_limit: 2,
        ..Default::default()
    });
    {
        let mut first = pool.acquire("k");
        assert!(!first.reused(), "a fresh lease from an empty pool");
        assert_eq!(
            pool.idle(),
            0,
            "no idle client while the fresh lease is out"
        );
        // Establishes the connection this lease's client keeps once it goes
        // back to idle -- without this, `second` below would have nothing
        // live to reuse.
        let reply = first.client().send(&Request::get("/ping"), None, None);
        assert_eq!(reply.unwrap().status, 200);
    }
    assert_eq!(pool.idle(), 1, "1 idle client after a clean lease ends");
    {
        let mut second = pool.acquire("k");
        assert!(second.reused(), "the idle client reused");
        second.discard();
    }
    assert_eq!(pool.idle(), 0, "0 idle after a discarded lease ends");
    // Three clean leases at once, then returned: the idle set stops at the limit.
    {
        let _a = pool.acquire("k");
        let _b = pool.acquire("k");
        let _c = pool.acquire("k");
    }
    assert_eq!(pool.idle(), 2, "idle capped at 2");
    handle.stop();
}

// A pool outlives its leases: dropping the last Arc while a lease is out
// must not leave the lease pointing at freed memory.
#[test]
fn pool_outlives_an_outstanding_lease() {
    let _shim_guard = shim_lock::shim_lock();
    let pool = UpstreamPool::new(UpstreamOptions {
        origin: "http://127.0.0.1:9".to_string(),
        ..Default::default()
    });
    let weak = Arc::downgrade(&pool);
    {
        let lease = pool.acquire("k");
        drop(pool);
        assert!(weak.upgrade().is_some(), "pool alive while a lease is out");
        drop(lease);
    }
    assert!(
        weak.upgrade().is_none(),
        "pool freed once the last lease returned"
    );
}

#[test]
fn retry_rule_only_on_a_stale_reused_connection() {
    let _shim_guard = shim_lock::shim_lock();
    use wally::net::http1::Error as E;

    struct Case {
        error: E,
        has_response: bool,
        received_any: bool,
        reused: bool,
        expect: bool,
        why: &'static str,
    }
    // The C++ table has 12 rows; two are omitted here. `E::Timeout` and
    // `E::SSLServerVerification` have no Rust `http1::Error` equivalent --
    // see `retry_on_fresh_connection`'s doc comment in upstream_pool.rs for
    // why this crate's Error is deliberately smaller than httplib's.
    let cases = [
        Case {
            error: E::Read,
            has_response: false,
            received_any: false,
            reused: true,
            expect: true,
            why: "stale reused socket, nothing back",
        },
        Case {
            error: E::Connection,
            has_response: false,
            received_any: false,
            reused: true,
            expect: true,
            why: "reused, connect-class error",
        },
        Case {
            error: E::ConnectionClosed,
            has_response: false,
            received_any: false,
            reused: true,
            expect: true,
            why: "reused, closed by peer",
        },
        Case {
            error: E::Write,
            has_response: false,
            received_any: false,
            reused: true,
            expect: true,
            why: "reused, write failed",
        },
        Case {
            error: E::SslConnection,
            has_response: false,
            received_any: false,
            reused: true,
            expect: true,
            why: "reused, TLS layer reset",
        },
        Case {
            error: E::Read,
            has_response: false,
            received_any: false,
            reused: false,
            expect: false,
            why: "fresh connection: a real outage surfaces",
        },
        Case {
            error: E::Read,
            has_response: true,
            received_any: false,
            reused: true,
            expect: false,
            why: "a status arrived: never repeat",
        },
        Case {
            error: E::Read,
            has_response: false,
            received_any: true,
            reused: true,
            expect: false,
            why: "bytes reached the caller: never repeat",
        },
        Case {
            error: E::ConnectionTimeout,
            has_response: false,
            received_any: false,
            reused: true,
            expect: false,
            why: "connect timeout is the network",
        },
        Case {
            error: E::Canceled,
            has_response: false,
            received_any: false,
            reused: true,
            expect: false,
            why: "a reader that left is not a stale socket",
        },
    ];
    for case in cases {
        let got = retry_on_fresh_connection(
            case.error,
            case.has_response,
            case.received_any,
            case.reused,
        );
        assert_eq!(
            got,
            case.expect,
            "{} -> expected {}",
            case.why,
            if case.expect { "retry" } else { "no retry" }
        );
    }
}

// A reused connection the far side has quietly stopped serving: the request
// goes out, nothing comes back, the connection ends. The translator must try
// once more on a fresh connection and answer 200, and the upstream must see
// exactly three requests: the first (answered), the stale one (dropped), and
// the retry (answered) on a NEW connection.
//
// Unlike the C++ original (POSIX-only, `#if defined(_WIN32)` skipped): this
// crate's `HalfOpenUpstream` is built on plain `std::net`, not raw POSIX
// sockets, so it runs on every platform and this case is not skipped here.
#[test]
fn stale_reused_connection_is_retried_once_on_a_fresh_one() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = HalfOpenUpstream::new();
    assert!(upstream.ok(), "could not bind the half-open upstream");
    let shim = RunningShim::local(&upstream.base_url());
    assert!(shim.started(), "translator did not start");
    let first = shim.send(false);
    let second = shim.send(false);
    let ports = upstream.ports();
    assert!(
        first == 200 && second == 200 && ports.len() == 3 && ports[0] == ports[1] && ports[2] != ports[0],
        "200, 200; three upstream requests, the first two on one connection, the third on another: {first}, {second}; {}",
        describe(&ports)
    );
}

// The case received_any exists for: a REUSED connection whose far side dies
// after it has started answering. The error is connection-class and there is
// no status, so only the "bytes reached the caller" rule stops a retry -- and
// a retry would run the generation twice. The upstream must see exactly two
// requests: the one that warmed the connection and the one that died on it.
#[test]
fn upstream_dying_mid_stream_is_not_retried() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    let shim = RunningShim::local(&upstream.base_url());
    assert!(shim.started(), "translator did not start");
    let warm = shim.send(false);
    upstream.die_mid_stream(true);
    let dying = shim.send(true);
    let ports = upstream.ports();
    assert!(
        warm == 200 && dying == 200 && ports.len() == 2 && ports[0] == ports[1],
        "200, 200 (the error rides inside the stream); exactly two upstream requests on one connection: {warm}, {dying}; {}",
        describe(&ports)
    );
}

// The editor leaves while the upstream is still producing the body (the
// engine is decoding; the id is already in hand). Within a second the fake's
// cancel route sees that id with this session's bearer, the upstream socket
// is dropped, and -- the pool having been warmed so the lease is REUSED --
// the stale-retry rule does not re-send the prompt: arrivals stay at two.
#[test]
fn an_abandoned_stream_is_cancelled_by_name_and_never_resent() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    let shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    // Warm the pool: the second request goes out on a reused connection.
    assert_eq!(shim.send(false), 200, "warm-up request failed");
    upstream.hold_streams_until(99); // the body never comes on its own
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();

    assert!(
        upstream.wait_for_cancels(1, Duration::from_secs(1)),
        "no cancel reached the endpoint within 1s of the editor leaving"
    );
    let cancels = upstream.cancels();
    assert!(
        cancels[0].request_id == FakeUpstream::request_id_of(2)
            && cancels[0].authorization == "Bearer test-upstream-key",
        "the cancel must name the abandoned request with the session's bearer: id={} auth={}",
        cancels[0].request_id,
        cancels[0].authorization
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        upstream.arrivals(),
        2,
        "two upstream requests (warm-up + the abandoned one); no re-send"
    );
}

// Leaving while tokens are FLOWING -- Esc mid-answer, the common case. Here
// the leave is noticed by the failed write to the editor, not by the poll
// (the fake drips a frame every 5ms; the 100ms poll rarely gets there
// first), and that path must name the cancel just the same, and drop the
// upstream socket so the drip stops.
#[test]
fn leaving_while_tokens_flow_cancels_by_name() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    let shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    assert_eq!(shim.send(false), 200, "warm-up request failed"); // warm the pool: the stream's lease is reused
    upstream.drip(1000, 2); // ~2s of tokens
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();
    assert!(
        upstream.wait_for_cancels(1, Duration::from_secs(1)),
        "no cancel reached the endpoint within 1s of the editor leaving"
    );
    let cancels = upstream.cancels();
    assert!(
        cancels.len() == 1
            && cancels[0].request_id == FakeUpstream::request_id_of(2)
            && cancels[0].authorization == "Bearer test-upstream-key",
        "one cancel naming the abandoned request: n={}",
        cancels.len()
    );
    // The upstream socket is dropped: the fake's drip stops growing.
    std::thread::sleep(Duration::from_millis(200));
    let dripped = upstream.dripped();
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        upstream.dripped() == dripped && dripped < 1000,
        "the upstream socket must be dropped once the editor left: dripped {} then {}",
        dripped,
        upstream.dripped()
    );
    assert!(
        upstream.arrivals() == 2 && upstream.cancels().len() == 1,
        "no re-send and no second cancel: arrivals={} cancels={}",
        upstream.arrivals(),
        upstream.cancels().len()
    );
    let received = editor.received();
    assert!(
        String::from_utf8_lossy(&received).contains("content_block_delta"),
        "the frames before the leave must have reached the editor"
    );
}

// A stream that completed is never cancelled, whenever the editor goes.
#[test]
fn a_completed_stream_is_not_cancelled() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.die_mid_stream(false);
    let shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    // A stream the fake finishes on its own is over in a millisecond, so the
    // editor leaves after it -- that must NOT cancel (nothing is running).
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    editor.join();
    editor.leave();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        upstream.cancels().is_empty(),
        "a completed stream must never be cancelled"
    );
    assert_eq!(editor.status(), 200);
    let received = editor.received();
    assert!(
        String::from_utf8_lossy(&received).contains("message_stop"),
        "the completed stream must have reached the editor whole"
    );
}

// A local endpoint (no console, no key) has nothing to cancel: the abandon is
// logged and no cancel is attempted anywhere.
#[test]
fn a_local_endpoint_is_never_cancelled() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_streams_until(99);
    let shim = RunningShim::local(&upstream.base_url()); // no console_url
    assert!(shim.started(), "translator did not start");
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        upstream.cancels().is_empty(),
        "a local server must not be asked to cancel"
    );
}

// The wrapper exits right after the editor abandoned a stream (app quit):
// stop() must let the cancel go out before returning -- the fake sits on its
// answer for 500ms, so an un-joined stop() would return without it -- and
// must still return within the bounded cancel window, not after waiting for
// the engine's first token. Windows' socket shutdown reaches the 3s cancel
// bound plus the 500ms reply delay and one scheduler tick, so keep 1s of
// timing slack without weakening the behavioral assertions.
#[test]
fn stop_sends_the_last_cancel_before_returning() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_streams_until(99);
    upstream.delay_cancel_reply(500);
    let mut shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();
    let took = shim.stop_now();
    assert_eq!(
        upstream.cancels().len(),
        1,
        "stop() returned without sending the abandoned request's cancel"
    );
    assert!(took <= Duration::from_millis(4500), "stop() took {took:?}");
}

// The editor leaves during PREFILL -- the upstream has not sent its headers,
// so no id exists yet. The shim keeps the upstream open, and when the
// headers arrive it cancels by the id they carry.
//
// Scoped to POSIX -- see reader_gone_mid_body_names_the_cancel_and_drops_the_socket
// in test_wally_net_call.rs: the loopback fake signals a gone reader by
// closing a POSIX-shaped connection, and abandon detection reads that close
// differently on winsock, so this hermetic timing test would hang on the
// fake's 5s wait rather than measuring the product.
#[test]
#[cfg(windows)]
fn leaving_during_prefill_cancels_at_the_first_token() {}

#[test]
#[cfg(not(windows))]
fn leaving_during_prefill_cancels_at_the_first_token() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_headers(true);
    let shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        upstream.cancels().is_empty(),
        "nothing can be cancelled before the id exists"
    );
    upstream.release_headers(); // the first token
    assert!(
        upstream.wait_for_cancels(1, Duration::from_secs(1)),
        "the cancel must follow the headers within 1s"
    );
    assert_eq!(
        upstream.cancels()[0].request_id,
        FakeUpstream::request_id_of(1)
    );
}

// If the wrapper exits first (rather than the headers ever arriving), it
// gives up within a poll instead of waiting for the first token.
#[test]
fn stopping_during_prefill_does_not_wait_for_the_first_token() {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    upstream.hold_headers(true);
    let mut shim = RunningShim::new(&upstream.base_url(), &origin_of(&upstream.base_url()));
    assert!(shim.started(), "translator did not start");
    let mut editor = Editor::new(shim.shim());
    editor.start_streaming();
    std::thread::sleep(Duration::from_millis(300));
    editor.leave();
    editor.join();
    let took = shim.stop_now();
    upstream.release_headers();
    assert!(
        took <= Duration::from_millis(1500),
        "stop() waited for the first token: {took:?}"
    );
}

// A connection the shim never manages to open must answer 502 with "the
// model endpoint did not answer" -- not the 502 status folded into the
// status *reported to the translator*, which used to make it say "the model
// endpoint returned status 502" instead (and would have logged 502, not 0,
// to shim.log). Covers the non-streaming and pre-stream-failure streaming
// paths, both of which keep a raw/possibly-zero status separate from the
// client-visible one.
#[test]
fn a_dead_upstream_answers_502_with_the_did_not_answer_message() {
    let _shim_guard = shim_lock::shim_lock();
    // A port with nothing listening: bind to grab a free one, then drop the
    // listener so the connect the shim attempts is refused immediately
    // rather than hanging.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead_port = listener.local_addr().unwrap().port();
    drop(listener);

    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{dead_port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let shim = started.expect("translator did not start");
    let mut client = Client::new(
        &shim.base_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .unwrap();
    for streaming in [false, true] {
        let body = serde_json::json!({
            "model": "test",
            "stream": streaming,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let request = Request::post("/v1/messages", body.into_bytes())
            .header("x-api-key", shim.auth_token.clone())
            .header("Content-Type", "application/json");
        let reply = client
            .send(&request, None, None)
            .expect("shim answers even when the upstream is unreachable");
        assert_eq!(reply.status, 502, "streaming={streaming}");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            parsed["error"]["message"],
            "the model endpoint did not answer",
            "streaming={streaming} body={:?}",
            String::from_utf8_lossy(&reply.body)
        );
    }
    let mut shim = shim;
    anthropic::stop(&mut shim);
}

// A raw upstream error body cut at 1000 bytes (not 1000 characters) can land
// mid-character; nlohmann's `.dump()` throws serializing the result. Neither
// `HandleNonStreaming` nor `HandleStreaming` catches that locally, so it
// unwinds to the try/catch wrapped directly around them in the `POST
// /v1/messages` handler (not httplib's own routing()-level catch -- nothing
// registers an exception_handler_, but this closer one fires first), which
// answers 500 with `error.what()` re-wrapped as a fresh `api_error` body --
// not the (impossible to construct in Rust, since a `String` can never hold
// invalid UTF-8) translated error it would otherwise have been. Covers both
// the non-streaming and the streaming pre-stream-failure paths, which share
// the same cascade.
#[test]
fn a_1000_byte_cut_that_splits_a_utf8_character_falls_back_to_the_generic_500() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    // 999 ASCII bytes, then a 2-byte UTF-8 character (U+00E9, "e"): the cut
    // at byte 1000 keeps the 999 ASCII bytes plus exactly the character's
    // first byte, an invalid, un-terminated sequence. Not JSON, so
    // `upstream_failure` never gets a message from `payload_error` first.
    let mut raw_body = "a".repeat(999).into_bytes();
    raw_body.extend_from_slice("\u{00e9}\u{00e9}".as_bytes());
    server.route("POST", "/v1/chat/completions", move |_req, res, _peer| {
        let _ = res.send_full(400, &[("Content-Type", "text/plain")], &raw_body);
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let shim = started.expect("translator did not start");
    let mut client = Client::new(
        &shim.base_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .unwrap();
    for streaming in [false, true] {
        let body = serde_json::json!({
            "model": "test",
            "stream": streaming,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let request = Request::post("/v1/messages", body.into_bytes())
            .header("x-api-key", shim.auth_token.clone())
            .header("Content-Type", "application/json");
        let reply = client
            .send(&request, None, None)
            .expect("shim answers even on the malformed-UTF-8 cascade");
        assert_eq!(reply.status, 500, "streaming={streaming}");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            parsed["error"]["type"], "api_error",
            "streaming={streaming}"
        );
        // Byte 999 of the raw body is 0xC3, the lead byte of the first "e"'s
        // 2-byte encoding; the cut at byte 1000 keeps only that lead byte.
        assert_eq!(
            parsed["error"]["message"],
            "[json.exception.type_error.316] incomplete UTF-8 string; last byte: 0xC3",
            "streaming={streaming}"
        );
    }
    let mut shim = shim;
    anthropic::stop(&mut shim);
    handle.stop();
}

// `parsed.value("stream", false)` on the C++ side calls nlohmann's
// `get<bool>()` once the key is present, which throws a `type_error` for any
// non-boolean value instead of silently defaulting to false -- and that
// throw lands in the same try/catch around `HandleStreaming`/
// `HandleNonStreaming`, answering 500 with a fresh `api_error` body before
// either handler (or any upstream call) is ever reached.
#[test]
fn a_non_boolean_stream_field_answers_the_generic_500_not_a_silent_false() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    server.route("POST", "/v1/chat/completions", |_req, _res, _peer| {
        panic!("upstream must never be called for a request the shim itself rejects");
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let shim = started.expect("translator did not start");
    let mut client = Client::new(
        &shim.base_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .unwrap();
    let cases: [(serde_json::Value, &str); 4] = [
        (serde_json::json!("true"), "string"),
        (serde_json::json!(1), "number"),
        (serde_json::Value::Null, "null"),
        (serde_json::json!([true]), "array"),
    ];
    for (stream_value, want_type_name) in cases {
        let body = serde_json::json!({
            "model": "test",
            "stream": stream_value,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
        })
        .to_string();
        let request = Request::post("/v1/messages", body.into_bytes())
            .header("x-api-key", shim.auth_token.clone())
            .header("Content-Type", "application/json");
        let reply = client
            .send(&request, None, None)
            .expect("shim answers even for a rejected request shape");
        assert_eq!(reply.status, 500, "stream={want_type_name}");
        let parsed: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(
            parsed["error"]["type"], "api_error",
            "stream={want_type_name}"
        );
        assert_eq!(
            parsed["error"]["message"],
            format!(
                "[json.exception.type_error.302] type must be boolean, but is {want_type_name}"
            ),
            "stream={want_type_name}"
        );
    }
    let mut shim = shim;
    anthropic::stop(&mut shim);
    handle.stop();
}

// `parsed.value("stream", false)` on the C++ side is nlohmann's `value()`,
// which only works on `is_object()`: a `null`/array/string/number/boolean
// top-level body throws `type_error.306` ("cannot use value() with <type>")
// before `value()` ever looks at the "stream" key -- so a malformed body
// never reaches `HandleStreaming`/`HandleNonStreaming`, and nothing goes
// upstream. `serde_json::Value::get` has no such guard on a non-object (it
// just returns `None`), so a body that is not a JSON object has to be
// rejected explicitly to avoid being silently treated as `{}` and forwarded.
#[test]
fn a_non_object_top_level_body_answers_the_generic_500_and_never_reaches_upstream() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    server.route("POST", "/v1/chat/completions", |_req, _res, _peer| {
        panic!("upstream must never be called for a request the shim itself rejects");
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let shim = started.expect("translator did not start");
    let mut client = Client::new(
        &shim.base_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .unwrap();
    // The exact bodies the C++/Rust diff harness's shim_probe.py drives as
    // json_null/json_array/json_string/json_number/json_true: the whole
    // request body, not a field inside one.
    let cases: [(&str, &str); 5] = [
        ("null", "null"),
        ("[]", "array"),
        ("\"hi\"", "string"),
        ("123", "number"),
        ("true", "boolean"),
    ];
    for (body, want_type_name) in cases {
        let request = Request::post("/v1/messages", body.as_bytes().to_vec())
            .header("x-api-key", shim.auth_token.clone())
            .header("Content-Type", "application/json");
        let reply = client
            .send(&request, None, None)
            .expect("shim answers even for a non-object top-level body");
        assert_eq!(reply.status, 500, "body={body}");
        assert_eq!(
            reply.header("Content-Type"),
            Some("application/json"),
            "body={body}"
        );
        assert_eq!(
            String::from_utf8(reply.body.clone()).unwrap(),
            format!(
                "{{\"error\":{{\"message\":\"[json.exception.type_error.306] cannot use value() with {want_type_name}\",\"type\":\"api_error\"}},\"type\":\"error\"}}"
            ),
            "body={body}"
        );
    }
    let mut shim = shim;
    anthropic::stop(&mut shim);
    handle.stop();
}

// An SSE `data:` line whose JSON string content carries a lone, non-continued
// UTF-8 lead byte (0xE9 immediately followed by `"`, never a valid
// continuation byte) is exactly the shape nlohmann's `Json::parse` rejects
// mid-parse, byte for byte -- not a shape `serde_json::from_slice` should
// ever get the chance to lossily repair into a normal chunk first. The
// payload accumulator has to stay raw bytes (never pre-converted through
// `String::from_utf8_lossy`) for this to surface as the same malformed-frame
// error event C++ produces instead of a corrupted-but-"successful" delta.
#[test]
fn an_invalid_utf8_sse_data_line_is_a_malformed_frame_not_a_silently_repaired_chunk() {
    let _shim_guard = shim_lock::shim_lock();
    let mut server = Server::new();
    server.route("POST", "/v1/chat/completions", |_req, res, _peer| {
        let mut frame: Vec<u8> = b"data: {\"choices\":[{\"delta\":{\"content\":\"caf".to_vec();
        frame.push(0xE9); // lead byte of a 2-byte sequence, but the next byte (0x22, '"') is not a continuation byte
        frame.extend_from_slice(b"\"}}]}\n\n");
        if res
            .begin_chunked(200, &[("Content-Type", "text/event-stream")])
            .is_err()
        {
            return;
        }
        let _ = res.write_chunk(&frame);
        let _ = res.end_chunked();
    });
    let (mut handle, port) = server.bind_and_run("127.0.0.1").unwrap();
    let endpoint = Endpoint {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        api_key: "test-upstream-key".to_string(),
        console_url: String::new(),
        serving: false,
        context_window: 0,
        max_output: 0,
    };
    let started = anthropic::start(
        &endpoint,
        "test-model",
        DeclaredHarness::KClaudeCode,
        false,
        "",
        &ModelAliases::new(),
    );
    let shim = started.expect("translator did not start");
    let mut client = Client::new(
        &shim.base_url,
        Duration::from_secs(10),
        Duration::from_secs(10),
    )
    .unwrap();
    let body = serde_json::json!({
        "model": "test",
        "stream": true,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
    })
    .to_string();
    let request = Request::post("/v1/messages", body.into_bytes())
        .header("x-api-key", shim.auth_token.clone())
        .header("Content-Type", "application/json");
    let reply = client
        .send(&request, None, None)
        .expect("shim answers even on a malformed SSE frame");
    assert_eq!(reply.status, 200, "the stream itself opens fine");
    let received = String::from_utf8_lossy(&reply.body);
    assert!(
        received.contains("the model endpoint sent a malformed stream frame"),
        "expected the malformed-frame error event; got: {received}"
    );
    assert!(
        !received.contains("\"text\":\"caf"),
        "the invalid byte must not surface as a lossily-repaired content delta; got: {received}"
    );
    let mut shim = shim;
    anthropic::stop(&mut shim);
    handle.stop();
}

// The bridge declares the harness it serves on every upstream request, buffered
// and streaming alike: `X-RA-Harness` with the contract's name, and a
// User-Agent of wally's own carrying the hyphenated needle harness.py's
// User-Agent table matches.
fn check_bridge_declares(declared: DeclaredHarness, value: &str, needle: &str) {
    let _shim_guard = shim_lock::shim_lock();
    let upstream = FakeUpstream::new();
    let shim = RunningShim::declaring(&upstream.base_url(), "", declared);
    assert!(shim.started(), "translator did not start");
    assert_eq!((shim.send(false), shim.send(true)), (200, 200));
    let seen = upstream.seen();
    assert_eq!(seen.len(), 2);
    assert!(!seen[0].streaming && seen[1].streaming);
    for request in &seen {
        assert_eq!(request.harness.as_deref(), Some(value));
        assert!(
            request.user_agent.starts_with("wally/") && request.user_agent.contains(needle),
            "User-Agent: {}",
            request.user_agent
        );
    }
}

#[test]
fn bridge_declares_claude_code() {
    check_bridge_declares(DeclaredHarness::KClaudeCode, "claude_code", "(claude-code)");
}

#[test]
fn bridge_declares_claude_desktop() {
    check_bridge_declares(
        DeclaredHarness::KClaudeDesktop,
        "claude_desktop",
        "(claude-desktop)",
    );
}
