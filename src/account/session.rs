//! A signed-in console session for the commands that read the account: load the
//! saved credential, refresh it ahead of expiry, and answer a 401 with one
//! refresh and one retry. Every account read goes through here, so the rule for
//! when a session is refreshed is written once.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{ConsoleClient, Credentials, IdentityResult, RefreshError};

/// A token this close to its expiry is refreshed before it is sent, so a call
/// is never made with a token that lapses on the way.
const EXPIRY_SKEW_SECONDS: i64 = 60;

/// What a grant that names no lifetime is taken to last.
const DEFAULT_GRANT_SECONDS: i64 = 3600;

/// Seconds since the Unix epoch, or 0 on a clock set before it.
pub fn epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}

pub struct ConsoleSession {
    client: ConsoleClient,
    credentials: Credentials,
    /// The clock the session was opened on. Expiry is judged and a refreshed
    /// deadline is stamped on this one reading, so a caller that pins it (a
    /// test) sees both on the same timeline.
    now: i64,
}

impl ConsoleSession {
    /// The saved session, refreshed first when it is about to expire.
    pub fn open(client: ConsoleClient) -> Result<Self, String> {
        let credentials = super::load()?;
        Self::resume(client, credentials, epoch_seconds())
    }

    /// `open` for a caller that already holds the credential.
    pub fn resume(
        client: ConsoleClient,
        credentials: Credentials,
        now: i64,
    ) -> Result<Self, String> {
        if !credentials.signed_in() {
            return Err("not signed in — run `wally account login`".to_string());
        }
        let mut session = ConsoleSession {
            client,
            credentials,
            now,
        };
        if session
            .credentials
            .access_token_expired(now, EXPIRY_SKEW_SECONDS)
        {
            session.refresh().map_err(|e| {
                if e.unavailable || e.message.contains("wally account login") {
                    e.message
                } else {
                    format!("{}; run `wally account login`", e.message)
                }
            })?;
        }
        Ok(session)
    }

    /// Trades the refresh token for a new access token and saves the result.
    /// `unavailable` on the error keeps "the console is busy, try again" apart
    /// from "this session is gone, log in again": they need different actions.
    fn refresh(&mut self) -> Result<(), RefreshError> {
        let credentials = &mut self.credentials;
        if credentials.refresh_token.is_empty() {
            return Err(RefreshError {
                message: "the cloud session cannot be refreshed".to_string(),
                unavailable: false,
            });
        }
        let grant = self
            .client
            .refresh(&credentials.console_url, &credentials.refresh_token)?;
        credentials.access_token = grant.access_token;
        if !grant.refresh_token.is_empty() {
            credentials.refresh_token = grant.refresh_token;
        }
        credentials.expires_at = self.now
            + if grant.expires_in > 0 {
                grant.expires_in
            } else {
                DEFAULT_GRANT_SECONDS
            };
        super::save(credentials).map_err(|message| RefreshError {
            message,
            unavailable: false,
        })
    }

    /// Runs one console read. A 401 is answered by refreshing once and asking
    /// again; any other failure is returned as the read phrased it.
    pub fn call<T>(
        &mut self,
        mut read: impl FnMut(&ConsoleClient, &str, &str) -> (IdentityResult, T, String),
    ) -> Result<T, String> {
        let (mut result, mut value, mut failure) = read(
            &self.client,
            &self.credentials.console_url,
            &self.credentials.access_token,
        );
        if result == IdentityResult::Unauthorized {
            // Not "expired". The console answers unknown, revoked, expired and
            // malformed with the same 401 on purpose, so which of the four this
            // was is not something we know -- and sending someone to re-login
            // over a revoked key wastes the trip.
            match self.refresh() {
                Ok(()) => {}
                // A 429 or 5xx on the refresh says nothing about the session,
                // and logging in again would not get past a console that is
                // busy. The message already says to try again shortly.
                Err(e) if e.unavailable => return Err(e.message),
                Err(e) => {
                    return Err(format!(
                        "the console rejected this session ({}); run `wally account login`",
                        e.message
                    ))
                }
            }
            (result, value, failure) = read(
                &self.client,
                &self.credentials.console_url,
                &self.credentials.access_token,
            );
        }
        if result == IdentityResult::Ok {
            Ok(value)
        } else {
            Err(failure)
        }
    }
}
