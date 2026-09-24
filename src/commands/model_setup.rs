//! Resolve a model reference, ensure it is downloaded, find its artifact (port
//! of src/commands/model_setup.cpp). Owner: the run/llm/tool/serve port.

use crate::bootstrap::GlobalOptions;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedModelPaths {
    pub model_id: String,
    pub display_name: String,
    /// resolved artifact (file or inner directory)
    pub primary_path: String,
}

/// Err is the exit code the command should return (errors already printed).
pub fn ensure_model_ready(
    options: &GlobalOptions,
    reference: &str,
) -> Result<ResolvedModelPaths, i32> {
    todo!("run port: ensure_model_ready ({options:?}, {reference})")
}

pub fn refresh_registry() -> Result<(), String> {
    todo!("run port: refresh_registry")
}
