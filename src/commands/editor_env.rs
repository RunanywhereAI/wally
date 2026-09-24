//! The `open` argv that starts a macOS app bundle (Claude Desktop) with the shim
//! variables set (C++ editor_env.h; defined in cmd_editors.cpp). Owner: the
//! coding-tool harness port.

use crate::anthropic::Shim;

pub fn open_args(bundle: &str, shim: &Shim, passthrough: &[String], model: &str) -> Vec<String> {
    let _ = shim;
    todo!("harness port: OpenArgs ({bundle}, {passthrough:?}, {model})")
}
