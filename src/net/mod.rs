//! Networking: the control-plane wiring (SDK transport), the loopback token the
//! coding-tool proxies hand their tools, and the upstream client + minimal
//! HTTP/1.1 layer behind the Anthropic shim.

pub mod control_plane;
pub mod decompress;
pub mod http1;
pub mod loopback_auth;
pub mod upstream_call;
pub mod upstream_pool;

pub use control_plane::*;
pub use loopback_auth::*;
