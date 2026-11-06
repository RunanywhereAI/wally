//! `--hybrid` for Hermes: the parent on the
//! cloud under a policy, every subagent on the local model, through the PLS
//! router. dsh's launcher lives beside its other config in `agents`.
//!
//! Every launch is temporary. Each harness gets its config through a channel
//! that lasts one run (an environment variable, a temp file or a temp
//! directory), and nothing in the person's own harness files changes.

use std::path::PathBuf;

use crate::bootstrap::GlobalOptions;
use crate::io::output as out;

use super::agents::{
    effective_args, hermes_argv, start_hybrid, temp_directory_path, yaml_single_quoted, Agent,
    ScopedEnv, HYBRID_LOCAL_MODEL_ID, HYBRID_MODEL_ID,
};
use super::harness::launch;
use super::pls::HybridHarness;

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

#[cfg(test)]
mod tests {
    use super::*;

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
