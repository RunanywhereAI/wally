//! The stored console credential, console/browser origin rules and profile
//! paths (port of src/account/credentials.cpp). Owner: the account port.

/// The credential is a normal API key with the customer's credit behind it:
/// `Debug` never prints the tokens.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub console_url: String,
    pub email: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("console_url", &self.console_url)
            .field("email", &self.email)
            .field(
                "access_token",
                &if self.access_token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field(
                "refresh_token",
                &if self.refresh_token.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl Credentials {
    pub fn signed_in(&self) -> bool {
        !self.access_token.is_empty()
    }

    /// `skew_seconds` defaulted to 60 in C++.
    pub fn access_token_expired(&self, now: i64, skew_seconds: i64) -> bool {
        self.expires_at > 0 && self.expires_at <= now + skew_seconds
    }
}

/// The console API used when neither a login flag nor WALLY_CONSOLE_URL is set.
pub fn default_console_url() -> String {
    todo!("account port: DefaultConsoleUrl")
}

/// The browser origins allowed to host the approval page for `console_url`.
/// Never empty.
pub fn trusted_browser_origins(console_url: &str) -> Vec<String> {
    todo!("account port: TrustedBrowserOrigins ({console_url})")
}

/// The baked development control plane, normalised, or empty when none baked.
pub fn baked_console_api_url() -> String {
    todo!("account port: BakedConsoleApiUrl")
}

pub fn browser_url_is_trusted(url: &str, origins: &[String]) -> bool {
    todo!("account port: BrowserUrlIsTrusted ({url}, {origins:?})")
}

/// Validate and canonicalize a console origin. HTTPS is required except for an
/// exact loopback host. Origins may not contain credentials, paths or queries.
pub fn normalize_console_url(input: &str) -> Result<String, String> {
    todo!("account port: NormalizeConsoleUrl ({input})")
}

/// Browser URLs may include a path/query but obey the same HTTPS/loopback rule.
pub fn browser_url_is_safe(url: &str) -> bool {
    todo!("account port: BrowserUrlIsSafe ({url})")
}

pub fn browser_url_matches_console(url: &str, console_url: &str) -> bool {
    todo!("account port: BrowserUrlMatchesConsole ({url}, {console_url})")
}

/// Bearer/refresh tokens are constrained to RFC 6750 b64token characters.
pub fn session_token_is_safe(token: &str) -> bool {
    let _ = token;
    todo!("account port: SessionTokenIsSafe")
}

pub fn profile_directory() -> String {
    todo!("account port: ProfileDirectory")
}

pub fn credentials_path() -> String {
    todo!("account port: CredentialsPath")
}

/// Missing credentials are not an error; the result then carries the default
/// console URL. (C++ `bool Load(Credentials*, std::string*)`.)
pub fn load() -> Result<Credentials, String> {
    todo!("account port: Load")
}

/// C++'s legacy `Credentials Load()` overload for the editor/harness code: an
/// empty session on read failure.
pub fn load_or_empty() -> Credentials {
    todo!("account port: Load() compatibility overload")
}

pub fn save(credentials: &Credentials) -> Result<(), String> {
    let _ = credentials;
    todo!("account port: Save")
}

pub fn clear() -> Result<(), String> {
    todo!("account port: Clear")
}
