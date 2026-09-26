//! `wally models pull <model|hf.co/...|url>` (alias `wally models download`) —
//! download via the commons orchestrator: plan → start → progress callback →
//! terminal state.
//!
//! SIGINT cancels the task (partial bytes preserved → re-pull resumes via the
//! plan's can_resume path). Exit codes: 0 done, 1 failure, 130 user cancel.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use prost::Message;

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, ValueType};
use crate::commands::engine_options::{engine_choices, resolve_engine_hint};
use crate::commands::model_setup::refresh_registry;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, v1, ProtoBuffer};
use crate::progress::progress_bar::ProgressRenderer;
use crate::sys;

struct PullInner {
    last: v1::DownloadProgress,
    terminal: bool,
    got_progress: bool,
    /// filter: only this model's updates
    model_id: String,
}

struct PullShared {
    inner: Mutex<PullInner>,
    cv: Condvar,
}

/// The process has exactly one pull in flight at a time (matches the C++
/// `g_state` global); `progress_callback` reads/writes this while the SDK's
/// orchestrator thread may call it at any point between wiring and unwiring
/// the callback below.
static PULL_STATE: Mutex<Option<Arc<PullShared>>> = Mutex::new(None);

// extern "C" fn handed to the SDK as the process-wide download progress
// callback. Must never let a panic cross the FFI boundary (rule: every
// extern "C" fn wraps its body in catch_unwind).
extern "C" fn progress_callback(
    proto_bytes: *const u8,
    proto_size: usize,
    _user_data: *mut c_void,
) {
    let _ = std::panic::catch_unwind(|| {
        if proto_bytes.is_null() || proto_size == 0 {
            return;
        }
        // SAFETY: the SDK guarantees `proto_bytes` is valid for `proto_size`
        // bytes for the duration of this call only (the documented contract of
        // rac_download_proto_progress_callback_fn).
        let bytes = unsafe { std::slice::from_raw_parts(proto_bytes, proto_size) };
        let progress = match v1::DownloadProgress::decode(bytes) {
            Ok(p) => p,
            Err(_) => return,
        };
        let shared = match PULL_STATE.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => return,
        };
        let Some(shared) = shared else { return };
        let mut inner = match shared.inner.lock() {
            Ok(inner) => inner,
            Err(_) => return,
        };
        if !inner.model_id.is_empty() && progress.model_id != inner.model_id {
            return;
        }
        inner.last = progress.clone();
        inner.got_progress = true;
        match v1::DownloadState::try_from(progress.state) {
            Ok(v1::DownloadState::Completed)
            | Ok(v1::DownloadState::Failed)
            | Ok(v1::DownloadState::Cancelled) => {
                inner.terminal = true;
            }
            _ => {}
        }
        drop(inner);
        shared.cv.notify_all();
    });
}

/// Chooses the download-start failure text the same way C++'s
/// `cmd_pull.cpp` does: `!parse_proto_buffer(...) || rc != RAC_SUCCESS` is
/// the failure condition, but the printed text is chosen purely by `rc` —
/// `rc != RAC_SUCCESS ? describe_result(rc) : error`. A non-SUCCESS rc
/// always wins with the generic description, even when the buffer's own
/// envelope also carried a more specific `error_message`; the buffer's parse
/// error only surfaces when `rc` itself is SUCCESS but the envelope/decode
/// failed.
fn download_start_result(
    rc: sys::rac_result_t,
    parsed: Result<v1::DownloadStartResult, String>,
) -> Result<v1::DownloadStartResult, String> {
    if rc != sys::SUCCESS {
        Err(out::describe_result(rc))
    } else {
        parsed
    }
}

/// The "already downloaded, nothing to fetch" fast path is only safe right
/// after a rescan that actually succeeded; a failed rescan (`refresh_ok`
/// false) cannot rule out files deleted from disk since the registry was
/// last written, so it must fall through to plan/start instead of trusting a
/// stale `Downloaded` status.
fn should_report_already_downloaded(refresh_ok: bool, status: Option<i32>) -> bool {
    refresh_ok && status == Some(v1::ModelRegistryStatus::Downloaded as i32)
}

/// Shared pull flow (plan → start → progress → terminal state) for an
/// already-registered model id. Returns 0 / 1 / 130 (cancel).
pub fn pull_model_flow(options: &GlobalOptions, model_id: &str) -> i32 {
    let resolved_model_id = model_id.to_string();

    // bootstrap() registers the catalog; it does not rescan what is on disk. So
    // registry_status below can still read DOWNLOADED for a model whose files
    // were deleted since, and `wally models pull` would report success without
    // fetching anything. A refresh failure is not fatal here: the download path
    // that follows is the fallback, and refusing to pull because a rescan
    // failed would be worse than pulling something already present — but it
    // does mean a failed refresh can never be trusted to say "already
    // downloaded" (see should_report_already_downloaded below).
    let refresh_ok = match refresh_registry() {
        Ok(()) => true,
        Err(refresh_error) => {
            out::status_line(&format!(
                "could not rescan local models ({refresh_error}); continuing from the registry as it stands"
            ));
            false
        }
    };

    // The orchestrator plans from embedded metadata (it does not consult the
    // registry), so fetch the saved ModelInfo first.
    let model_info: v1::ModelInfo = {
        let mut info_out = ProtoBuffer::new();
        // SAFETY: rac_get_model_registry() returns the process-wide registry
        // handle; info_out is a valid out-param for this call only.
        let get_rc = unsafe {
            sys::rac_model_registry_get_proto_buffer(
                sys::rac_get_model_registry(),
                model_ref_c_string(&resolved_model_id).as_ptr(),
                info_out.as_mut_ptr(),
            )
        };
        // parse unconditionally: it interprets the {status,error_message}
        // envelope and frees the buffer on every path (no leak on get failure).
        match parse_proto_buffer::<v1::ModelInfo>(info_out) {
            Ok(model) if get_rc == sys::SUCCESS => model,
            Ok(_) => {
                out::error_line(&format!("model not found in registry: {resolved_model_id}"));
                return 1;
            }
            Err(error) => {
                out::error_line(&format!(
                    "model not found in registry: {resolved_model_id} ({error})"
                ));
                return 1;
            }
        }
    };

    // Already on disk: planning and starting anyway would ask the orchestrator
    // to "download" zero remaining bytes, which it reports as a download that
    // completed instantly — a progress bar animating to 100% at whatever
    // (bytes / ~0 elapsed) works out to, not a real transfer rate. Nothing to
    // fetch, so say so and stop before any of that renders. But only when the
    // rescan above actually ran: a failed refresh_ok=false status is stale by
    // construction, and falling through to plan/start below is what
    // rediscovers files deleted from disk.
    if should_report_already_downloaded(refresh_ok, model_info.registry_status) {
        if options.json {
            let mut json = out::JsonWriter::new();
            json.begin_object()
                .field_str("id", &model_info.id)
                .field_str("name", &model_info.name)
                .field_str("local_path", &model_info.local_path)
                .field_bool("already_downloaded", true)
                .end_object();
            out::result_line(json.str());
        } else {
            let suffix = if model_info.local_path.is_empty() {
                String::new()
            } else {
                format!(" → {}", model_info.local_path)
            };
            out::result_line(&format!("{} is already downloaded{suffix}", model_info.id));
        }
        return 0;
    }

    // Plan
    let plan_request = v1::DownloadPlanRequest {
        model_id: resolved_model_id.clone(),
        model: Some(model_info.clone()),
        ..Default::default()
    };
    let plan_bytes = crate::io::proto::serialize(&plan_request);

    let mut plan_out = ProtoBuffer::new();
    // SAFETY: plan_bytes/plan_out are valid for the duration of this call.
    let rc = unsafe {
        sys::rac_download_plan_proto(plan_bytes.as_ptr(), plan_bytes.len(), plan_out.as_mut_ptr())
    };
    if rc != sys::SUCCESS {
        out::error_line(&format!(
            "download plan failed: {}",
            out::describe_result(rc)
        ));
        return 1;
    }
    let plan: v1::DownloadPlanResult = match parse_proto_buffer(plan_out) {
        Ok(plan) => plan,
        Err(error) => {
            out::error_line(&format!("download plan failed: {error}"));
            return 1;
        }
    };
    if !plan.can_start {
        let reason = plan
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "plan rejected".to_string());
        out::error_line(&format!("cannot pull {resolved_model_id}: {reason}"));
        return 1;
    }
    if plan.total_bytes == 0 && plan.can_resume {
        out::status_line("resuming partial download");
    }

    // Progress wiring before start so no early events are missed.
    let shared = Arc::new(PullShared {
        inner: Mutex::new(PullInner {
            last: v1::DownloadProgress::default(),
            terminal: false,
            got_progress: false,
            model_id: resolved_model_id.clone(),
        }),
        cv: Condvar::new(),
    });
    *PULL_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(shared.clone());
    // SAFETY: progress_callback is `extern "C" fn` with the exact signature the
    // SDK expects, catch_unwind-wrapped, and stays valid for the process
    // lifetime; user_data is unused (null is documented as acceptable).
    unsafe {
        sys::rac_download_set_progress_proto_callback(Some(progress_callback), std::ptr::null_mut())
    };

    let mut renderer = ProgressRenderer::new(!options.no_progress && !options.json);

    // Start
    // skip_registry_update left unset: default (false) means "do NOT skip" —
    // the registry is updated on completion.
    let start_request = v1::DownloadStartRequest {
        model_id: resolved_model_id.clone(),
        plan: Some(plan),
        ..Default::default()
    };
    let start_bytes = crate::io::proto::serialize(&start_request);

    let mut start_out = ProtoBuffer::new();
    // SAFETY: start_bytes/start_out are valid for the duration of this call.
    let rc = unsafe {
        sys::rac_download_start_proto(
            start_bytes.as_ptr(),
            start_bytes.len(),
            start_out.as_mut_ptr(),
        )
    };
    // Always call parse_proto_buffer first so the buffer is freed on every
    // path, exactly as C++'s always-parse-then-check does.
    let start: v1::DownloadStartResult =
        match download_start_result(rc, parse_proto_buffer(start_out)) {
            Ok(start) => start,
            Err(message) => {
                unwire_progress_callback();
                out::error_line(&format!("download start failed: {message}"));
                return 1;
            }
        };
    if !start.accepted {
        unwire_progress_callback();
        let message = start.error.map(|e| e.message).unwrap_or_default();
        out::error_line(&format!("download rejected: {message}"));
        return 1;
    }

    // Wait for terminal state; SIGINT cancels once (partial bytes preserved).
    let interrupted = Arc::new(AtomicBool::new(false));
    // Ctrl-C belongs to this download only until the guard drops at the end
    // of this function; `serve` and `run` pull first and then need it back.
    let _interrupt = {
        let interrupted = interrupted.clone();
        crate::util::interrupt::on_interrupt(move || {
            interrupted.store(true, Ordering::SeqCst);
        })
    };

    let mut cancel_sent = false;
    let final_progress: v1::DownloadProgress;
    {
        let mut inner = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if inner.terminal {
                break;
            }
            let (guard, _timeout) = shared
                .cv
                .wait_timeout(inner, Duration::from_millis(200))
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
            if inner.got_progress && !inner.terminal {
                renderer.update(&inner.last);
            }
            if interrupted.load(Ordering::SeqCst) && !cancel_sent {
                cancel_sent = true;
                drop(inner);
                renderer.finish();
                out::status_line("cancelling (partial bytes kept — re-pull resumes)...");
                let cancel_request = v1::DownloadCancelRequest {
                    task_id: start.task_id.clone(),
                    model_id: resolved_model_id.clone(),
                    delete_partial_bytes: false,
                };
                let cancel_bytes = crate::io::proto::serialize(&cancel_request);
                let mut cancel_out = ProtoBuffer::new();
                // SAFETY: cancel_bytes/cancel_out are valid for the duration of this call.
                unsafe {
                    sys::rac_download_cancel_proto(
                        cancel_bytes.as_ptr(),
                        cancel_bytes.len(),
                        cancel_out.as_mut_ptr(),
                    )
                };
                inner = shared.inner.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
        final_progress = inner.last.clone();
        if !cancel_sent {
            renderer.update(&final_progress);
        }
    }
    renderer.finish();
    unwire_progress_callback();

    match v1::DownloadState::try_from(final_progress.state) {
        Ok(v1::DownloadState::Completed) => {}
        Ok(v1::DownloadState::Cancelled) => {
            out::error_line("pull cancelled");
            return 130;
        }
        _ => {
            let message = final_progress
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "download error".to_string());
            out::error_line(&format!("pull failed: {message}"));
            return 1;
        }
    }

    // Report the saved entry. parse runs unconditionally: it interprets the
    // {status,error_message} envelope and frees the buffer on every path.
    let mut model_out = ProtoBuffer::new();
    // SAFETY: as above.
    let report_rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            model_ref_c_string(&resolved_model_id).as_ptr(),
            model_out.as_mut_ptr(),
        )
    };
    match parse_proto_buffer::<v1::ModelInfo>(model_out) {
        Ok(model) if report_rc == sys::SUCCESS => {
            if options.json {
                let mut json = out::JsonWriter::new();
                json.begin_object()
                    .field_str("id", &model.id)
                    .field_str("name", &model.name)
                    .field_str("local_path", &model.local_path)
                    .field_i64("bytes", final_progress.bytes_downloaded)
                    .end_object();
                out::result_line(json.str());
            } else {
                let suffix = if model.local_path.is_empty() {
                    String::new()
                } else {
                    format!(" → {}", model.local_path)
                };
                out::result_line(&format!("pulled {}{suffix}", model.id));
            }
        }
        _ => out::result_line(&format!("pulled {resolved_model_id}")),
    }
    0
}

fn unwire_progress_callback() {
    // SAFETY: passing None unregisters the process-wide callback; valid at any time.
    unsafe { sys::rac_download_set_progress_proto_callback(None, std::ptr::null_mut()) };
    *PULL_STATE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn model_ref_c_string(model_id: &str) -> std::ffi::CString {
    // A model id can never legitimately contain a NUL; a NUL in user input
    // becomes an empty lookup (not found) rather than a panic.
    std::ffi::CString::new(model_id).unwrap_or_default()
}

pub fn configure_models_download(cmd: &mut App) {
    cmd.add_option(
        "model",
        ValueType::Text,
        "Model id, alias, hf.co/org/repo/file or URL",
    )
    .required();
    cmd.add_option(
        "--engine",
        ValueType::Text,
        &format!("Engine to fetch for ({})", engine_choices()),
    );
    cmd.callback(|p, g| {
        let Ok(_env) = bootstrap(g) else {
            return 1;
        };
        let reference = p.get_str("model").unwrap_or_default();
        let engine = p.get_str("--engine").unwrap_or_default();
        let engine_hint = match resolve_engine_hint(&engine) {
            Ok(hint) => hint,
            Err(error) => {
                out::error_line(&error);
                return 2;
            }
        };
        let resolved = match model_ref::resolve(&reference, Some(&engine_hint.resolve_options)) {
            Ok(resolved) => resolved,
            Err((_, error)) => {
                out::error_line(&error);
                return 1;
            }
        };
        pull_model_flow(g, &resolved.model_id)
    });
}

#[cfg(test)]
mod download_start_result_tests {
    use super::*;

    // When rac_download_start_proto returns a
    // non-SUCCESS rc while the out-buffer's own envelope also decodes
    // cleanly with a specific error_message, C++ always shows the generic
    // describe_result(rc) text, never the buffer's own message.
    #[test]
    fn non_success_rc_wins_over_a_cleanly_parsed_buffer_error() {
        let parsed: Result<v1::DownloadStartResult, String> =
            Err("no space left on device".to_string());
        let result = download_start_result(sys::RAC_ERROR_NOT_INITIALIZED, parsed);
        assert_eq!(
            result,
            Err(out::describe_result(sys::RAC_ERROR_NOT_INITIALIZED))
        );
        assert_ne!(result, Err("no space left on device".to_string()));
    }

    #[test]
    fn success_rc_with_parse_error_surfaces_the_parse_error() {
        let parsed: Result<v1::DownloadStartResult, String> =
            Err("failed to parse DownloadStartResult bytes".to_string());
        let result = download_start_result(sys::SUCCESS, parsed);
        assert_eq!(
            result,
            Err("failed to parse DownloadStartResult bytes".to_string())
        );
    }

    #[test]
    fn success_rc_with_ok_parse_returns_the_start_result() {
        let start = v1::DownloadStartResult {
            accepted: true,
            ..Default::default()
        };
        let result = download_start_result(sys::SUCCESS, Ok(start.clone()));
        assert_eq!(result, Ok(start));
    }
}

#[cfg(test)]
mod should_report_already_downloaded_tests {
    use super::*;

    #[test]
    fn a_failed_rescan_never_reports_already_downloaded() {
        // The bug: a stale Downloaded status survived a failed refresh and
        // returned success without fetching the files a broken rescan could
        // not see were gone.
        assert!(!should_report_already_downloaded(
            false,
            Some(v1::ModelRegistryStatus::Downloaded as i32)
        ));
    }

    #[test]
    fn a_successful_rescan_still_reports_already_downloaded() {
        assert!(should_report_already_downloaded(
            true,
            Some(v1::ModelRegistryStatus::Downloaded as i32)
        ));
    }

    #[test]
    fn a_successful_rescan_with_no_downloaded_status_falls_through() {
        assert!(!should_report_already_downloaded(true, None));
    }
}
