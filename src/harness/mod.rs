//! Launching coding tools (Claude Code, opencode, Hermes, OpenClaw, DeepSeek)
//! against a hosted or local model. One namespace, as C++ `wally::harness` was.
//!

pub mod agents;
pub mod catalog_models;
#[allow(clippy::module_inception)]
pub mod harness;
pub mod local_models;
pub mod opencode;

pub use agents::*;
pub use catalog_models::*;
pub use harness::*;
pub use local_models::*;
pub use opencode::*;
