//! The `open` argv that starts a macOS app bundle (Claude Desktop) with the shim
//! variables set (C++ editor_env.h; defined in cmd_editors.cpp).
//!
//! `open --env` is the only way in: launchd starts a bundle from the reader's
//! login session, so nothing the parent process exports reaches it. Anything the
//! terminal path sets with a scoped environment variable has to be listed here
//! too, or a desktop launch silently runs unwired.
//!
//! `model` empty means no wiring was asked for, and the model variables are then
//! omitted rather than exported empty -- an empty value reads as "unset this" to
//! the wrapped tool, which is not the same as leaving its own configuration
//! alone.

use crate::anthropic::Shim;

/// `open(1)` arguments that launch a macOS bundle against `shim`, carrying the
/// model as well as the endpoint.
pub fn open_args(bundle: &str, shim: &Shim, passthrough: &[String], model: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-n".to_string(), "-W".to_string()];
    if shim.running {
        // `open --env` is what carries them across; launchd would otherwise
        // start the app with the reader's login environment instead of ours.
        args.push("--env".to_string());
        args.push(format!("ANTHROPIC_BASE_URL={}", shim.base_url));
        // Bearer token only, no ANTHROPIC_API_KEY: the token outranks a key and
        // setting a key is what makes Claude Code warn about claude.ai
        // connectors being off. See the ScopedEnv path in cmd_editors.rs.
        args.push("--env".to_string());
        args.push(format!("ANTHROPIC_AUTH_TOKEN={}", shim.auth_token));
        // The same two the terminal path sets, for the same reason: without them
        // the wrapped app reports its own default model as the one that answered.
        // A bundle gets no inherited environment at all -- launchd starts it from
        // the reader's login session -- so anything the terminal path sets with
        // ScopedEnv has to be listed here too or the desktop launch silently
        // keeps the wrong labels. `model` is empty on the no-wiring path, and an
        // empty value would read as "unset this", so both are conditional.
        if !model.is_empty() {
            args.push("--env".to_string());
            args.push(format!("ANTHROPIC_MODEL={model}"));
            args.push("--env".to_string());
            args.push(format!("ANTHROPIC_DEFAULT_HAIKU_MODEL={model}"));
        }
    }
    args.push("-a".to_string());
    args.push(bundle.to_string());
    if !passthrough.is_empty() {
        args.push("--args".to_string());
        args.extend(passthrough.iter().cloned());
    }
    args
}
