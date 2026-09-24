//! The per-session secret a loopback proxy hands the tool it launched (port of
//! src/net/loopback_auth.cpp). Owner: the upstream / shim port.
//!
//! The coding-harness proxies bind 127.0.0.1 and forward to the hosted API on
//! the signed-in user's credit. The port is reachable by any other process
//! running as the same local user, so the proxy must prove its caller is the
//! tool it launched and not a bystander. It does that by generating one of
//! these per session, giving it to the tool as its API key, and rejecting any
//! request that does not present it.
//!
//! The value is 32 bytes of the OS CSPRNG rendered as hex. This is not a
//! cryptographic key exchange, only a same-host capability token, so that is
//! enough. Compared byte-for-byte with a constant-time check at the boundary.

/// A fresh random secret: 32 bytes of OS CSPRNG output rendered as hex.
pub fn generate_loopback_token() -> String {
    let mut bytes = [0u8; 32];
    // SAFETY: getrandom::fill only writes into the provided buffer; a failure
    // here means the OS CSPRNG is unavailable, which we treat as fatal like
    // the C++ std::random_device would (it throws on construction failure).
    getrandom::fill(&mut bytes).expect("OS CSPRNG unavailable");
    let mut token = String::with_capacity(64);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    token
}

/// A length-independent equality check, so a caller cannot learn the secret one
/// byte at a time from how long a rejection takes.
pub fn constant_time_equals(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference: u8 = 0;
    for (x, y) in a.bytes().zip(b.bytes()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_64_hex_chars() {
        let token = generate_loopback_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn tokens_are_not_trivially_equal() {
        let a = generate_loopback_token();
        let b = generate_loopback_token();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_equals_matches_string_equality() {
        assert!(constant_time_equals("abc", "abc"));
        assert!(!constant_time_equals("abc", "abd"));
        assert!(!constant_time_equals("abc", "ab"));
        assert!(!constant_time_equals("", "a"));
        assert!(constant_time_equals("", ""));
    }
}
