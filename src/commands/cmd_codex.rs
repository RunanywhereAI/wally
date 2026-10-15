//! `wally codex`: Codex CLI against a local or hosted model, through the
//! loopback Responses translator (`crate::codex`).
//!
//! Its own command rather than a row in `cmd_editors`' table: that table is the
//! Anthropic-shaped tools, and Codex is wired by command-line overrides, not
//! environment variables.

use std::path::Path;

use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::codex;
use crate::commands::cmd_editors::{
    cloud_context_window, context_window_for_tool, context_window_problem, text_only_ids,
    write_private, RemovedOnDrop, ScopedEnv,
};
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::harness::{self, codex as codex_config};
use crate::io::output as out;

const COMMAND: &str = "codex";

/// The model catalog Codex reads at startup, in a file wally owns for this
/// session only. None when there is nowhere to write it; Codex then runs on
/// its fallback metadata, which works but warns.
fn write_model_catalog(catalog: &[harness::CatalogModel]) -> RemovedOnDrop {
    let state_dir = cli_paths::state_dir();
    if state_dir.is_empty() {
        return RemovedOnDrop(None);
    }
    let dir = Path::new(&state_dir).join("codex");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        out::status_line(&format!("could not write Codex's model catalog ({error})"));
        return RemovedOnDrop(None);
    }
    let path = dir.join(format!("wally-models-{}.json", std::process::id()));
    match write_private(&path, &codex_config::codex_model_catalog(catalog)) {
        Ok(()) => RemovedOnDrop(Some(path.to_string_lossy().into_owned())),
        Err(error) => {
            out::status_line(&format!("could not write Codex's model catalog ({error})"));
            RemovedOnDrop(None)
        }
    }
}

/// Holds the translator open and prints what to point at it, for a client
/// that speaks the Responses API but is not Codex.
fn serve(model: &str, options: &GlobalOptions) -> i32 {
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::result_line(&format!("OPENAI_BASE_URL={}", shim.base_url));
    out::result_line(&format!("OPENAI_API_KEY={}", shim.auth_token));
    out::status_line(&format!("serving {model}; press Ctrl-C to stop"));
    // No signal handling: Ctrl-C ends the process, and the OS reclaims the port
    // and the model.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn run(
    model: &str,
    args: &[String],
    context_override: Option<i64>,
    options: &GlobalOptions,
) -> i32 {
    if model.is_empty() {
        // No model named means no wiring to do: Codex runs exactly as the
        // reader has it configured. Same contract as `wally opencode`.
        return harness::launch(COMMAND, "", args, options);
    }
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    // Read once: it names the models Codex lists, their windows, and which of
    // them take no images.
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(mut shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::status_line(&format!(
        "codex will talk to {model} through {}",
        shim.base_url
    ));
    let real = if endpoint.serving {
        endpoint.context_window
    } else if catalog[0].context_window > 0 {
        catalog[0].context_window
    } else {
        cloud_context_window(model)
    };
    let context = context_window_for_tool(real, context_override);
    if context > 0 {
        out::status_line(&format!("context window: {context} tokens"));
    }

    // The catalog carries each model's window too, and Codex reads it from
    // there for the launched model; a smaller asked-for window goes in both.
    let mut catalog = catalog;
    if context > 0 {
        catalog[0].context_window = context;
    }
    let catalog_file = write_model_catalog(&catalog);
    let overrides =
        codex_config::codex_overrides(&shim.base_url, catalog_file.0.as_deref(), context);
    // The loopback token, never the upstream key, and only for this child.
    let status = {
        let _key = ScopedEnv::new(codex_config::CODEX_KEY_VARIABLE, &shim.auth_token);
        harness::launch(
            COMMAND,
            "",
            &codex_config::codex_argv(&overrides, model, args),
            options,
        )
    };

    codex::stop(&mut shim);
    harness::release(&endpoint);
    status
}

pub fn register_codex(app: &mut App) {
    let command = app.add_subcommand(COMMAND, "Open Codex with a model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally codex -m qwen3-4b-instruct-2507",
            "The certified local coding model",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash -- exec \"explain this project\"",
            "Pass arguments through to Codex after --",
        ),
    ]));
    command.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    command.add_option(
        "--context-window",
        ValueType::Int64,
        "Tell Codex the model holds this many tokens, so it compacts sooner",
    );
    command.add_flag(
        "--serve",
        "Print the endpoint and keep it open instead of launching",
    );
    // Tokens after the wally flags belong to Codex, its own flags included;
    // `run()` inserts the `--` that lets them through (SplitPassthroughArgv).
    command
        .add_option("args", ValueType::Text, "Passed through to codex")
        .multi();
    command.prefix_command(true);
    command.callback(|p, g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        if let Some(problem) = context_window_problem(p.get_i64("--context-window")) {
            out::error_line(&problem);
            return 2;
        }
        // A missing tool shows only that it is missing and how to get it,
        // before any model resolution. --serve launches nothing.
        if !p.flag("--serve") && !harness::ensure_installed(COMMAND) {
            return 127;
        }
        let effective =
            resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
        if p.flag("--serve") {
            if effective.is_empty() {
                out::error_line(
                    "--serve requires --model <id>, and no default is set (wally models default <id>)",
                );
                return 2;
            }
            serve(&effective, g)
        } else {
            run(
                &effective,
                &p.get_strs("args"),
                p.get_i64("--context-window"),
                g,
            )
        }
    });
}
