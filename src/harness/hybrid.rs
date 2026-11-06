//! Hybrid routing launches. DeepSeek and Hermes are the only ones: the parent
//! on the cloud under a policy, every subagent on the local model, through
//! the PLS router.
//!
//! Every launch is temporary. Each harness gets its config through a channel
//! that lasts one run (an environment variable, a temp file or a temp
//! directory), and nothing in the person's own harness files changes.

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::bootstrap::GlobalOptions;
use crate::catalog;
use crate::io::json::dump;
use crate::io::output as out;

use super::agents::{
    clean_leftover_configs, console_limits, deep_seek_wants_headless, deep_seek_web_args,
    effective_args, hermes_argv, temp_directory_path, yaml_single_quoted, Agent, ModelLimits,
    ScopedEnv, TemporaryConfig, KEY_VARIABLE, PROVIDER_ID,
};
use super::harness::launch;
use super::local_models::local_models;
use super::pls::HybridHarness;

/// The commands that have a hybrid launch. `cmd_harness` offers `--hybrid`
/// on these and on no others.
pub const HYBRID_COMMANDS: &[&str] = &["deepseek", "hermes"];

/// `wally <harness> --hybrid` for one of [`HYBRID_COMMANDS`].
pub fn launch_hybrid(
    agent: &Agent,
    model: &str,
    cloud_model: &str,
    policy: &str,
    args: &[String],
    options: &GlobalOptions,
) -> i32 {
    match agent.id {
        "hermes" => launch_hermes_hybrid(agent, model, cloud_model, policy, args, options),
        "deepseek" => launch_deep_seek_hybrid(model, cloud_model, policy, args, options),
        _ => {
            out::error_line(&format!("{} has no hybrid launch", agent.id));
            2
        }
    }
}

/// The header every router request carries: the per-launch secret.
const TOKEN_HEADER: &str = "x-pls-token";

/// The variable Hermes expands `${PLS_TOKEN}` from, so the token is never
/// written to disk.
const HERMES_TOKEN_VARIABLE: &str = "PLS_TOKEN";

/// Hermes' managed overlay for a hybrid launch (`HERMES_MANAGED_DIR`). Its
/// keys are merged over the person's `config.yaml` per leaf, so the route
/// wins over their `model.provider` without the file changing.
///
/// The token is `${PLS_TOKEN}`, expanded by Hermes from its environment.
/// `vision` goes here instead of the person's file; `None` leaves it unset.
pub fn build_hermes_hybrid_overlay(base_url: &str, vision: Option<bool>) -> String {
    let mut overlay = format!(
        "model:\n  provider: custom\n  default: {}\n  base_url: {}\n  default_headers:\n    {TOKEN_HEADER}: ${{{HERMES_TOKEN_VARIABLE}}}\n",
        yaml_single_quoted(HYBRID_MODEL_ID),
        yaml_single_quoted(&format!("{base_url}/v1")),
    );
    if let Some(vision) = vision {
        overlay.push_str(&format!("  supports_vision: {vision}\n"));
    }
    // A child that names only a model keeps the parent's endpoint and
    // headers. Depth 1 makes children leaves. Two at a time, because the
    // local lane runs one request and queues the rest.
    overlay.push_str(&format!(
        "delegation:\n  model: {}\n  max_spawn_depth: 1\n  max_concurrent_children: 2\n",
        yaml_single_quoted(HYBRID_LOCAL_MODEL_ID),
    ));
    // The post-turn review replays the whole conversation with tools on the
    // parent's model, which would read as a main-chat turn.
    overlay.push_str("auxiliary:\n  background_review:\n    enabled: false\n");
    overlay
}

/// A private temp directory, removed with everything in it on drop.
struct TemporaryDirectory {
    path: PathBuf,
}

impl TemporaryDirectory {
    fn new(prefix: &str) -> Result<Self, String> {
        let Some(parent) = temp_directory_path() else {
            return Err("no temp directory to write the agent config into".to_string());
        };
        let random = getrandom::u64().map_err(|error| format!("no randomness ({error})"))?;
        let path = parent.join(format!("{prefix}-{random:016x}"));
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder
        };
        // The per-user temp directory is already private to its owner.
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder
            .create(&path)
            .map_err(|error| format!("could not create {} ({error})", path.display()))?;
        Ok(TemporaryDirectory { path })
    }

    fn write(&self, name: &str, contents: &str) -> Result<(), String> {
        let file = self.path.join(name);
        std::fs::write(&file, contents)
            .map_err(|error| format!("could not write {} ({error})", file.display()))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `wally hermes --hybrid`.
pub fn launch_hermes_hybrid(
    agent: &Agent,
    model: &str,
    cloud_model: &str,
    policy: &str,
    args: &[String],
    options: &GlobalOptions,
) -> i32 {
    if !super::agents::stop_tui_gateway(agent.command) {
        return 1;
    }
    let hybrid = match start_hybrid(model, cloud_model, policy, HybridHarness::Hermes, options) {
        Ok(hybrid) => hybrid,
        Err(status) => return status,
    };
    let vision = if hybrid.cloud.image_input {
        Some(true)
    } else if hybrid.cloud.text_only {
        Some(false)
    } else {
        None
    };
    let managed = match TemporaryDirectory::new("wally-hermes") {
        Ok(managed) => managed,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };
    if let Err(failure) = managed.write(
        "config.yaml",
        &build_hermes_hybrid_overlay(&hybrid.router.base_url, vision),
    ) {
        out::error_line(&failure);
        return 1;
    }
    let base_url = format!("{}/v1", hybrid.router.base_url);
    let environment = [
        ScopedEnv::new("HERMES_MANAGED_DIR", &managed.path.to_string_lossy()),
        ScopedEnv::new(HERMES_TOKEN_VARIABLE, &hybrid.router.token),
        ScopedEnv::new("CUSTOM_BASE_URL", &base_url),
        ScopedEnv::new("HERMES_INFERENCE_PROVIDER", "custom"),
        ScopedEnv::new("HERMES_INFERENCE_MODEL", HYBRID_MODEL_ID),
        ScopedEnv::new("HERMES_MODEL", HYBRID_MODEL_ID),
    ];
    if environment.iter().any(|variable| !variable.applied()) {
        out::error_line("could not set the router for hermes");
        return 1;
    }
    hybrid.announce();
    let argv = hermes_argv(HYBRID_MODEL_ID, &effective_args(agent, args));
    let status = launch(agent.command, "", &argv, options);
    // The gateway this launch started holds a router that is about to stop
    // and an overlay about to be deleted.
    super::agents::stop_tui_gateway(agent.command);
    drop(environment);
    drop(managed);
    drop(hybrid);
    status
}

/// The model id the parent uses. The router has one endpoint for the parent
/// and every subagent and tells them apart from the request itself (a
/// subagent prompt starts with a `pls-task:` line), not from the id.
pub const HYBRID_MODEL_ID: &str = "Hybrid-Wally-0.1";

/// The model id subagents use. Same endpoint; it exists only so dsh compacts
/// a subagent at the local model's window instead of the cloud's.
pub const HYBRID_LOCAL_MODEL_ID: &str = "Hybrid-Wally-0.1-local";

/// The local lane's limits: the model server's KV cap and reply cap.
pub(super) const HYBRID_LOCAL_CONTEXT: i64 = 65536;
pub(super) const HYBRID_LOCAL_OUTPUT: i64 = 8192;

/// The one-provider `llm-pi-ai` config for a hybrid launch. `cloud` is what
/// the console says the parent's cloud model can hold: the parent's model
/// entry declares those limits and the subagent's the local ones, because a
/// shared local cap cut cloud replies off at 8192 tokens once the parent
/// started writing whole files itself under the cost policy.
fn build_hybrid_llm_config(
    base_url: &str,
    key_variable: &str,
    token: &str,
    cloud: &ModelLimits,
) -> String {
    let model = |id: &str, context: i64, output: i64| {
        json!({
            "id": id,
            "name": id,
            "input": ["text"],
            "contextWindow": context,
            "maxTokens": output,
            "reasoningEfforts": false,
        })
    };
    let subagent = model(
        HYBRID_LOCAL_MODEL_ID,
        HYBRID_LOCAL_CONTEXT,
        HYBRID_LOCAL_OUTPUT,
    );
    let (context, output) = parent_limits(cloud);
    let parent = model(HYBRID_MODEL_ID, context, output);
    dump(&json!({
        "providers": {
            PROVIDER_ID: {
                "displayName": "Wally",
                "api": "openai-completions",
                "baseURL": format!("{base_url}/v1"),
                "apiKeyEnv": key_variable,
                // The router port is loopback but shared, so every request
                // carries the launch secret.
                "headers": { "x-pls-token": token },
                "models": [parent, subagent],
            }
        }
    }))
}

/// The patch for a hybrid launch: one provider, the parent on its model id
/// and every subagent on the local one.
fn build_hybrid_patch(llm_config: &str) -> String {
    let mut patch = format!(
        "- id: llm-pi-ai\n  config: {llm_config}\n\
         - id: agent-default-model\n  config:\n    provider: {PROVIDER_ID}\n    model: {}\n\
         - id: tool-subagent\n  disabled: false\n  config:\n    provider: spawn\n    agentOptions:\n      provider: {PROVIDER_ID}\n      model: {}\n",
        yaml_single_quoted(HYBRID_MODEL_ID),
        yaml_single_quoted(HYBRID_LOCAL_MODEL_ID),
    );
    // The web profile mounts its own copy of the tool from the `standard`
    // agent preset, so the row above is inert there. The preset is a root
    // entry, and a patch replaces its whole config, so its plugin list is
    // copied from the installed preset with the subagent model injected.
    match hybrid_preset_patch() {
        Some(preset) => patch.push_str(&preset),
        None => out::error_line(
            "hybrid: could not read the installed dsh web preset; \
             subagents in the web profile will not use the local model",
        ),
    }
    patch
}

/// `preset-standard` with `agentOptions` added to its `tool-subagent` row, or
/// `None` when the installed preset cannot be read or no longer has that row.
fn hybrid_preset_patch() -> Option<String> {
    let dsh = find_on_path("dsh")?;
    let root = dsh.canonicalize().ok()?.parent()?.parent()?.to_path_buf();
    let file = root.join("node_modules/@deepseek-ai/dsh-web-app/presets/standard.patch.yml");
    let text = std::fs::read_to_string(file).ok()?;

    let mut block: Vec<&str> = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.trim_start().starts_with("- id: preset-standard") {
            inside = true;
            continue;
        }
        if inside {
            let indent = line.len() - line.trim_start().len();
            if !line.trim().is_empty() && indent <= 4 {
                break;
            }
            block.push(line);
        }
    }
    if !block
        .iter()
        .any(|line| line.trim() == "- id: tool-subagent")
    {
        return None;
    }

    let mut out = String::from("- id: preset-standard\n");
    let mut injected = false;
    for line in block {
        let stripped = line.strip_prefix("    ").unwrap_or(line);
        let trimmed = stripped.trim_start();
        if trimmed.starts_with("name:") && !injected && out.ends_with("- id: preset-standard\n") {
            continue;
        }
        out.push_str(stripped);
        out.push('\n');
        if !injected && trimmed == "config:" && out.contains("- id: tool-subagent\n") {
            let indent = stripped.len() - trimmed.len();
            let pad = " ".repeat(indent + 2);
            out.push_str(&format!(
                "{pad}agentOptions:\n{pad}  provider: {PROVIDER_ID}\n{pad}  model: {}\n",
                yaml_single_quoted(HYBRID_LOCAL_MODEL_ID),
            ));
            injected = true;
        }
    }
    injected.then_some(out)
}

fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// The first downloaded MLX model certified for a harness, as a catalog id.
fn default_hybrid_model(options: &GlobalOptions) -> Option<String> {
    let env = crate::bootstrap::bootstrap(options).ok()?;
    local_models(&env.home)
        .into_iter()
        .filter(|candidate| candidate.framework == "MLX")
        .find_map(|candidate| {
            let entry = catalog::all()
                .iter()
                .find(|entry| entry.harness_compatible && candidate.id.contains(entry.id))?;
            Some(entry.id.to_string())
        })
}

/// The directory of the local model `model` names, if it is an MLX model on
/// this machine.
fn hybrid_model_directory(model: &str, options: &GlobalOptions) -> Option<String> {
    if Path::new(model).is_dir() {
        return Some(model.to_string());
    }
    let env = crate::bootstrap::bootstrap(options).ok()?;
    let entry = catalog::find(model)?;
    let installed = local_models(&env.home);
    let found = installed.iter().find(|candidate| {
        candidate.framework == "MLX"
            && (candidate.id == model
                || candidate.id == entry.id
                || candidate.id.contains(entry.id)
                || catalog::merge_key_for(&candidate.id) == model)
    })?;
    if found.dir.is_empty() {
        out::error_line(&format!("{model} is on this machine but has no directory"));
        return None;
    }
    Some(found.dir.clone())
}

/// A started hybrid launch: the router, and what the harness config needs.
pub(crate) struct HybridStart {
    pub router: super::pls::Router,
    /// The local model, as the person named it or as it was picked.
    pub model: String,
    pub policy: super::pls::Policy,
    /// What the console says the parent's cloud model can hold.
    pub cloud: ModelLimits,
}

impl HybridStart {
    /// The line every hybrid launch prints before handing over the terminal.
    pub fn announce(&self) {
        out::status_line(&format!(
            "hybrid ({} policy): parent on the cloud, subagents on {} through {}",
            self.policy.as_str(),
            self.model,
            self.router.base_url
        ));
    }
}

fn parent_limits(cloud: &ModelLimits) -> (i64, i64) {
    (
        if cloud.context_window > 0 {
            cloud.context_window
        } else {
            HYBRID_LOCAL_CONTEXT
        },
        if cloud.max_output > 0 {
            cloud.max_output
        } else {
            HYBRID_LOCAL_OUTPUT
        },
    )
}

/// Everything a hybrid launch does before its harness config: check the
/// platform and policy, pick the local model, read the cloud model's limits,
/// write the router config for `harness`, and start the router. `Err` is the
/// exit code, after the reason was printed.
pub(crate) fn start_hybrid(
    model: &str,
    cloud_model: &str,
    policy: &str,
    harness: super::pls::HybridHarness,
    options: &GlobalOptions,
) -> Result<HybridStart, i32> {
    if let Err(error) = super::pls::supported() {
        out::error_line(&error);
        return Err(2);
    }
    let policy = match super::pls::parse_policy(policy) {
        Ok(policy) => policy,
        Err(error) => {
            out::error_line(&error);
            return Err(2);
        }
    };
    clean_leftover_configs();
    // No -m: use the first downloaded MLX model certified for a harness, so
    // `--hybrid` alone is enough.
    let model = if model.is_empty() {
        match default_hybrid_model(options) {
            Some(id) => id,
            None => {
                out::error_line("--hybrid needs -m <local model> and none is downloaded");
                return Err(2);
            }
        }
    } else {
        model.to_string()
    };
    let Some(directory) = hybrid_model_directory(&model, options) else {
        out::error_line(&format!("{model} is not a downloaded MLX model"));
        out::status_line("pull one first: wally models pull <model>");
        return Err(1);
    };
    if cloud_model.is_empty() {
        out::error_line("--hybrid needs a cloud model: pass --cloud-model <id> or set a default");
        return Err(2);
    }
    let cloud = console_limits(cloud_model);
    if cloud.max_output <= 0 {
        out::status_line(&format!(
            "hybrid: the console gave no output limit for {cloud_model}; \
             the parent keeps {HYBRID_LOCAL_OUTPUT} tokens per reply"
        ));
    }
    super::pls::configure(
        &directory,
        cloud_model,
        policy,
        harness,
        (cloud.context_window, cloud.max_output),
    );
    let decider = super::pls::resolve_decider(options);
    let Some(router) = super::pls::start_logged(&directory, decider) else {
        return Err(1);
    };
    Ok(HybridStart {
        router,
        model,
        policy,
        cloud,
    })
}

/// `wally deepseek --hybrid`: route the parent to the cloud and every
/// subagent to the local model through the PLS router.
pub fn launch_deep_seek_hybrid(
    model: &str,
    cloud_model: &str,
    policy: &str,
    args: &[String],
    options: &GlobalOptions,
) -> i32 {
    let hybrid = match start_hybrid(
        model,
        cloud_model,
        policy,
        super::pls::HybridHarness::DeepSeek,
        options,
    ) {
        Ok(hybrid) => hybrid,
        Err(status) => return status,
    };
    let router = &hybrid.router;
    let cloud_limits = &hybrid.cloud;
    let llm_config =
        build_hybrid_llm_config(&router.base_url, KEY_VARIABLE, &router.token, cloud_limits);
    let mut config = TemporaryConfig::new();
    if let Err(failure) = config.write(&build_hybrid_patch(&llm_config), ".yaml") {
        out::error_line(&failure);
        return 1;
    }
    let key = ScopedEnv::new(KEY_VARIABLE, "local");
    if !key.applied() {
        out::error_line("could not set the router key");
        return 1;
    }

    let names_profile = args.iter().any(|arg| arg == "--profile");
    let mut launch_args: Vec<String> = if names_profile {
        vec!["--patch".to_string(), config.path()]
    } else if deep_seek_wants_headless(args) {
        vec![
            "--profile".to_string(),
            "headless".to_string(),
            "--patch".to_string(),
            config.path(),
        ]
    } else {
        out::status_line("opening the dsh web ui; pass a prompt to run headless instead");
        deep_seek_web_args(&config.path(), args)
    };
    launch_args.extend(args.iter().cloned());
    hybrid.announce();
    let status = launch("dsh", "", &launch_args, options);
    drop(hybrid);
    status
}

#[cfg(test)]
mod tests {
    use super::super::agents::{ModelLimits, PROVIDER_ID};
    use super::*;
    use serde_json::{json, Value};

    fn hybrid_config_with(cloud: &ModelLimits) -> Value {
        let config =
            build_hybrid_llm_config("http://127.0.0.1:1234", "WALLY_PLS_KEY", "tok", cloud);
        serde_json::from_str(&config).unwrap()
    }

    fn hybrid_model<'a>(parsed: &'a Value, id: &str) -> &'a Value {
        parsed["providers"][PROVIDER_ID]["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == json!(id))
            .unwrap_or_else(|| panic!("no model entry for {id}"))
    }

    #[test]
    fn the_hybrid_config_has_one_provider_on_v1() {
        let parsed = hybrid_config_with(&ModelLimits::default());
        let providers = parsed["providers"].as_object().unwrap();
        assert_eq!(
            providers.keys().collect::<Vec<_>>(),
            vec![&PROVIDER_ID.to_string()]
        );
        assert_eq!(
            parsed["providers"][PROVIDER_ID]["baseURL"],
            json!("http://127.0.0.1:1234/v1")
        );
    }

    #[test]
    fn the_hybrid_provider_lists_the_parent_and_subagent_model_ids() {
        let parsed = hybrid_config_with(&ModelLimits::default());
        let ids: Vec<&Value> = parsed["providers"][PROVIDER_ID]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| &model["id"])
            .collect();
        assert_eq!(
            ids,
            vec![&json!(HYBRID_MODEL_ID), &json!(HYBRID_LOCAL_MODEL_ID)]
        );
    }

    #[test]
    fn the_hybrid_model_ids_are_the_published_names() {
        assert_eq!(HYBRID_MODEL_ID, "Hybrid-Wally-0.1");
        assert_eq!(HYBRID_LOCAL_MODEL_ID, "Hybrid-Wally-0.1-local");
    }

    #[test]
    fn the_hybrid_patch_has_no_local_provider_or_local_path() {
        let config = build_hybrid_llm_config(
            "http://127.0.0.1:1234",
            "WALLY_PLS_KEY",
            "tok",
            &ModelLimits::default(),
        );
        let patch = build_hybrid_patch(&config);
        assert!(!patch.contains("wally-local"));
        assert!(!patch.contains("/local/v1"));
    }

    #[test]
    fn the_hybrid_patch_keeps_the_subagent_row_live_and_spawned() {
        let config = build_hybrid_llm_config(
            "http://127.0.0.1:1234",
            "WALLY_PLS_KEY",
            "tok",
            &ModelLimits::default(),
        );
        let patch = build_hybrid_patch(&config);
        // The row's own provider must survive the overlay, or dsh rejects the
        // whole config with "$.provider missing required value".
        assert!(patch.contains("provider: spawn"));
        // The web profile disables this row; without the override the parent
        // has no subagent tool and nothing ever reaches the local lane.
        assert!(patch.contains("disabled: false"));
    }

    #[test]
    fn the_hybrid_parent_row_names_the_parent_model_id() {
        let patch = build_hybrid_patch(&build_hybrid_llm_config(
            "http://127.0.0.1:1234",
            "K",
            "tok",
            &ModelLimits::default(),
        ));
        let row = patch
            .split("- id: ")
            .find(|row| row.starts_with("agent-default-model"))
            .unwrap();
        assert!(row.contains(&format!("provider: {PROVIDER_ID}")));
        assert!(row.contains(&format!("model: '{HYBRID_MODEL_ID}'")));
    }

    #[test]
    fn the_hybrid_subagent_row_names_the_local_model_id() {
        let patch = build_hybrid_patch(&build_hybrid_llm_config(
            "http://127.0.0.1:1234",
            "K",
            "tok",
            &ModelLimits::default(),
        ));
        let row = patch
            .split("- id: ")
            .find(|row| row.starts_with("tool-subagent"))
            .unwrap();
        assert!(row.contains(&format!("provider: {PROVIDER_ID}")));
        assert!(row.contains(&format!("model: '{HYBRID_LOCAL_MODEL_ID}'")));
    }

    #[test]
    fn the_hybrid_subagent_model_carries_the_local_limits() {
        let cloud = ModelLimits {
            context_window: 1_000_000,
            max_output: 65_536,
            ..ModelLimits::default()
        };
        let parsed = hybrid_config_with(&cloud);
        let local = hybrid_model(&parsed, HYBRID_LOCAL_MODEL_ID);
        assert_eq!(local["contextWindow"], json!(65_536));
        assert_eq!(local["maxTokens"], json!(8_192));
    }

    #[test]
    fn the_hybrid_parent_model_carries_the_cloud_limits() {
        let cloud = ModelLimits {
            context_window: 1_000_000,
            max_output: 65_536,
            ..ModelLimits::default()
        };
        let parsed = hybrid_config_with(&cloud);
        let parent = hybrid_model(&parsed, HYBRID_MODEL_ID);
        assert_eq!(parent["contextWindow"], json!(1_000_000));
        assert_eq!(parent["maxTokens"], json!(65_536));
    }

    #[test]
    fn hermes_overlay_routes_the_parent_and_pins_children_to_the_local_id() {
        let overlay = build_hermes_hybrid_overlay("http://127.0.0.1:9", None);
        assert!(overlay.contains("  provider: custom\n"));
        assert!(overlay.contains("  default: 'Hybrid-Wally-0.1'\n"));
        assert!(overlay.contains("  base_url: 'http://127.0.0.1:9/v1'\n"));
        assert!(overlay.contains("  model: 'Hybrid-Wally-0.1-local'\n"));
        assert!(overlay.contains("  max_spawn_depth: 1\n"));
        assert!(overlay.contains("  max_concurrent_children: 2\n"));
    }

    #[test]
    fn hermes_overlay_carries_the_token_as_a_reference_not_a_value() {
        let overlay = build_hermes_hybrid_overlay("http://h", None);
        assert!(overlay.contains("    x-pls-token: ${PLS_TOKEN}\n"));
    }

    #[test]
    fn hermes_overlay_sets_vision_only_when_known() {
        assert!(!build_hermes_hybrid_overlay("http://h", None).contains("supports_vision"));
        assert!(build_hermes_hybrid_overlay("http://h", Some(true))
            .contains("  supports_vision: true\n"));
    }

    #[test]
    fn hermes_overlay_turns_off_the_background_review() {
        let overlay = build_hermes_hybrid_overlay("http://h", None);
        assert!(overlay.contains("auxiliary:\n  background_review:\n    enabled: false\n"));
    }

    #[test]
    fn a_temporary_directory_is_removed_with_its_files() {
        let directory = TemporaryDirectory::new("wally-test").unwrap();
        directory.write("config.yaml", "x: 1\n").unwrap();
        let path = directory.path.clone();
        assert!(path.join("config.yaml").is_file());
        drop(directory);
        assert!(!path.exists());
    }
}
