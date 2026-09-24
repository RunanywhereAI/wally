//! The hosted models a coding tool is offered, with prices and limits (port of
//! src/harness/catalog_models.cpp).

use crate::account::ConsoleClient;

use super::harness::Endpoint;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogModel {
    pub id: String,
    pub context_window: i64,
    pub max_output: i64,
    pub input_per_mtok: i64,
    pub output_per_mtok: i64,
}

pub fn catalog_models_with(
    console: &ConsoleClient,
    console_url: &str,
    access_token: &str,
    primary: &str,
) -> Vec<CatalogModel> {
    let _ = (console, access_token);
    todo!("harness port: CatalogModels(console, …) ({console_url}, {primary})")
}

pub fn catalog_models(console_url: &str, access_token: &str, primary: &str) -> Vec<CatalogModel> {
    let _ = access_token;
    todo!("harness port: CatalogModels(url, …) ({console_url}, {primary})")
}

pub fn catalog_models_for(endpoint: &Endpoint, primary: &str) -> Vec<CatalogModel> {
    let _ = endpoint;
    todo!("harness port: CatalogModels(endpoint, …) ({primary})")
}
