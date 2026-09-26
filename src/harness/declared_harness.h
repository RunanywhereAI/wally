#ifndef WALLY_HARNESS_DECLARED_HARNESS_H
#define WALLY_HARNESS_DECLARED_HARNESS_H

#include <string>

#include "account/console_contract.h"

/// Which coding harness a request to the model endpoint came from, declared by
/// wally rather than guessed by the server.
///
/// InferenceInfra attributes every inference request to a harness
/// (api/app/services/harness.py `detect()`): a declared `X-RA-Harness` header
/// first, then a substring match over the User-Agent. The attribution shapes a
/// request -- with `RA_REASONING_ALLOWANCE_HARNESS_ONLY` on, only a recognised
/// harness gets the reasoning allowance on top of its `max_tokens` -- so a
/// harness that reaches the endpoint looking like a plain API client gets its
/// reasoning trace cut short.
///
/// wally launches these tools and knows exactly which one it started, so it
/// says so instead of leaving the server to read an agent string wally did not
/// write. The names are the contract's own `Harness` enum (the generated
/// binding, never a retyped list), so a value the server cannot store cannot
/// be sent.
namespace wally::harness {

using DeclaredHarness = account::contract::Harness;

/// The request header the gateway and control plane read
/// (api/app/services/harness.py `HEADER`, gateway credit_gate.py
/// `HARNESS_HEADER`).
inline constexpr const char kHarnessHeader[] = "X-RA-Harness";

/// The header's value: the contract's spelling, e.g. "claude_code".
std::string HarnessHeaderValue(DeclaredHarness harness);

/// The User-Agent the Anthropic bridge sends upstream, e.g.
/// "wally/0.6.1 (claude-code)".
///
/// The comment carries the harness in the User-Agent table's own spelling
/// (hyphens, `_SIGNATURES` in harness.py), not the header's. That is
/// deliberate: a server whose `detect()` accepts a declaration only for its
/// own first-party callers ignores `X-RA-Harness: claude_code` and falls back
/// to the User-Agent, and this one then still attributes to the right harness.
/// It also names wally and its version in the endpoint's request logs.
std::string UpstreamUserAgent(DeclaredHarness harness);

}  // namespace wally::harness

#endif  // WALLY_HARNESS_DECLARED_HARNESS_H
