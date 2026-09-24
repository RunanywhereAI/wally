//! The per-session secret a loopback proxy hands the tool it launched (port of
//! src/net/loopback_auth.cpp). Owner: the upstream / shim port.

/// A fresh random secret: 32 bytes of OS CSPRNG output rendered as hex.
pub fn generate_loopback_token() -> String {
    todo!("shim port: GenerateLoopbackToken")
}

/// A length-independent equality check, so a caller cannot learn the secret one
/// byte at a time from how long a rejection takes.
pub fn constant_time_equals(a: &str, b: &str) -> bool {
    let _ = (a, b);
    todo!("shim port: ConstantTimeEquals")
}
