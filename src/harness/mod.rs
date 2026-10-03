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
/// must never be routed through chat/completions or a coding harness.
pub const DECISIONS_MODEL_IDS: &[&str] = &["qwev"];

pub fn is_decisions_model(id: &str) -> bool {
    DECISIONS_MODEL_IDS.contains(&id)
}
