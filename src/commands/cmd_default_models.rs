//! Port of src/commands/cmd_default_models.cpp. Owner: the models port.
use crate::cli::App;

pub fn register_default_models(app: &mut App) {
    let _ = app;
    todo!("models port: register_default_models")
}

/// The model a harness launch should use: `explicit_model` when given, else the
/// effective default (env, file, then the built-in id — see config::preferences).
/// Prints a blue notice when a default fills in for an omitted `-m`.
pub fn resolve_default_model(explicit_model: &str, no_color: bool) -> String {
    todo!("models port: ResolveDefaultModel ({explicit_model}, {no_color})")
}
