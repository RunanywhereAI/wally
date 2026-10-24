//! The hosted models a coding tool is offered, with prices and limits (port of
//! src/harness/catalog_models.cpp).
//!
//! A harness shows the models its provider config lists, so wally writes every
//! model the console advertises rather than only the one the person launched.
//! The launched model stays first so it remains the default.

use crate::account::{ConsoleClient, IdentityResult, ReasoningSupport};

use super::harness::Endpoint;
use super::local_models::{local_context_size, local_output_size};

/// One catalog model with the window and price a harness needs to declare it.
/// A zero field means the catalog did not carry that number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub context_window: i64,
    pub max_output: i64,
    pub input_per_mtok: i64,
    pub output_per_mtok: i64,
    /// What a prompt token read from the server's cache costs.
    pub cached_input_per_mtok: i64,
    /// The catalog lists "image" among the model's input modalities, so a
    /// harness may attach an image to a prompt for it. False for a local
    /// model and for a model the catalog does not describe.
    pub image_input: bool,
    /// The catalog lists the model's input modalities and "image" is not
    /// among them. False when that is not known (a failed catalog read, a
    /// local model, a model the catalog does not describe), so nothing is
    /// treated as text-only on a guess.
    pub text_only: bool,
    /// How the model reasons, from the model list. None for a local model
    /// and for a model the list does not describe.
    pub reasoning: Option<ReasoningSupport>,
}

/// Moves the entry whose id is `primary` to the front, or inserts a bare one
/// when the catalog did not carry it — the launched model is always selectable.
/// What a model on `catalog` is sent for a requested `reasoning_effort`,
/// `asked`. A model whose model list entry says thinking cannot be turned off
/// is refused `none` by the gateway, so it gets its lowest level instead, or
/// no level at all when it lists none. Anything else goes through as asked.
pub fn effort_for(
    catalog: &[CatalogModel],
    model: &str,
    asked: &'static str,
) -> Option<&'static str> {
    if asked != "none" {
        return Some(asked);
    }
    match catalog
        .iter()
        .find(|entry| entry.id == model)
        .and_then(|entry| entry.reasoning.as_ref())
    {
        Some(reasoning) if !reasoning.thinking_off => reasoning.levels.first().copied(),
        _ => Some(asked),
    }
}

/// `body`'s `reasoning_effort` (a chat-completions request for `model`) made
/// one `model` accepts, per `effort_for`.
pub fn fit_reasoning_effort(body: &mut serde_json::Value, catalog: &[CatalogModel], model: &str) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    if object
        .get("reasoning_effort")
        .and_then(serde_json::Value::as_str)
        != Some("none")
    {
        return;
    }
    match effort_for(catalog, model, "none") {
        Some(level) => {
            object.insert("reasoning_effort".into(), serde_json::json!(level));
        }
        None => {
            object.remove("reasoning_effort");
        }
    }
}

/// The ids in `catalog` known to take no image input, for the translator: an
/// image the person pastes for one is refused there, and any other becomes a
/// note. Empty when the catalog could not be read, so nothing is refused on a
/// guess and the gateway judges.
pub fn text_only_ids(catalog: &[CatalogModel]) -> Vec<String> {
    catalog
        .iter()
        .filter(|model| model.text_only)
        .map(|model| model.id.clone())
        .collect()
}

fn primary_first(models: &mut Vec<CatalogModel>, primary: &str) {
    if let Some(index) = models.iter().position(|m| m.id == primary) {
        let entry = models.remove(index);
        models.insert(0, entry);
    } else {
        models.insert(
            0,
            CatalogModel {
                id: primary.to_string(),
                ..Default::default()
            },
        );
    }
}

/// Every hosted model the account can use, each with its limits and price, in
/// the console's order, and what happened asking for them: the identity result
/// and message of the model-list call, so a caller that must not guess (an
/// empty list from an offline machine is not an empty account) can tell. The
/// price call is best effort; a failed one leaves the prices at zero.
pub fn account_models_with(
    console: &ConsoleClient,
    console_url: &str,
    access_token: &str,
) -> (IdentityResult, Vec<CatalogModel>, String) {
    let mut out = Vec::new();

    let (result, models, error) = console.fetch_models(console_url, access_token);
    let (_, prices, _) = console.fetch_catalog(console_url, access_token);
    let price_for = |id: &str| -> (i64, i64, i64) {
        prices
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.input_per_mtok, p.output_per_mtok, p.cached_input_per_mtok))
            .unwrap_or((0, 0, 0))
    };

    for info in &models {
        if info.id.is_empty() {
            continue;
        }
        let (in_price, out_price, cached_price) = price_for(&info.id);
        out.push(CatalogModel {
            id: info.id.clone(),
            context_window: info.context_window,
            max_output: info.max_output_tokens,
            input_per_mtok: in_price,
            output_per_mtok: out_price,
            cached_input_per_mtok: cached_price,
            image_input: info.input_modalities.iter().any(|m| m == "image"),
            text_only: !info.input_modalities.is_empty()
                && !info.input_modalities.iter().any(|m| m == "image"),
            reasoning: info.reasoning.clone(),
        });
    }
    (result, out, error)
}

/// Every hosted model the console advertises, each with its limits and price,
/// `primary` first so it stays the harness default. Fetches the live catalog
/// through `console` from `console_url` with `access_token`; `primary` is
/// always present even if the catalog does not name it. The `console` overload
/// is the test seam.
pub fn catalog_models_with(
    console: &ConsoleClient,
    console_url: &str,
    access_token: &str,
    primary: &str,
) -> Vec<CatalogModel> {
    let (_, mut out, _) = account_models_with(console, console_url, access_token);
    primary_first(&mut out, primary);
    out
}

pub fn catalog_models(console_url: &str, access_token: &str, primary: &str) -> Vec<CatalogModel> {
    let console = ConsoleClient::default();
    catalog_models_with(&console, console_url, access_token, primary)
}

/// The same, resolved from a launch `endpoint`. A local endpoint (empty
/// `api_key`) has no catalog, so it yields just `primary` at the context size a
/// local server was started with, and an output budget that leaves room for
/// the coding prompt and conversation.
pub fn catalog_models_for(endpoint: &Endpoint, primary: &str) -> Vec<CatalogModel> {
    if endpoint.api_key.is_empty() {
        // Use the exact selected backend's limits, including when `primary` is
        // an alias or merged model id. Recomputing from that spelling can
        // produce a different window from the server we already started.
        let context = if endpoint.context_window > 0 {
            endpoint.context_window
        } else {
            local_context_size(primary)
        };
        let output = if endpoint.max_output > 0 {
            endpoint.max_output
        } else {
            local_output_size(context)
        };
        return vec![CatalogModel {
            id: primary.to_string(),
            context_window: context,
            max_output: output,
            input_per_mtok: 0,
            output_per_mtok: 0,
            cached_input_per_mtok: 0,
            image_input: false,
            text_only: false,
            reasoning: None,
        }];
    }
    catalog_models(&endpoint.console_url, &endpoint.api_key, primary)
}

#[cfg(test)]
mod effort_tests {
    use super::*;

    /// muse-spark as /v1/models describes it: thinking always on.
    fn always_thinks() -> CatalogModel {
        CatalogModel {
            id: "muse-spark".to_string(),
            reasoning: Some(ReasoningSupport {
                levels: vec!["minimal", "low", "medium", "high", "xhigh"],
                default_level: Some("medium"),
                thinking_off: false,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_model_that_cannot_stop_thinking_gets_its_lowest_level_for_none() {
        let catalog = [always_thinks()];
        assert_eq!(effort_for(&catalog, "muse-spark", "none"), Some("minimal"));
        assert_eq!(effort_for(&catalog, "muse-spark", "high"), Some("high"));
    }

    #[test]
    fn none_goes_through_where_it_turns_thinking_off_or_nothing_is_known() {
        let mut glm = always_thinks();
        glm.id = "glm-5.3-flash".to_string();
        glm.reasoning.as_mut().unwrap().thinking_off = true;
        let catalog = [always_thinks(), glm];
        assert_eq!(effort_for(&catalog, "glm-5.3-flash", "none"), Some("none"));
        assert_eq!(effort_for(&catalog, "local-model", "none"), Some("none"));
    }

    #[test]
    fn with_no_level_to_fall_back_to_the_effort_is_left_out() {
        let mut model = always_thinks();
        model.reasoning.as_mut().unwrap().levels.clear();
        let mut body = serde_json::json!({"model": "muse-spark", "reasoning_effort": "none"});
        fit_reasoning_effort(&mut body, &[model], "muse-spark");
        assert!(body.get("reasoning_effort").is_none());
        let mut body = serde_json::json!({"reasoning_effort": "none"});
        fit_reasoning_effort(&mut body, &[always_thinks()], "muse-spark");
        assert_eq!(body["reasoning_effort"], "minimal");
    }
}
