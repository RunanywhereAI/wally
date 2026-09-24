//! The loopback Anthropic-compatible shim that lets Claude Code / Claude Desktop
//! talk to an OpenAI-style upstream. Owner: the upstream / shim port.

pub mod messages;
pub mod translate;

pub use messages::*;
