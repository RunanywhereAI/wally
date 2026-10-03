//! Launching coding tools (Claude Code, opencode, Hermes, OpenClaw, DeepSeek)
//! against a hosted or local model. One namespace, as C++ `wally::harness` was.

pub mod agents;
pub mod catalog_models;
pub mod declared_harness;
#[allow(clippy::module_inception)]
pub mod harness;
pub mod local_models;
pub mod opencode;
pub mod path_reload;

pub use agents::*;
pub use catalog_models::*;
pub use declared_harness::*;
pub use harness::*;
pub use local_models::*;
pub use opencode::*;

/// Hosted decision models expose probabilities rather than generated text and
/// must never be routed through chat/completions or a coding harness. The
/// console catalog cannot tell them apart (its entries carry an id, a name and
/// prices, and the gateway hides decisions-only models from /v1/models), so
/// the list is explicit. `qwev` is retired but stays refused, so an old
/// command line gets this answer rather than a gateway 404.
pub const DECISIONS_MODEL_IDS: &[&str] = &["pplx-decider-v1", "qwev"];

/// The model `wally decisions` asks when no `--model` is given.
pub const DEFAULT_DECISIONS_MODEL: &str = "pplx-decider-v1";

pub fn is_decisions_model(id: &str) -> bool {
    DECISIONS_MODEL_IDS.contains(&id)
}

/// What a chat surface prints when it is handed a decision model.
pub fn decisions_model_refusal(id: &str) -> String {
    format!("{id} is a decision model, not a chat model; use `wally decisions -m {id}` instead")
}

/// For the coding-tool commands, ahead of their is-the-tool-installed check:
/// prints the refusal and returns true when `-m` names a decision model, so
/// nobody is offered an install of a tool that could never use it.
pub fn refuse_decisions_model(model: Option<&str>) -> bool {
    match model {
        Some(id) if is_decisions_model(id) => {
            crate::io::output::error_line(&decisions_model_refusal(id));
            true
        }
        _ => false,
    }
}
