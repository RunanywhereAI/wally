//! Port of src/commands/cmd_backends.cpp. Owner: the maintenance/diagnostics port.
use std::collections::BTreeMap;

use crate::cli::App;

use super::EngineRow;

pub fn register_backends(app: &mut App) {
    let _ = app;
    todo!("maintenance/diagnostics port: register_backends")
}

/// Snapshot of every registered inference backend, keyed by engine name.
/// Assumes bootstrap() has already run. Shared by `wally backends` and `wally about`.
pub fn collect_backend_rows() -> BTreeMap<String, EngineRow> {
    todo!("diagnostics port: collect_backend_rows")
}

/// collect_backend_rows() narrowed to engines that serve generate_text
/// (TEMP llm-only cut; see the C++ comment in commands.h).
pub fn collect_llm_backend_rows() -> BTreeMap<String, EngineRow> {
    todo!("diagnostics port: collect_llm_backend_rows")
}
