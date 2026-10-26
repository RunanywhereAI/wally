//! Shared hosted-account access for security-tool launchers.

use crate::account::{self, ConsoleClient, IdentityResult};
use crate::harness::catalog_models::{catalog_models_with, CatalogModel};
use crate::harness::harness::{
    model_id_is_safe, refresh_session, report_cloud_session_invalid, report_not_signed_in,
    verify_cloud_session,
};
use crate::io::output as out;

/// A console Cloud key, for a scan that outlasts a signed-in session.
pub const CLOUD_KEY_VARIABLE: &str = "WALLY_CLOUD_KEY";

/// A session that would end sooner than this is refreshed before the scan
/// starts: a scan runs for hours, and these tools cannot refresh anything.
const REFRESH_WITHIN_SECONDS: i64 = 4 * 60 * 60;

/// Where the models are served from and the key that reads them.
pub struct Hosted {
    pub console_url: String,
    pub key: String,
    pub session: bool,
}

fn cloud_key_from_environment() -> Option<String> {
    let value = std::env::var(CLOUD_KEY_VARIABLE).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// `WALLY_CLOUD_KEY` if set, else the signed-in session, refreshed first when
/// it would end within four hours.
pub fn hosted_access(console: &ConsoleClient, tool: &str) -> Option<Hosted> {
    let mut credentials = match account::load() {
        Ok(credentials) => credentials,
        Err(message) => {
            out::error_line(&message);
            return None;
        }
    };

    if let Some(key) = cloud_key_from_environment() {
        if !account::session_token_is_safe(&key) {
            out::error_line(&format!(
                "{CLOUD_KEY_VARIABLE} holds characters a key cannot contain"
            ));
            return None;
        }
        return Some(Hosted {
            console_url: credentials.console_url,
            key,
            session: false,
        });
    }

    if !credentials.signed_in() {
        report_not_signed_in();
        out::status_line(&format!(
            "or set {CLOUD_KEY_VARIABLE} to a Cloud key from the console"
        ));
        return None;
    }
    if !credentials.refresh_token.is_empty()
        && credentials.access_token_expired(account::epoch_seconds(), REFRESH_WITHIN_SECONDS)
    {
        if let Err(error) = refresh_session(console, &mut credentials) {
            let usable =
                error.unverified && !credentials.access_token_expired(account::epoch_seconds(), 60);
            if !usable {
                report_cloud_session_invalid(tool);
                return None;
            }
        }
    }
    match verify_cloud_session(console, &mut credentials) {
        Ok(email) => {
            if !email.is_empty() {
                out::status_line(&format!("signed in as {email}"));
            }
        }
        Err(error) if error.unverified => {
            out::status_line(&format!(
                "could not confirm the cloud session ({}) - continuing on the stored session",
                error.message
            ));
        }
        Err(_) => {
            report_cloud_session_invalid(tool);
            return None;
        }
    }
    let remaining = credentials.expires_at - account::epoch_seconds();
    if credentials.expires_at > 0 && remaining < REFRESH_WITHIN_SECONDS {
        out::status_line(&format!(
            "this sign-in stays valid for about {} minutes. For a longer scan, create a Cloud \
             key in the console and set {CLOUD_KEY_VARIABLE}.",
            remaining.max(0) / 60
        ));
    }
    Some(Hosted {
        console_url: credentials.console_url,
        key: credentials.access_token,
        session: true,
    })
}

pub fn not_available_message(model: &str) -> String {
    format!(
        "'{model}' is not available on this account. If it needs approval, request access in \
         the console."
    )
}

/// The model the scan is pointed at: `requested` if the account has it, else
/// the preferred default if it has that, else the first model on the list.
pub fn choose_model(
    models: &[CatalogModel],
    requested: Option<&str>,
    preferred_default: &str,
) -> Result<String, String> {
    if let Some(id) = requested {
        return if models.iter().any(|model| model.id == id) {
            Ok(id.to_string())
        } else {
            Err(not_available_message(id))
        };
    }
    if models.iter().any(|model| model.id == preferred_default) {
        return Ok(preferred_default.to_string());
    }
    models
        .first()
        .map(|model| model.id.clone())
        .ok_or_else(|| "your account has no models available for a security scan".to_string())
}

pub fn require_safe_model(requested: Option<&str>) -> Result<(), i32> {
    if let Some(id) = requested {
        if !model_id_is_safe(id) {
            out::error_line(&format!("'{id}' is not a valid model id"));
            return Err(2);
        }
    }
    Ok(())
}

pub fn load_models(
    console: &ConsoleClient,
    hosted: &Hosted,
    primary: &str,
) -> Result<Vec<CatalogModel>, i32> {
    let (result, _, error) = console.fetch_models(&hosted.console_url, &hosted.key);
    match result {
        IdentityResult::Ok => {}
        IdentityResult::Unauthorized if hosted.session => {
            report_cloud_session_invalid("this scan");
            return Err(1);
        }
        IdentityResult::Unauthorized => {
            out::error_line(&format!(
                "the console did not accept {CLOUD_KEY_VARIABLE}; check the key in the console"
            ));
            return Err(1);
        }
        _ => {
            out::error_line(&format!("could not read your model list: {error}"));
            return Err(1);
        }
    }
    let mut models = catalog_models_with(console, &hosted.console_url, &hosted.key, primary);
    models.retain(|model| !super::is_decisions_model(&model.id));
    Ok(models)
}

pub fn pick_model(
    console: &ConsoleClient,
    hosted: &Hosted,
    requested: Option<&str>,
) -> Result<(String, String), i32> {
    let preferred = crate::config::preferences::resolve_model("");
    let primary = requested.unwrap_or(preferred.as_str());
    let models = load_models(console, hosted, primary)?;
    let model = match choose_model(&models, requested, &preferred) {
        Ok(model) => model,
        Err(message) => {
            out::error_line(&message);
            return Err(1);
        }
    };
    if requested.is_none() {
        out::status_line(&format!("model not provided, using default model {model}"));
    }
    let base_url = format!("{}/v1", hosted.console_url);
    Ok((model, base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            ..CatalogModel::default()
        }
    }

    #[test]
    fn choose_model_prefers_requested_then_default_then_first() {
        let models = vec![model("a"), model("b"), model("c")];
        assert_eq!(choose_model(&models, Some("b"), "a").unwrap(), "b");
        assert_eq!(choose_model(&models, None, "c").unwrap(), "c");
        assert_eq!(choose_model(&models, None, "missing").unwrap(), "a");
        assert!(choose_model(&models, Some("nope"), "a").is_err());
        assert!(choose_model(&[], None, "a").is_err());
    }
}
