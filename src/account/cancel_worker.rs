//! Sends cancels to the control plane off the request path (port of
//! src/account/cancel_worker.cpp, wally #81): one worker thread, in order, each
//! bounded by `timeout_ms`; `stop()` drains the queue and joins. The bearer is
//! read at SEND time and is never logged.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::console::{CancelOutcome, ConsoleClient, Transport};

/// Every outcome is reported here (translators write it to their log).
pub type CancelResult = Arc<dyn Fn(&str, CancelOutcome, &str) + Send + Sync>;
/// Returns the current bearer at send time.
pub type Bearer = Arc<dyn Fn() -> String + Send + Sync>;

/// State the worker thread and the public methods both touch, guarded by one
/// mutex (mirrors the C++ `mutex_` covering both `queue_` and `stopping_`).
struct State {
    queue: VecDeque<String>,
    stopping: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// Sends cancels to the control plane off the request path (wally #81). One
/// worker thread, in order, each bounded by `timeout_ms`. `stop()` drains
/// whatever is already queued, then joins.
pub struct CancelWorker {
    shared: Arc<Shared>,
    // `&self`-only methods (matching the C++ API, where `Stop()` still runs
    // off a `CancelWorker&`) need this behind a lock to `take()` it on stop.
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn run(
    shared: Arc<Shared>,
    client: ConsoleClient,
    console_url: String,
    bearer: Bearer,
    timeout_ms: i32,
    on_result: CancelResult,
) {
    loop {
        let request_id = {
            let state = shared.state.lock().unwrap();
            let mut state = shared
                .wake
                .wait_while(state, |s| !s.stopping && s.queue.is_empty())
                .unwrap();
            match state.queue.pop_front() {
                Some(request_id) => request_id,
                // stopping, and nothing left to send
                None => return,
            }
        };
        let bearer_value = bearer();
        let (outcome, error) =
            client.cancel_request(&console_url, &bearer_value, &request_id, timeout_ms);
        on_result(&request_id, outcome, &error);
    }
}

impl CancelWorker {
    pub fn new(
        console_url: &str,
        bearer: Bearer,
        timeout_ms: i32,
        on_result: CancelResult,
        transport: Option<Transport>,
    ) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queue: VecDeque::new(),
                stopping: false,
            }),
            wake: Condvar::new(),
        });
        let client = ConsoleClient::new(transport);
        let console_url = console_url.to_string();
        let worker_shared = Arc::clone(&shared);
        let thread = std::thread::spawn(move || {
            run(
                worker_shared,
                client,
                console_url,
                bearer,
                timeout_ms,
                on_result,
            )
        });
        CancelWorker {
            shared,
            thread: Mutex::new(Some(thread)),
        }
    }

    /// Queue one cancel. Returns at once.
    pub fn enqueue(&self, request_id: &str) {
        {
            let mut state = self.shared.state.lock().unwrap();
            if state.stopping {
                return;
            }
            state.queue.push_back(request_id.to_string());
        }
        self.shared.wake.notify_one();
    }

    /// Send everything queued, then stop the worker. Bounded by the queue
    /// depth times `timeout_ms`; typically one call. Returns how many cancels
    /// were sent while stopping.
    pub fn stop(&self) -> i32 {
        let drained;
        {
            let mut state = self.shared.state.lock().unwrap();
            if state.stopping {
                return 0;
            }
            state.stopping = true;
            drained = state.queue.len() as i32;
        }
        self.shared.wake.notify_all();
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
        drained
    }

    pub fn pending(&self) -> i32 {
        self.shared.state.lock().unwrap().queue.len() as i32
    }
}

impl Drop for CancelWorker {
    fn drop(&mut self) {
        self.stop();
    }
}
