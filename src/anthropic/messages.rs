//! The loopback shim server (port of src/anthropic/messages.cpp). Owner: the
//! upstream / shim port.

use crate::harness::Endpoint;

pub type ModelAliases = Vec<(String, String)>;

/// A running shim, as handed to the wrapped tool.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Shim {
    pub base_url: String,
    /// The per-session loopback token. Never logged.
    pub auth_token: String,
    pub running: bool,
}

impl std::fmt::Debug for Shim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shim")
            .field("base_url", &self.base_url)
            .field("running", &self.running)
            .finish()
    }
}

/// Start the shim in front of `upstream`, serving `model`. `advertised` is the
/// model name reported to the tool (defaults to `model`); `aliases` map names
/// the tool may send onto upstream ids. C++ defaulted verbose=false,
/// advertised="" and aliases={}. Like the C++ (which returned bool), it reports
/// its own failures on stderr and returns None.
pub fn start(
    upstream: &Endpoint,
    model: &str,
    verbose: bool,
    advertised: &str,
    aliases: &ModelAliases,
) -> Option<Shim> {
    let _ = (upstream, verbose, advertised, aliases);
    todo!("shim port: Start ({model})")
}

pub fn stop(shim: &mut Shim) {
    let _ = shim;
    todo!("shim port: Stop")
}
