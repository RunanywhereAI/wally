//! Port of src/commands/cmd_pull.cpp. Owner: the models port.
use crate::bootstrap::GlobalOptions;
use crate::cli::App;

pub fn configure_models_download(cmd: &mut App) {
    let _ = cmd;
    todo!("models port: configure_models_download")
}

/// Shared pull flow (plan → start → progress → terminal state) for an
/// already-registered model id. Returns 0 / 1 / 130 (cancel).
pub fn pull_model_flow(options: &GlobalOptions, model_id: &str) -> i32 {
    todo!("models port: pull_model_flow ({options:?}, {model_id})")
}
