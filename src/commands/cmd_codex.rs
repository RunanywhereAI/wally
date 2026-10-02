//! `wally codex -m <model>`: Codex CLI on a local or hosted model.
//!
//! Codex speaks only the OpenAI Responses API, so wally starts the loopback
//! Responses bridge (`crate::responses`) and points Codex at it. Everything
//! Codex needs is passed as `-c` overrides on its command line and one
//! environment variable for the key, so `~/.codex` is never written: the
//! person's own config, login and history stay exactly as they were.
//!
//! Codex's `/model` picker lists only what its model catalog names, so each
//! launch writes one into wally's state directory with every
//! model on the account, and removes it when Codex exits. The bridge already
//! serves whichever catalog model a request names.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::cmd_default_models::resolve_default_model;
use crate::commands::cmd_editors::{cloud_context_window, ScopedEnv};
use crate::harness::{self, CatalogModel, DeclaredHarness};
use crate::io::output as out;
use crate::responses::{self, ModelAliases};

const TOOL: &str = "codex";
// The provider id Codex files our endpoint under, and the variable it reads
// the key from. Neither is ever written to disk.
const PROVIDER_ID: &str = "runanywhere";
const KEY_VARIABLE: &str = "WALLY_CODEX_API_KEY";
// What Codex is told when the catalog carries no window.
const FALLBACK_CONTEXT_WINDOW: i64 = 128_000;

/// Codex's model catalog for `models`, in the shape Codex reads from
/// `model_catalog_json`. Hosted models truncate tool output by tokens, local
/// ones by bytes.
pub fn codex_catalog(models: &[CatalogModel], hosted: bool) -> Value {
    let entries: Vec<Value> = models
        .iter()
        .map(|model| {
            let window = if model.context_window > 0 {
                model.context_window
            } else {
                FALLBACK_CONTEXT_WINDOW
            };
            json!({
                "slug": model.id,
                "display_name": model.id,
                "context_window": window,
                "shell_type": "default",
                "visibility": "list",
                "supported_in_api": true,
                "priority": 0,
                "truncation_policy": {"mode": if hosted { "tokens" } else { "bytes" }, "limit": 10000},
                "input_modalities": ["text"],
                "base_instructions": "",
                "support_verbosity": true,
                "default_verbosity": "low",
                "supports_parallel_tool_calls": false,
                "supports_reasoning_summaries": false,
                "supported_reasoning_levels": [],
                "default_reasoning_level": Value::Null,
                "experimental_supported_tools": [],
            })
        })
        .collect();
    json!({ "models": entries })
}

/// Writes this launch's catalog, or None when there is nowhere to put it
/// (Codex then shows its own list, and the launched model still runs).
fn write_catalog(catalog: &Value) -> Option<PathBuf> {
    let state = crate::config::cli_paths::state_dir();
    if state.is_empty() {
        return None;
    }
    let dir = Path::new(&state).join("codex");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("catalog-{}.json", std::process::id()));
    std::fs::write(&path, serde_json::to_vec_pretty(catalog).ok()?).ok()?;
    Some(path)
}

/// The `-c` overrides that put Codex on the bridge at `base_url`, ahead of
/// the person's own arguments. Values are TOML, which is how Codex parses
/// `-c`; a JSON string literal is also a valid TOML basic string, which keeps
/// a Windows path's backslashes intact. `context_window` 0 leaves Codex's
/// own default.
pub fn codex_args(
    base_url: &str,
    model: &str,
    context_window: i64,
    catalog: Option<&Path>,
    rest: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-c".to_string(),
        format!("model_provider=\"{PROVIDER_ID}\""),
        "-c".to_string(),
        format!(
            "model_providers.{PROVIDER_ID}={{name=\"RunAnywhere\",base_url=\"{base_url}/v1\",wire_api=\"responses\",env_key=\"{KEY_VARIABLE}\"}}"
        ),
    ];
    if context_window > 0 {
        args.push("-c".to_string());
        args.push(format!("model_context_window={context_window}"));
    }
    if let Some(path) = catalog {
        let quoted = serde_json::to_string(&path.to_string_lossy()).unwrap_or_default();
        args.push("-c".to_string());
        args.push(format!("model_catalog_json={quoted}"));
    }
    args.push("-m".to_string());
    args.push(model.to_string());
    args.extend(rest.iter().cloned());
    args
}

fn run(model: &str, rest: &[String], options: &GlobalOptions) -> i32 {
    if model.is_empty() {
        // Nothing to wire: Codex runs as the person has it configured.
        return harness::spawn(TOOL, rest);
    }
    let Some(endpoint) = harness::resolve(model, options, TOOL) else {
        return 1;
    };
    let Some(mut shim) = responses::start(
        &endpoint,
        model,
        DeclaredHarness::KCodex,
        options.verbose,
        "",
        &ModelAliases::new(),
    ) else {
        harness::release(&endpoint);
        return 1;
    };
    out::status_line(&format!(
        "codex will talk to {model} through {}",
        shim.base_url
    ));

    let context_window = if endpoint.serving {
        endpoint.context_window
    } else {
        cloud_context_window(model)
    };
    let catalog = write_catalog(&codex_catalog(
        &harness::catalog_models_for(&endpoint, model),
        !endpoint.serving,
    ));
    let status = {
        let _key = ScopedEnv::new(KEY_VARIABLE, &shim.auth_token);
        harness::spawn(
            TOOL,
            &codex_args(
                &shim.base_url,
                model,
                context_window,
                catalog.as_deref(),
                rest,
            ),
        )
    };
    if let Some(path) = &catalog {
        let _ = std::fs::remove_file(path);
    }
    responses::stop(&mut shim);
    harness::release(&endpoint);
    status
}

pub fn register_codex(app: &mut App) {
    let command = app.add_subcommand("codex", "Open Codex with a model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally codex -m qwen3-4b-instruct-2507",
            "The certified local coding model",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
    ]));
    command.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    command
        .add_option("args", ValueType::Text, "Passed through to codex")
        .multi();
    command.prefix_command(true);
    command.callback(|p, g| {
        if !harness::ensure_installed(TOOL) {
            return 127;
        }
        let model = resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
        run(&model, &p.get_strs("args"), g)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_args_point_codex_at_the_bridge_with_responses_wire_api() {
        let args = codex_args(
            "http://127.0.0.1:5000",
            "glm-5.3-flash",
            1048576,
            None,
            &["exec".to_string()],
        );
        assert_eq!(
            args,
            [
                "-c",
                "model_provider=\"runanywhere\"",
                "-c",
                "model_providers.runanywhere={name=\"RunAnywhere\",base_url=\"http://127.0.0.1:5000/v1\",wire_api=\"responses\",env_key=\"WALLY_CODEX_API_KEY\"}",
                "-c",
                "model_context_window=1048576",
                "-m",
                "glm-5.3-flash",
                "exec",
            ]
        );
    }

    #[test]
    fn codex_args_leave_the_window_to_codex_when_unknown() {
        let args = codex_args("http://127.0.0.1:5000", "m", 0, None, &[]);
        assert!(!args.iter().any(|a| a.starts_with("model_context_window")));
    }

    #[test]
    fn codex_args_name_the_catalog_as_a_toml_string() {
        let path = Path::new("/tmp/state/codex/catalog-1.json");
        let args = codex_args("http://127.0.0.1:5000", "m", 0, Some(path), &[]);
        assert!(
            args.contains(&"model_catalog_json=\"/tmp/state/codex/catalog-1.json\"".to_string())
        );
    }

    #[test]
    fn codex_catalog_lists_every_model_with_its_window() {
        let models = [
            CatalogModel {
                id: "glm-5.3-flash".into(),
                context_window: 1048576,
                ..Default::default()
            },
            CatalogModel {
                id: "mimo-v2.6-pro".into(),
                ..Default::default()
            },
        ];
        let catalog = codex_catalog(&models, true);
        let entries = catalog["models"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["slug"], "glm-5.3-flash");
        assert_eq!(entries[0]["context_window"], 1048576);
        assert_eq!(entries[1]["context_window"], 128000);
        assert_eq!(entries[0]["truncation_policy"]["mode"], "tokens");
    }
}
