//! `--hybrid` for opencode, OpenWork, Hermes and OpenClaw: the parent on the
//! cloud under a policy, every subagent on the local model, through the PLS
//! router. dsh's launcher lives beside its other config in `agents`.
//!
//! Every launch is temporary. Each harness gets its config through a channel
//! that lasts one run (an environment variable, a temp file or a temp
//! directory), and nothing in the person's own harness files changes.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::bootstrap::GlobalOptions;
use crate::io::json::dump;
use crate::io::output as out;

use super::agents::{
    build_open_claw_config, drop_stale_open_claw_provider, effective_args, hermes_argv,
    open_claw_state_directory, read_open_claw_config, start_hybrid, temp_directory_path,
    yaml_single_quoted, Agent, HybridStart, ScopedEnv, TemporaryConfig, HYBRID_LOCAL_CONTEXT,
    HYBRID_LOCAL_MODEL_ID, HYBRID_LOCAL_OUTPUT, HYBRID_MODEL_ID, PROVIDER_ID,
};
use super::catalog_models::CatalogModel;
use super::harness::launch;
use super::pls::HybridHarness;

/// The header every router request carries: the per-launch secret.
const TOKEN_HEADER: &str = "x-pls-token";

/// opencode's provider id for the router.
const OPEN_CODE_PROVIDER: &str = "pls";

/// The variable Hermes expands `${PLS_TOKEN}` from, so the token is never
/// written to disk.
const HERMES_TOKEN_VARIABLE: &str = "PLS_TOKEN";

/// opencode's (and OpenWork's) inline config for a hybrid launch: one
/// provider at the router with both model ids, the parent on the cloud id,
/// the built-in subagents (`general`, `explore`) on the local one, and the
/// title call kept on the parent's id on purpose rather than by fallback.
pub fn build_open_code_hybrid_config(base_url: &str, token: &str, parent: (i64, i64)) -> String {
    let entry = |id: &str, (context, output): (i64, i64)| json!({ "name": id, "limit": { "context": context, "output": output } });
    let parent_id = format!("{OPEN_CODE_PROVIDER}/{HYBRID_MODEL_ID}");
    let local_id = format!("{OPEN_CODE_PROVIDER}/{HYBRID_LOCAL_MODEL_ID}");
    dump(&json!({
        "provider": {
            OPEN_CODE_PROVIDER: {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Wally hybrid",
                "options": {
                    "baseURL": format!("{base_url}/v1"),
                    "apiKey": "local",
                    "headers": { TOKEN_HEADER: token },
                },
                "models": {
                    HYBRID_MODEL_ID: entry(HYBRID_MODEL_ID, parent),
                    HYBRID_LOCAL_MODEL_ID:
                        entry(HYBRID_LOCAL_MODEL_ID, (HYBRID_LOCAL_CONTEXT, HYBRID_LOCAL_OUTPUT)),
                },
            }
        },
        "model": parent_id,
        "small_model": parent_id,
        "agent": {
            "general": { "model": local_id },
            "explore": { "model": local_id },
            "title": { "model": parent_id },
        },
    }))
}

/// `wally opencode --hybrid`.
pub fn launch_open_code_hybrid(
    model: &str,
    cloud_model: &str,
    policy: &str,
    args: &[String],
    options: &GlobalOptions,
) -> i32 {
    // The installed version picks the spawn tool PLS watches for: `task` on
    // 1.x, `subagent` on 2.x.
    let major = super::opencode::installed_open_code_major_version();
    let hybrid = match start_hybrid(
        model,
        cloud_model,
        policy,
        HybridHarness::for_open_code(major),
        options,
    ) {
        Ok(hybrid) => hybrid,
        Err(status) => return status,
    };
    let config = build_open_code_hybrid_config(
        &hybrid.router.base_url,
        &hybrid.router.token,
        hybrid.parent_limits(),
    );
    let content = ScopedEnv::new("OPENCODE_CONFIG_CONTENT", &config);
    if !content.applied() {
        out::error_line("could not set the temporary OpenCode configuration");
        return 1;
    }
    hybrid.announce();
    // 2.x serves sessions from a background service that never sees this
    // environment; `--standalone` gives the launch its own.
    let args = super::opencode::with_private_server(args, major);
    let status = launch("opencode", "", &args, options);
    drop(content);
    drop(hybrid);
    status
}

/// OpenWork's hybrid launch: the started router and the opencode config for
/// its engine. OpenWork pins that engine to 1.x (the only one that reads
/// `OPENCODE_CONFIG_CONTENT`), so the router serves the 1.x profile.
pub(crate) fn start_openwork_hybrid(
    model: &str,
    cloud_model: &str,
    policy: &str,
    options: &GlobalOptions,
) -> Result<(HybridStart, String), i32> {
    let hybrid = start_hybrid(
        model,
        cloud_model,
        policy,
        HybridHarness::OpenCodeV1,
        options,
    )?;
    let config = build_open_code_hybrid_config(
        &hybrid.router.base_url,
        &hybrid.router.token,
        hybrid.parent_limits(),
    );
    Ok((hybrid, config))
}

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

/// OpenClaw's config for a hybrid launch, merged onto theirs: one provider at
/// the router with both ids, the parent on the cloud id, subagents on the
/// local one and unable to spawn, and utility calls (titles, recaps) on the
/// parent's id.
pub fn build_open_claw_hybrid_config(
    existing: &str,
    base_url: &str,
    token: &str,
    parent: (i64, i64),
) -> String {
    let model = |id: &str, (context, output): (i64, i64)| CatalogModel {
        id: id.to_string(),
        context_window: context,
        max_output: output,
        ..CatalogModel::default()
    };
    let built = build_open_claw_config(
        existing,
        HYBRID_MODEL_ID,
        &format!("{base_url}/v1"),
        "local",
        &[
            model(HYBRID_MODEL_ID, parent),
            model(
                HYBRID_LOCAL_MODEL_ID,
                (HYBRID_LOCAL_CONTEXT, HYBRID_LOCAL_OUTPUT),
            ),
        ],
    );
    let mut config: Value = serde_json::from_str(&built).unwrap_or_else(|_| json!({}));
    config["models"]["providers"][PROVIDER_ID]["headers"][TOKEN_HEADER] = json!(token);
    let defaults = &mut config["agents"]["defaults"];
    defaults["subagents"]["model"] = json!(format!("{PROVIDER_ID}/{HYBRID_LOCAL_MODEL_ID}"));
    defaults["subagents"]["maxSpawnDepth"] = json!(1);
    defaults["utilityModel"] = json!(format!("{PROVIDER_ID}/{HYBRID_MODEL_ID}"));
    dump(&config)
}

/// `wally openclaw --hybrid`.
pub fn launch_open_claw_hybrid(
    agent: &Agent,
    model: &str,
    cloud_model: &str,
    policy: &str,
    args: &[String],
    options: &GlobalOptions,
) -> i32 {
    let hybrid = match start_hybrid(model, cloud_model, policy, HybridHarness::OpenClaw, options) {
        Ok(hybrid) => hybrid,
        Err(status) => return status,
    };
    let existing = read_open_claw_config();
    let built = build_open_claw_hybrid_config(
        &existing,
        &hybrid.router.base_url,
        &hybrid.router.token,
        hybrid.parent_limits(),
    );
    let mut config = TemporaryConfig::new();
    if let Err(failure) = config.write(&built, ".json") {
        out::error_line(&failure);
        return 1;
    }
    let state = open_claw_state_directory();
    if state.as_os_str().is_empty() {
        out::error_line("could not work out where openclaw keeps its state");
        return 1;
    }
    // The router's port changes every launch; a provider left in an agent's
    // `models.json` would otherwise win over this config.
    drop_stale_open_claw_provider(&state, &existing);
    // The state directory first: OpenClaw otherwise derives it from the
    // config file's folder.
    let state_dir = ScopedEnv::new("OPENCLAW_STATE_DIR", &state.to_string_lossy());
    let path = ScopedEnv::new("OPENCLAW_CONFIG_PATH", &config.path());
    if !state_dir.applied() || !path.applied() {
        out::error_line("could not set the router for openclaw");
        return 1;
    }
    hybrid.announce();
    let status = launch(agent.command, "", &effective_args(agent, args), options);
    drop(path);
    drop(state_dir);
    drop(config);
    drop(hybrid);
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn opencode_hybrid_names_both_ids_on_one_provider() {
        let config = parsed(&build_open_code_hybrid_config(
            "http://127.0.0.1:9",
            "tok",
            (200_000, 32_000),
        ));
        let models = &config["provider"]["pls"]["models"];
        assert_eq!(models[HYBRID_MODEL_ID]["limit"]["context"], 200_000);
        assert_eq!(
            models[HYBRID_LOCAL_MODEL_ID]["limit"]["output"],
            HYBRID_LOCAL_OUTPUT
        );
        assert_eq!(
            config["provider"]["pls"]["options"]["baseURL"],
            "http://127.0.0.1:9/v1"
        );
        assert_eq!(config["model"], "pls/Hybrid-Wally-0.1");
    }

    #[test]
    fn opencode_hybrid_puts_subagents_local_and_titles_on_the_parent() {
        let config = parsed(&build_open_code_hybrid_config("http://h", "tok", (1, 1)));
        for agent in ["general", "explore"] {
            assert_eq!(
                config["agent"][agent]["model"], "pls/Hybrid-Wally-0.1-local",
                "{agent}"
            );
        }
        assert_eq!(config["agent"]["title"]["model"], "pls/Hybrid-Wally-0.1");
        assert_eq!(config["small_model"], "pls/Hybrid-Wally-0.1");
    }

    #[test]
    fn opencode_hybrid_sends_the_launch_token() {
        let config = parsed(&build_open_code_hybrid_config("http://h", "s3cret", (1, 1)));
        assert_eq!(
            config["provider"]["pls"]["options"]["headers"]["x-pls-token"],
            "s3cret"
        );
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
    fn openclaw_hybrid_routes_parent_children_and_utility_calls() {
        let config = parsed(&build_open_claw_hybrid_config(
            "",
            "http://127.0.0.1:9",
            "tok",
            (200_000, 32_000),
        ));
        let defaults = &config["agents"]["defaults"];
        assert_eq!(defaults["model"]["primary"], "runanywhere/Hybrid-Wally-0.1");
        assert_eq!(
            defaults["subagents"]["model"],
            "runanywhere/Hybrid-Wally-0.1-local"
        );
        assert_eq!(defaults["subagents"]["maxSpawnDepth"], 1);
        assert_eq!(defaults["utilityModel"], "runanywhere/Hybrid-Wally-0.1");
        let ids: Vec<&str> = config["models"]["providers"]["runanywhere"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, [HYBRID_MODEL_ID, HYBRID_LOCAL_MODEL_ID]);
    }

    #[test]
    fn openclaw_hybrid_sends_the_launch_token() {
        let config = parsed(&build_open_claw_hybrid_config(
            "",
            "http://h",
            "s3cret",
            (1, 1),
        ));
        assert_eq!(
            config["models"]["providers"]["runanywhere"]["headers"]["x-pls-token"],
            "s3cret"
        );
    }

    #[test]
    fn openclaw_hybrid_keeps_the_persons_other_settings() {
        let existing = r#"{"gateway":{"port":1},"agents":{"defaults":{"thinking":"low"}}}"#;
        let config = parsed(&build_open_claw_hybrid_config(
            existing,
            "http://h",
            "t",
            (1, 1),
        ));
        assert_eq!(config["gateway"]["port"], 1);
        assert_eq!(config["agents"]["defaults"]["thinking"], "low");
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
