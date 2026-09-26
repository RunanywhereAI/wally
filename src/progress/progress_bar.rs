//! Renders runanywhere.v1.DownloadProgress updates on stderr (port of
//! src/progress/progress_bar.cpp).
//!
//! TTY: single re-drawn line — stage, bar, bytes, speed, ETA.
//! Non-TTY / --no-progress: one plain line per 10% step (and per stage change)
//! so CI logs stay readable.

use std::ffi::c_void;
use std::io::Write;
use std::sync::{Arc, Mutex, Weak};

use prost::Message;

use crate::io::output;
use crate::io::proto::v1;
use crate::sys;
use crate::util::term;

fn stage_label(state: v1::DownloadState) -> &'static str {
    match state {
        v1::DownloadState::Downloading => "pulling",
        v1::DownloadState::Extracting => "extracting",
        v1::DownloadState::Validating => "verifying",
        v1::DownloadState::Completed => "done",
        _ => "preparing",
    }
}

fn speed_text(bps: f32) -> String {
    if bps <= 0.0 {
        return String::new();
    }
    format!("{}/s", output::human_bytes(bps as u64))
}

fn eta_text(eta_seconds: i64) -> String {
    if eta_seconds < 0 {
        return String::new();
    }
    if eta_seconds >= 3600 {
        format!("{}h{}m", eta_seconds / 3600, (eta_seconds % 3600) / 60)
    } else if eta_seconds >= 60 {
        format!("{}m{}s", eta_seconds / 60, eta_seconds % 60)
    } else {
        format!("{eta_seconds}s")
    }
}

fn fraction_of(p: &v1::DownloadProgress) -> f32 {
    // A legitimate 0.0 (freshly started) must be accepted, and a NaN or
    // out-of-range value must fall back to the byte ratio instead of being
    // clamped into something that looks valid.
    if p.overall_progress.is_finite() && (0.0..=1.0).contains(&p.overall_progress) {
        return p.overall_progress;
    }
    if p.total_bytes > 0 {
        return (p.bytes_downloaded as f32 / p.total_bytes as f32).min(1.0);
    }
    0.0
}

fn download_state_of(progress: &v1::DownloadProgress) -> v1::DownloadState {
    v1::DownloadState::try_from(progress.state).unwrap_or(v1::DownloadState::Unspecified)
}

fn render_bar(fraction: f32, width: i32) -> String {
    let filled = (fraction * width as f32) as i32;
    let mut bar = String::from("▕");
    for i in 0..width {
        bar.push(if i < filled { '█' } else { ' ' });
    }
    bar.push('▏');
    bar
}

pub struct ProgressRenderer {
    interactive: bool,
    line_open: bool,
    last_step: i32,
    last_stage: String,
}

impl ProgressRenderer {
    /// `interactive == false` forces plain-line mode.
    pub fn new(interactive: bool) -> Self {
        ProgressRenderer {
            interactive: interactive && term::stderr_is_tty(),
            line_open: false,
            last_step: -1,
            last_stage: String::new(),
        }
    }

    pub fn update(&mut self, progress: &v1::DownloadProgress) {
        let fraction = fraction_of(progress);
        let percent = (fraction * 100.0) as i32;
        let stage = stage_label(download_state_of(progress)).to_string();

        if !self.interactive {
            // Plain mode: line per stage change or 10%-step.
            let step = percent / 10;
            if stage != self.last_stage || step != self.last_step {
                self.last_stage = stage.clone();
                self.last_step = step;
                let mut line = format!("{stage} {} {percent}%", progress.model_id);
                // bytes_downloaded is cumulative across a multi-file plan while
                // total_bytes is per-file — only show the pair when coherent.
                if progress.total_bytes > 0 && progress.bytes_downloaded <= progress.total_bytes {
                    line += &format!(
                        " ({}/{})",
                        output::human_bytes(progress.bytes_downloaded as u64),
                        output::human_bytes(progress.total_bytes as u64)
                    );
                }
                output::status_line(&line);
            }
            return;
        }

        // Interactive: redraw one line.
        let mut line = format!("{stage} {} ", progress.model_id);
        let width = term::terminal_width();
        let bar_width = (width - line.len() as i32 - 40).clamp(10, 40);
        line += &render_bar(fraction, bar_width);
        line += &format!(" {percent:>3}%");
        if progress.total_bytes > 0 && progress.bytes_downloaded <= progress.total_bytes {
            line += &format!(
                "  {}/{}",
                output::human_bytes(progress.bytes_downloaded as u64),
                output::human_bytes(progress.total_bytes as u64)
            );
        }
        let speed = speed_text(progress.bytes_per_second);
        if !speed.is_empty() {
            line += &format!("  {speed}");
        }
        let eta = eta_text(progress.eta_seconds.unwrap_or(0));
        if !eta.is_empty() {
            line += &format!("  ETA {eta}");
        }
        if progress.total_files > 1 {
            line += &format!(
                "  [{}/{}]",
                progress.current_file_index + 1,
                progress.total_files
            );
        }

        eprint!("\r\x1b[2K{line}");
        let _ = std::io::stderr().flush();
        self.line_open = true;
    }

    /// Erase/terminate the in-place line (call before printing results).
    pub fn finish(&mut self) {
        if self.line_open {
            eprintln!();
            let _ = std::io::stderr().flush();
            self.line_open = false;
        }
    }
}

// -----------------------------------------------------------------------------
// DownloadProgressScope
// -----------------------------------------------------------------------------

struct ScopeInner {
    renderer: Mutex<ProgressRenderer>,
    model_id: String,
}

/// The process-wide "currently rendering" scope, as a `Weak` reference. A
/// callback that fires after `DownloadProgressScope::drop` has already run
/// (unregistering the callback is not a quiescence barrier — commons may have
/// an in-flight callback on another thread) upgrades this *before* the state
/// it points to can be freed, so the shared C++ "raw pointer read with no
/// synchronization, then a dangling read" race can't happen here: the Arc
/// only actually drops once every upgraded strong reference is done with it.
static ACTIVE_SCOPE: Mutex<Option<Weak<ScopeInner>>> = Mutex::new(None);

/// Registers the process-wide download progress callback and renders updates
/// for one model while alive (used by commands whose commons call may
/// auto-download, e.g. lifecycle load in `wally run`). Only one scope may be
/// active per process at a time. Events arrive on orchestrator worker threads;
/// the scope's state must outlive every in-flight callback (triage A1).
pub struct DownloadProgressScope {
    inner: Arc<ScopeInner>,
}

impl DownloadProgressScope {
    pub fn new(model_id: &str, interactive: bool) -> Self {
        let inner = Arc::new(ScopeInner {
            renderer: Mutex::new(ProgressRenderer::new(interactive)),
            model_id: model_id.to_string(),
        });
        *ACTIVE_SCOPE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&inner));
        // SAFETY: `callback` matches `rac_download_proto_progress_callback_fn`
        // exactly; it is a plain `extern "C" fn` (no captured state needing to
        // outlive this call) wrapped in `catch_unwind`, so it stays valid for
        // as long as the SDK may call it, including after this function
        // returns.
        let _ = unsafe {
            sys::rac_download_set_progress_proto_callback(Some(callback), std::ptr::null_mut())
        };
        DownloadProgressScope { inner }
    }
}

impl Drop for DownloadProgressScope {
    fn drop(&mut self) {
        // SAFETY: passing (None, null) unregisters the callback; always valid
        // to call.
        let _ =
            unsafe { sys::rac_download_set_progress_proto_callback(None, std::ptr::null_mut()) };
        *ACTIVE_SCOPE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut renderer = self
            .inner
            .renderer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        renderer.finish();
    }
}

// SAFETY: matches `rac_download_proto_progress_callback_fn`; the SDK calls
// this on its own worker threads, possibly after the owning
// `DownloadProgressScope` has already been dropped (unregistering is not a
// quiescence barrier), so the whole body is wrapped in `catch_unwind` and
// only ever touches state reached through the `Weak` upgrade above.
unsafe extern "C" fn callback(proto_bytes: *const u8, proto_size: usize, _user_data: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        let scope = {
            let guard = ACTIVE_SCOPE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().and_then(Weak::upgrade)
        };
        let Some(scope) = scope else {
            return;
        };
        if proto_bytes.is_null() {
            return;
        }
        // SAFETY: the SDK guarantees `proto_bytes` holds `proto_size` valid
        // bytes for the duration of this synchronous callback.
        let bytes = unsafe { std::slice::from_raw_parts(proto_bytes, proto_size) };
        let Ok(progress) = v1::DownloadProgress::decode(bytes) else {
            return;
        };
        let mut renderer = scope.renderer.lock().unwrap_or_else(|e| e.into_inner());
        if !scope.model_id.is_empty() && progress.model_id != scope.model_id {
            return;
        }
        renderer.update(&progress);
        let state = download_state_of(&progress);
        if matches!(
            state,
            v1::DownloadState::Completed | v1::DownloadState::Failed | v1::DownloadState::Cancelled
        ) {
            renderer.finish();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress_with(overall_progress: f32) -> v1::DownloadProgress {
        v1::DownloadProgress {
            overall_progress,
            // Stale byte fields from a previous stage: if `fraction_of`
            // ever fell through to these when `overall_progress` was a
            // legitimate value, the two would disagree.
            bytes_downloaded: 500,
            total_bytes: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn a_fresh_zero_overall_progress_is_not_rejected() {
        // Before this fix, `overall_progress > 0.0` was false for a
        // legitimate 0.0, so this fell through to the byte ratio (0.5)
        // instead of reporting the fresh-start 0.0 the SDK actually sent.
        assert_eq!(fraction_of(&progress_with(0.0)), 0.0);
    }

    #[test]
    fn nan_overall_progress_falls_back_to_the_byte_ratio() {
        assert_eq!(fraction_of(&progress_with(f32::NAN)), 0.5);
    }

    #[test]
    fn out_of_range_overall_progress_falls_back_to_the_byte_ratio() {
        assert_eq!(fraction_of(&progress_with(1.5)), 0.5);
    }

    #[test]
    fn an_in_range_overall_progress_is_used_directly() {
        assert_eq!(fraction_of(&progress_with(0.75)), 0.75);
    }
}
