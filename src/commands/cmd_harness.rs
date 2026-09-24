//! Port of src/commands/cmd_harness.cpp.

use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::resolve_default_model;
use crate::harness;
use crate::io::output as out;

pub fn register_harness(app: &mut App) {
    // `wally opencode <model>` rather than a flag on `run`: it hands the terminal
    // to another program, which is a different thing to do than talk to a model.
    let opencode = app.add_subcommand("opencode", "Open opencode with a model");
    opencode.footer(&examples_footer(&[
        Example::new("wally opencode -m qwen3-0.6b", "A model on this machine"),
        Example::new(
            "wally opencode --cloud -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
    ]));
    // A named option rather than a positional: with two positionals there is no
    // way to tell `wally opencode run` asking for passthrough from someone
    // naming a model called run, and the first reading wins silently.
    opencode.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    opencode.add_flag(
        "--cloud",
        "Use your account's hosted endpoint (never a local model)",
    );
    opencode
        .add_option("args", ValueType::Text, "Passed through to opencode")
        .multi();
    opencode.prefix_command(true);
    opencode.callback(|p, g| {
        // Before resolving a model or printing anything: a person without the
        // tool should see only that it is missing and how to install it.
        if !harness::ensure_installed("opencode") {
            return 127;
        }
        let effective =
            resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
        if p.flag("--cloud") {
            if effective.is_empty() {
                out::error_line(
                    "--cloud requires --model <console-model-id>, and no default is set \
                     (wally models default <id>)",
                );
                return 2;
            }
            return harness::launch_open_code_cloud(&effective, &p.get_strs("args"));
        }
        harness::launch("opencode", &effective, &p.get_strs("args"))
    });

    // The OpenAI-shaped agents, one subcommand per row of the table. They need
    // no translator, so there is nothing here but resolving the model and
    // handing the endpoint over the way each one takes it.
    for agent in harness::agents() {
        let agent = *agent;
        let command = app.add_subcommand(agent.id, agent.summary);
        let invocation = format!("wally {}", agent.id);
        command.footer(&examples_footer(&[
            Example::new(
                &format!("{invocation} -m qwen3-0.6b"),
                "A model on this machine",
            ),
            Example::new(
                &format!("{invocation} -m glm-5.3-flash"),
                "A hosted model (needs `wally account login`)",
            ),
        ]));
        command.add_option(
            "-m,--model",
            ValueType::Text,
            "A model on this machine, or a hosted one from your account",
        );
        command
            .add_option("args", ValueType::Text, "Passed through to the tool")
            .multi();
        command.prefix_command(true);
        command.callback(move |p, g| {
            // Same as opencode: check the tool is here before resolving a model
            // or printing a preamble.
            if !harness::ensure_installed(agent.command) {
                return 127;
            }
            harness::launch_agent(
                &agent,
                &resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color),
                &p.get_strs("args"),
            )
        });
    }
}
