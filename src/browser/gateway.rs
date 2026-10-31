//! The two model calls a step makes, and the key they carry. A signed-in Wally
//! session is refreshed ahead of expiry, so a long run does not fail halfway
//! on a lapsed token; a key from `WALLY_BROWSER_API_KEY` is sent as given.

use std::time::Duration;

use serde_json::Value;

use crate::account::{self, ConsoleClient, Credentials};

/// The model routes retry these: rate limits and a replica going away.
const RETRY_STATUSES: &[u16] = &[429, 503, 529];
const ATTEMPTS: u32 = 3;
const TIMEOUT: Duration = Duration::from_secs(25);
const EXPIRY_SKEW_SECONDS: i64 = 60;

pub enum Key {
    Fixed(String),
    Session(Box<Credentials>),
}

pub struct Gateway {
    pub base: String,
    key: Key,
}

impl Gateway {
    pub fn fixed(base: &str, key: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            key: Key::Fixed(key),
        }
    }

    pub fn session(credentials: Credentials) -> Self {
        Self {
            base: format!("{}/v1", credentials.console_url.trim_end_matches('/')),
            key: Key::Session(Box::new(credentials)),
        }
    }

    fn bearer(&mut self) -> Result<String, String> {
        match &mut self.key {
            Key::Fixed(key) => Ok(key.clone()),
            Key::Session(credentials) => {
                if credentials.access_token_expired(account::epoch_seconds(), EXPIRY_SKEW_SECONDS) {
                    crate::harness::refresh_session(&ConsoleClient::default(), credentials)
                        .map_err(|_| {
                            "the Wally session expired; run `wally account login`".to_string()
                        })?;
                }
                Ok(credentials.access_token.clone())
            }
        }
    }

    /// POSTs `body` to `{base}{path}` and returns the JSON reply. Every
    /// failure message says no action ran, because none did.
    pub fn post(&mut self, path: &str, body: &[u8]) -> Result<Value, String> {
        let key = self.bearer()?;
        let url = format!("{}{path}", self.base);
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build();
        let agent = ureq::Agent::new_with_config(config);
        for attempt in 0..ATTEMPTS {
            let response = agent
                .post(&url)
                .header("Authorization", &format!("Bearer {key}"))
                .header("Content-Type", "application/json")
                .send(body)
                .map_err(|_| "Model connection failed; no action executed.".to_string())?;
            let status = response.status().as_u16();
            if RETRY_STATUSES.contains(&status) && attempt + 1 < ATTEMPTS {
                std::thread::sleep(Duration::from_millis(500 << attempt));
                continue;
            }
            let text = response
                .into_body()
                .read_to_string()
                .map_err(|_| "Model connection failed; no action executed.".to_string())?;
            if status >= 400 {
                let detail: String = text
                    .chars()
                    .take(400)
                    .collect::<String>()
                    .replace('\n', " ");
                return Err(format!(
                    "Model provider returned HTTP {status}; no action executed. {detail}"
                ));
            }
            return serde_json::from_str(&text)
                .map_err(|_| "The model reply was not JSON; no action executed.".to_string());
        }
        Err("Model unavailable; no action executed.".to_string())
    }
}
