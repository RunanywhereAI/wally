//! The process's one Ctrl-C handler (also SIGTERM, and SIGHUP on Unix).
//!
//! `ctrlc` accepts a single `set_handler` per process: a second call fails and
//! the first handler keeps running. `wally serve` and `wally run` pull a
//! missing model before they start, and the pull's handler used to win, so
//! afterwards Ctrl-C only set a finished download's flag and the server could
//! not be stopped short of SIGKILL. Commands now swap their action in here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};

type Action = Arc<dyn Fn() + Send + Sync>;

static INSTALL: Once = Once::new();
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static CURRENT: Mutex<Option<(u64, Action)>> = Mutex::new(None);

/// Makes `action` what an interrupt does until the returned guard drops.
/// With no action in place an interrupt ends the process, as it would with
/// no handler installed at all.
///
/// `action` runs on `ctrlc`'s own thread, not in signal context, so it may
/// lock and allocate; it should still only flag the work to stop.
#[must_use = "the action is removed as soon as the guard drops"]
pub fn on_interrupt(action: impl Fn() + Send + Sync + 'static) -> InterruptGuard {
    INSTALL.call_once(|| {
        // Errors are ignored, as C++ never checked std::signal's SIG_ERR:
        // without a handler the default disposition still ends the process.
        let _ = ctrlc::set_handler(|| {
            let action = current().as_ref().map(|(_, action)| Arc::clone(action));
            match action {
                Some(action) => action(),
                None => exit_interrupted(),
            }
        });
    });
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *current() = Some((id, Arc::new(action)));
    InterruptGuard { id }
}

/// Removes its action on drop, unless a later `on_interrupt` replaced it.
pub struct InterruptGuard {
    id: u64,
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        let mut current = current();
        if current.as_ref().is_some_and(|(id, _)| *id == self.id) {
            *current = None;
        }
    }
}

fn current() -> std::sync::MutexGuard<'static, Option<(u64, Action)>> {
    CURRENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the default disposition would do: end now. `_exit` skips exit
/// handlers, which could otherwise run while another thread is still inside
/// the SDK.
fn exit_interrupted() -> ! {
    #[cfg(unix)]
    // SAFETY: _exit has no preconditions and does not return.
    unsafe {
        libc::_exit(130)
    }
    #[cfg(not(unix))]
    std::process::exit(130)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn fire() {
        let action = current().as_ref().map(|(_, action)| Arc::clone(action));
        if let Some(action) = action {
            action();
        }
    }

    // The regression: a pull's handler must not outlive the pull, and the
    // next command's action must be the one that runs.
    #[test]
    fn the_latest_action_runs_and_a_dropped_guard_removes_only_its_own() {
        static PULL: AtomicBool = AtomicBool::new(false);
        static SERVE: AtomicBool = AtomicBool::new(false);

        let pull = on_interrupt(|| PULL.store(true, Ordering::SeqCst));
        drop(pull);
        let serve = on_interrupt(|| SERVE.store(true, Ordering::SeqCst));
        fire();
        assert!(SERVE.load(Ordering::SeqCst), "serve's action must run");
        assert!(!PULL.load(Ordering::SeqCst), "the finished pull's must not");

        // A stale guard dropping late must not clear the newer action.
        let older = on_interrupt(|| {});
        let newer = on_interrupt(|| SERVE.store(false, Ordering::SeqCst));
        drop(older);
        fire();
        assert!(!SERVE.load(Ordering::SeqCst), "the newer action survived");
        drop(newer);
        drop(serve);
        assert!(current().is_none());
    }
}
