//! Port of src/commands/cmd_update.cpp. Owner: the maintenance/diagnostics port.
use crate::cli::App;

pub fn register_update(app: &mut App) {
    let _ = app;
    todo!("maintenance/diagnostics port: register_update")
}

/// Shared by `wally update` and the whole-argv `-u/--update` shortcut.
pub fn run_update(nightly: bool) -> i32 {
    todo!("diagnostics port: run_update ({nightly})")
}
