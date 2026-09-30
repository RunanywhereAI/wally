//! The loopback OpenAI **Responses API** shim that lets Codex (and other
//! Responses-only clients) talk to the RunAnywhere OpenAI-style chat upstream.
//!
//! Codex 0.145.0 dropped `wire_api = "chat"`, so a Responses front door is the
//! only way it can reach a chat-completions backend. This module is a sibling of
//! `crate::anthropic`: same loopback transport, a different wire dialect.

pub mod messages;
pub mod translate;

pub use messages::*;
