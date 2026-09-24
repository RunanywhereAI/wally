//! Port of src/commands/cmd_maintenance.cpp. Owner: the maintenance/diagnostics port.
use crate::cli::App;

pub fn register_uninstall(app: &mut App) {
    let _ = app;
    todo!("maintenance/diagnostics port: register_uninstall")
}

pub fn register_help(app: &mut App) {
    let _ = app;
    todo!("maintenance/diagnostics port: register_help")
}

/// Shared by `wally uninstall` and the whole-argv `-U/--uninstall` shortcut.
pub fn run_uninstall(yes: bool) -> i32 {
    todo!("diagnostics port: run_uninstall ({yes})")
}
