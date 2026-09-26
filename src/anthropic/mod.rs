//! The loopback Anthropic-compatible shim that lets Claude Code / Claude Desktop
//! talk to an OpenAI-style upstream.

pub mod messages;
pub mod translate;

pub use messages::*;
