//! Renders runanywhere.v1.DownloadProgress updates on stderr (port of
//! src/progress/progress_bar.cpp). Owner: the SDK bootstrap / device info /
//! progress port.
//!
//! TTY: single re-drawn line — stage, bar, bytes, speed, ETA.
//! Non-TTY / --no-progress: one plain line per 10% step (and per stage change)
//! so CI logs stay readable.

use crate::io::proto::v1;

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
            interactive,
            line_open: false,
            last_step: -1,
            last_stage: String::new(),
        }
    }

    pub fn update(&mut self, progress: &v1::DownloadProgress) {
        let _ = (
            progress,
            self.interactive,
            self.line_open,
            self.last_step,
            &self.last_stage,
        );
        todo!("progress port: ProgressRenderer::update")
    }

    /// Erase/terminate the in-place line (call before printing results).
    pub fn finish(&mut self) {
        todo!("progress port: ProgressRenderer::finish")
    }
}

/// Registers the process-wide download progress callback and renders updates
/// for one model while alive (used by commands whose commons call may
/// auto-download, e.g. lifecycle load in `wally run`). Only one scope may be
/// active per process at a time. Events arrive on orchestrator worker threads;
/// the scope's state must outlive every in-flight callback (triage A1).
pub struct DownloadProgressScope {
    _private: (),
}

impl DownloadProgressScope {
    pub fn new(model_id: &str, interactive: bool) -> Self {
        let _ = (model_id, interactive);
        todo!("progress port: DownloadProgressScope::new")
    }
}

impl Drop for DownloadProgressScope {
    fn drop(&mut self) {}
}
