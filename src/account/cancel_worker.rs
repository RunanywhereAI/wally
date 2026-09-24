//! Sends cancels to the control plane off the request path (port of
//! src/account/cancel_worker.cpp, wally #81): one worker thread, in order, each
//! bounded by `timeout_ms`; `stop()` drains the queue and joins. The bearer is
//! read at SEND time and is never logged. Owner: the account port.

use std::sync::Arc;

use super::console::{CancelOutcome, Transport};

/// Every outcome is reported here (translators write it to their log).
pub type CancelResult = Arc<dyn Fn(&str, CancelOutcome, &str) + Send + Sync>;
/// Returns the current bearer at send time.
pub type Bearer = Arc<dyn Fn() -> String + Send + Sync>;

pub struct CancelWorker {
    _private: (),
}

impl CancelWorker {
    pub fn new(
        console_url: &str,
        bearer: Bearer,
        timeout_ms: i32,
        on_result: CancelResult,
        transport: Option<Transport>,
    ) -> Self {
        let _ = (bearer, on_result, transport);
        todo!("account port: CancelWorker::new ({console_url}, {timeout_ms})")
    }

    /// Queue one cancel. Returns at once.
    pub fn enqueue(&self, request_id: &str) {
        todo!("account port: CancelWorker::enqueue ({request_id})")
    }

    /// Send everything queued, then stop the worker. Returns how many cancels
    /// were sent while stopping.
    pub fn stop(&self) -> i32 {
        todo!("account port: CancelWorker::stop")
    }

    pub fn pending(&self) -> i32 {
        todo!("account port: CancelWorker::pending")
    }
}
