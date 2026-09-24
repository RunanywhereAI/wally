//! The coding-agent table and the per-agent config builders (port of
//! src/harness/agents.cpp).

use super::catalog_models::CatalogModel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handoff {
    CustomEndpointEnvironment,
    ConfigFile,
    PatchOverlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    pub id: &'static str,
    pub command: &'static str,
    pub summary: &'static str,
    pub handoff: Handoff,
    pub default_args: &'static str,
}

/// C++ `kAgents` / `kAgentCount`.
pub fn agents() -> &'static [Agent] {
    todo!("harness port: kAgents")
}

pub fn build_open_claw_config(
    existing: &str,
    primary: &str,
    base_url: &str,
    api_key: &str,
    models: &[CatalogModel],
) -> String {
    let _ = (existing, api_key, models);
    todo!("harness port: BuildOpenClawConfig ({primary}, {base_url})")
}

pub fn hermes_key_variable(base_url: &str) -> String {
    todo!("harness port: HermesKeyVariable ({base_url})")
}

pub fn build_deep_seek_settings(
    base_url: &str,
    key_variable: &str,
    models: &[CatalogModel],
) -> String {
    let _ = models;
    todo!("harness port: BuildDeepSeekSettings ({base_url}, {key_variable})")
}

pub fn deep_seek_wants_headless(args: &[String]) -> bool {
    todo!("harness port: DeepSeekWantsHeadless ({args:?})")
}

pub fn build_deep_seek_patch(settings_path: &str, model: &str) -> String {
    todo!("harness port: BuildDeepSeekPatch ({settings_path}, {model})")
}

pub fn hermes_context_hint(context_window: i64) -> String {
    todo!("harness port: HermesContextHint ({context_window})")
}

pub fn hermes_argv(model: &str, child_args: &[String]) -> Vec<String> {
    todo!("harness port: HermesArgv ({model}, {child_args:?})")
}

pub fn launch_agent(agent: &Agent, model: &str, args: &[String]) -> i32 {
    todo!(
        "harness port: LaunchAgent ({}, {model}, {args:?})",
        agent.id
    )
}
