//! Which coding harness a request to the model endpoint came from, declared by
//! wally rather than guessed by the server.
//!
//! InferenceInfra attributes every inference request to a harness
//! (api/app/services/harness.py `detect()`): a declared `X-RA-Harness` header
//! first, then a substring match over the User-Agent. The attribution shapes a
//! request -- with `RA_REASONING_ALLOWANCE_HARNESS_ONLY` on, only a recognised
//! harness gets the reasoning allowance on top of its `max_tokens` -- so a
//! harness that reaches the endpoint looking like a plain API client gets its
//! reasoning trace cut short.
//!
//! wally launches these tools and knows which one it started, so it says so.
//! The names are the contract's own `Harness` enum, never a retyped list, so a
//! value the server cannot store cannot be sent.

pub use crate::account::console_contract::Harness as DeclaredHarness;

/// The request header the gateway and control plane read
/// (api/app/services/harness.py `HEADER`, gateway credit_gate.py
/// `HARNESS_HEADER`).
pub const HARNESS_HEADER: &str = "X-RA-Harness";

/// The header's value: the contract's spelling, e.g. "claude_code".
pub fn harness_header_value(harness: DeclaredHarness) -> &'static str {
    harness.as_str()
}

/// The User-Agent the Anthropic bridge sends upstream, e.g.
/// "wally/0.7.0 (claude-code)".
///
/// The comment carries the harness in the User-Agent table's own spelling
/// (hyphens, `_SIGNATURES` in harness.py), not the header's, so a server whose
/// `detect()` ignores the declaration still attributes the bridge correctly.
pub fn upstream_user_agent(harness: DeclaredHarness) -> String {
    format!(
        "wally/{} ({})",
        crate::WALLY_VERSION,
        harness_header_value(harness).replace('_', "-")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pinned as literals, not re-derived from the enum: they are what
    // harness.py matches, and a spelling change on either side is a silent
    // "unknown" in production.
    #[test]
    fn declared_harness_values_match_the_server() {
        assert_eq!(HARNESS_HEADER, "X-RA-Harness");
        for (harness, value) in [
            (DeclaredHarness::KClaudeCode, "claude_code"),
            (DeclaredHarness::KClaudeDesktop, "claude_desktop"),
            (DeclaredHarness::KOpencode, "opencode"),
            (DeclaredHarness::KOpenclaw, "openclaw"),
            (DeclaredHarness::KDeepseek, "deepseek"),
        ] {
            assert_eq!(harness_header_value(harness), value);
        }
        let code = upstream_user_agent(DeclaredHarness::KClaudeCode);
        assert!(
            code.starts_with("wally/") && code.ends_with("(claude-code)"),
            "{code}"
        );
        assert!(upstream_user_agent(DeclaredHarness::KClaudeDesktop).ends_with("(claude-desktop)"));
    }
}
