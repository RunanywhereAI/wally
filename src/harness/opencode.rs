//! opencode with a hosted model (port of src/harness/opencode.cpp).

use crate::account::ConsoleClient;

use super::catalog_models::CatalogModel;

/// Spawns the tool; returns its exit code (tests inject a fake).
pub type SpawnFunction = std::sync::Arc<dyn Fn(&str, &[String]) -> i32 + Send + Sync>;

pub fn build_open_code_cloud_config(
    primary: &str,
    base_url: &str,
    access_token: &str,
    models: &[CatalogModel],
) -> String {
    let _ = (access_token, models);
    todo!("harness port: BuildOpenCodeCloudConfig ({primary}, {base_url})")
}

pub fn launch_open_code_cloud(model: &str, arguments: &[String]) -> i32 {
    todo!("harness port: LaunchOpenCodeCloud ({model}, {arguments:?})")
}

pub fn launch_open_code_cloud_with(
    model: &str,
    arguments: &[String],
    console: &ConsoleClient,
    spawn: &SpawnFunction,
) -> i32 {
    let _ = (console, spawn);
    todo!("harness port: LaunchOpenCodeCloud with seams ({model}, {arguments:?})")
}
