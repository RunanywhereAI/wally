//! Serializes tests that start the Anthropic translator or otherwise touch
//! process-global state it owns (`anthropic::messages`'s single `CURRENT`
//! running-instance slot, guarded loopback auth). The C++ suite ran every
//! case in one thread, one at a time, so `anthropic::Start`/`Stop` never had
//! to think about a second case's instance; `cargo test`'s default parallel
//! threads do not give us that for free. Every `#[test]` in a file that pulls
//! this in should take the guard as its first statement and hold it for the
//! whole body -- matching the C++ binary's own one-case-at-a-time order
//! rather than trying to prove which subset of cases is actually
//! independent.
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn shim_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
