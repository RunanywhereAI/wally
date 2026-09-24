//! The hosted models a coding tool is offered, with prices and limits (port of
//! src/harness/catalog_models.cpp).
//!
//! A harness shows the models its provider config lists, so wally writes every
//! model the console advertises rather than only the one the person launched.
//! The launched model stays first so it remains the default.

use crate::account::ConsoleClient;

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
}

/// Moves the entry whose id is `primary` to the front, or inserts a bare one
/// when the catalog did not carry it — the launched model is always selectable.
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
    let mut out = Vec::new();

    let (_, models, _) = console.fetch_models(console_url, access_token);
    let (_, prices, _) = console.fetch_catalog(console_url, access_token);
    let price_for = |id: &str| -> (i64, i64) {
        prices
            .iter()
            .find(|p| p.id == id)
            .map(|p| (p.input_per_mtok, p.output_per_mtok))
            .unwrap_or((0, 0))
    };

    for info in &models {
        if info.id.is_empty() {
            continue;
        }
        let (in_price, out_price) = price_for(&info.id);
        out.push(CatalogModel {
            id: info.id.clone(),
            context_window: info.context_window,
            max_output: info.max_output_tokens,
            input_per_mtok: in_price,
            output_per_mtok: out_price,
        });
    }
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
        }];
    }
    catalog_models(&endpoint.console_url, &endpoint.api_key, primary)
}
