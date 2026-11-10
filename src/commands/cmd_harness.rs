//! Port of src/commands/cmd_harness.cpp.

use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::resolve_default_model;
use crate::harness;
use crate::io::output as out;

/// `--hybrid`, `--cloud-model` and `--policy`, the same on every harness that
/// has a hybrid launcher.
pub(crate) fn add_hybrid_flags(command: &mut App) {
    command.add_flag(
        "--hybrid",
        "Run the main agent on a cloud model and its subagents on a local model",
    );
    command.add_option(
        "--cloud-model",
        ValueType::Text,
        "The cloud model the main agent uses with --hybrid (default: your default model)",
    );
    command.add_option(
        "--policy",
        ValueType::Text,
        "What --hybrid optimises for: privacy (default) or cost",
    );
    command.add_flag(
        "--custom",
        "After --hybrid, ask for the local context window and the cloud model",
    );
}

/// The hybrid launch a parsed command asks for: `Ok(Some((local, cloud,
/// policy)))` with `--hybrid`, `Ok(None)` without it, `Err(2)` when
/// `--cloud-model` or `--policy` came without `--hybrid` or the cloud model
/// is a decision model.
pub(crate) fn hybrid_request(
    p: &crate::cli::Parsed,
    no_color: bool,
) -> Result<Option<(String, String, String)>, i32> {
    if !p.flag("--hybrid") {
        for flag in ["--cloud-model", "--policy", "--custom"] {
            if flag == "--custom" {
                if p.flag("--custom") {
                    out::error_line("--custom only applies with --hybrid");
                    return Err(2);
                }
                continue;
            }
            if p.get_str(flag).is_some_and(|value| !value.is_empty()) {
                out::error_line(&format!("{flag} only applies with --hybrid"));
                return Err(2);
            }
        }
        return Ok(None);
    }
    // `-m` names the *local* model here, so an absent one must not be filled
    // with the cloud default.
    let local = p.get_str("--model").unwrap_or_default();
    let cloud = if p.flag("--custom") {
        // A passed id is only a suggestion for the questions. Do not fill in
        // the account default before the person has answered.
        p.get_str("--cloud-model").unwrap_or_default()
    } else {
        p.get_str("--cloud-model").unwrap_or_else(|| {
            saved_cloud_model().unwrap_or_else(|| resolve_default_model("", no_color))
        })
    };
    if !cloud.is_empty() && harness::refuse_decisions_model(Some(cloud.as_str())) {
        return Err(2);
    }
    let policy = p
        .get_str("--policy")
        .unwrap_or_else(|| "privacy".to_string());
    Ok(Some((local, cloud, policy)))
}

pub fn register_harness(app: &mut App) {
    // `wally opencode <model>` rather than a flag on `run`: it hands the terminal
    // to another program, which is a different thing to do than talk to a model.
    let opencode = app.add_subcommand("opencode", "Open OpenCode with a local or cloud model");
    opencode.footer(&examples_footer(&[
        Example::new(
            "wally models pull qwen3-4b-instruct-2507",
            "Download the certified local model once",
        ),
        Example::new(
            "wally opencode -m qwen3-4b-instruct-2507",
            "Start OpenCode with the certified local model",
        ),
        Example::new(
            "wally opencode --cloud -m glm-5.3-flash",
            "Use a cloud model after wally account login",
        ),
        Example::new(
            "wally opencode -m qwen3-4b-instruct-2507 -- run \"explain this project\"",
            "Pass arguments through to OpenCode after --",
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
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
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
        harness::launch("opencode", &effective, &p.get_strs("args"), g)
    });

    // Shannon is not a coding harness: it is an open-source tool for
    // authorised security testing that runs its own Docker and Temporal stack.
    // So it has its own launcher, which writes the model config it needs and
    // runs the pinned release, rather than a row in the agents table.
    let shannon = app.add_subcommand(
        "shannon",
        "Test your own apps with Shannon, an AI pentester",
    );
    shannon.footer(&examples_footer(&[
        Example::new(
            "wally shannon -- -u https://staging.example.com -r ./my-repo",
            "Scan a target you are authorised to test (starts a scan)",
        ),
        Example::new(
            "wally shannon -m glm-5.3-flash -- -u https://staging.example.com -r ./my-repo",
            "Choose the model for the scan",
        ),
        Example::new("wally shannon -- scans", "List running and finished scans"),
        Example::new("wally shannon -- logs my-scan", "Follow a scan's log"),
    ]));
    shannon.add_option(
        "-m,--model",
        ValueType::Text,
        "A model from your account (default: your default model)",
    );
    shannon
        .add_option(
            "args",
            ValueType::Text,
            "Shannon's own command and options; with none, start",
        )
        .multi();
    shannon.prefix_command(true);
    shannon.callback(|p, _g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        harness::shannon::launch(p.get_str("--model").as_deref(), &p.get_strs("args"))
    });

    let deepsec = app.add_subcommand("deepsec", "Scan your own repos with DeepSec");
    deepsec.footer(&examples_footer(&[
        Example::new(
            "wally deepsec -- init . ./my-repo",
            "Start a scan on a repo you are authorised to test (runs init)",
        ),
        Example::new(
            "wally deepsec -m glm-5.3-flash -- process",
            "Choose the model",
        ),
        Example::new(
            "wally deepsec -- status",
            "Other DeepSec commands pass through",
        ),
    ]));
    deepsec.add_option(
        "-m,--model",
        ValueType::Text,
        "A model from your account (default: your default model)",
    );
    deepsec
        .add_option(
            "args",
            ValueType::Text,
            "DeepSec's own command and options; with none, init",
        )
        .multi();
    deepsec.prefix_command(true);
    deepsec.callback(|p, _g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        harness::deepsec::launch(p.get_str("--model").as_deref(), &p.get_strs("args"))
    });

    let vvah = app.add_subcommand("vvah", "Scan your own repos with VVAH");
    vvah.footer(&examples_footer(&[
        Example::new(
            "wally vvah",
            "Scan and remediate (edits files in) the current repo",
        ),
        Example::new(
            "wally vvah -m glm-5.3-flash -- scan --repo ./my-repo",
            "Scan another path with a chosen model",
        ),
        Example::new("wally vvah -- report", "Other VVAH commands pass through"),
    ]));
    vvah.add_option(
        "-m,--model",
        ValueType::Text,
        "A model from your account (default: your default model)",
    );
    vvah.add_option(
        "args",
        ValueType::Text,
        "VVAH's own command and options; with none, scan --repo . --remediate",
    )
    .multi();
    vvah.prefix_command(true);
    vvah.callback(|p, _g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        harness::vvah::launch(p.get_str("--model").as_deref(), &p.get_strs("args"))
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
                "wally models pull qwen3-4b-instruct-2507",
                "Download the certified local model once",
            ),
            Example::new(
                &format!("{invocation} -m qwen3-4b-instruct-2507"),
                "Start the tool with the certified local model",
            ),
            Example::new(
                &format!("{invocation} -m glm-5.3-flash"),
                "Use a cloud model after wally account login",
            ),
        ]));
        command.add_option(
            "-m,--model",
            ValueType::Text,
            "A model on this machine, or a hosted one from your account",
        );
        if harness::HYBRID_COMMANDS.contains(&agent.id) {
            add_hybrid_flags(command);
        }
        command
            .add_option("args", ValueType::Text, "Passed through to the tool")
            .multi();
        command.prefix_command(true);
        command.callback(move |p, g| {
            if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
                return 2;
            }
            // Same as opencode: check the tool is here before resolving a model
            // or printing a preamble.
            if !harness::ensure_installed(agent.command) {
                return 127;
            }
            if harness::HYBRID_COMMANDS.contains(&agent.id) {
                match hybrid_request(p, g.no_color) {
                    Err(status) => return status,
                    Ok(Some((mut local, mut cloud, policy))) => {
                        if let Some(context) = saved_local_context() {
                            let context = context.max(context_min_for(agent.id));
                            std::env::set_var("PLS_MAX_KV", context.to_string());
                            if p.flag("--custom") {
                                local = "mlx-pplx-coder-35b-4bit".to_string();
                            }
                            if p.get_str("--cloud-model").is_none() {
                                if let Some(saved) = saved_cloud_model() {
                                    cloud = saved;
                                }
                            }
                            out::status_line(&format!(
                                "hybrid defaults: context {context}, cloud {cloud}"
                            ));
                        } else if p.flag("--custom") {
                            match custom_hybrid(agent.id, &cloud) {
                                Err(status) => return status,
                                Ok(choice) => {
                                    cloud = choice.cloud;
                                    local = "mlx-pplx-coder-35b-4bit".to_string();
                                    std::env::set_var("PLS_MAX_KV", choice.context.to_string());
                                    if choice.save {
                                        save_local_context(choice.context);
                                    }
                                }
                            }
                        }
                        return harness::launch_hybrid(
                            &agent,
                            &local,
                            &cloud,
                            &policy,
                            &p.get_strs("args"),
                            g,
                        );
                    }
                    Ok(None) => {}
                }
            }
            let model =
                resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
            harness::launch_agent(&agent, &model, &p.get_strs("args"), g)
        });
    }
}

/// What `--custom` collected.
struct CustomChoice {
    context: i64,
    cloud: String,
    save: bool,
}

/// Lowest local window. A new sub-agent prompt in this session was about 14k tokens.
const CONTEXT_MIN: i64 = 16_384;
/// `max_position_embeddings` of mlx-pplx-coder-35b.
const CONTEXT_MAX: i64 = 262_144;
/// Hermes will not delegate to a model that advertises under this.
const HERMES_CONTEXT_MIN: i64 = 64_000;
/// Process peak at 16,384 tokens on the 35B coder: 22.156 GiB.
const PPLX_PEAK_AT_16K: u64 = 23_788_551_831;
/// Process-peak growth between the 4,096 and 16,384 runs: 133,256 bytes per token.
const PPLX_PEAK_PER_TOKEN: u64 = 133_256;

/// The context window whose measured prefill peak fits in `available` bytes.
fn context_for_free_ram(available: u64) -> i64 {
    let tokens =
        CONTEXT_MIN as u64 + available.saturating_sub(PPLX_PEAK_AT_16K) / PPLX_PEAK_PER_TOKEN;
    (tokens as i64).clamp(CONTEXT_MIN, CONTEXT_MAX)
}

fn context_min_for(harness: &str) -> i64 {
    if harness == "hermes" {
        HERMES_CONTEXT_MIN
    } else {
        CONTEXT_MIN
    }
}

fn config_json() -> serde_json::Value {
    let path = crate::harness::pls::config_file();
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn saved_local_context() -> Option<i64> {
    config_json()
        .get("local_context")
        .and_then(|value| value.as_i64())
        .filter(|value| *value > 0)
}

fn saved_cloud_model() -> Option<String> {
    saved_local_context()?;
    config_json()
        .get("cloud_model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn save_local_context(context: i64) {
    let path = crate::harness::pls::config_file();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
    let mut value = config_json();
    value["local_context"] = serde_json::json!(context);
    let _ = std::fs::write(path, value.to_string());
}

fn available_bytes() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut page = 16_384u64;
    let mut pages = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.split("page size of ").nth(1) {
            if let Some(n) = rest.split_whitespace().next() {
                if let Ok(size) = n.parse::<u64>() {
                    page = size;
                }
            }
        }
        let interesting = line.contains("Pages free")
            || line.contains("Pages inactive")
            || line.contains("Pages speculative");
        if !interesting {
            continue;
        }
        let number = line
            .split(':')
            .nth(1)
            .unwrap_or("")
            .trim()
            .trim_end_matches('.');
        if let Ok(count) = number.parse::<u64>() {
            pages += count;
        }
    }
    if pages == 0 {
        None
    } else {
        Some(pages.saturating_mul(page))
    }
}

fn ask(prompt: &str) -> Result<String, i32> {
    if !crate::util::term::stdin_is_tty() || !crate::util::term::stderr_is_tty() {
        out::error_line("--custom needs a terminal");
        return Err(2);
    }
    eprint!("{prompt}");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        out::error_line("no answer");
        return Err(2);
    }
    Ok(line.trim().to_string())
}

fn printable_model_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_graphic())
}

fn cloud_ids() -> Result<Vec<String>, i32> {
    let mut session =
        match crate::account::ConsoleSession::open(crate::account::ConsoleClient::default()) {
            Ok(session) => session,
            Err(error) => {
                let cached = crate::account::cached_model_ids();
                if cached.is_empty() {
                    out::error_line(&error);
                    return Err(2);
                }
                return Ok(cached
                    .into_iter()
                    .filter(|id| printable_model_id(id))
                    .collect());
            }
        };
    match session.call(|client, url, token| client.fetch_models_within(url, token, 8_000)) {
        Ok(models) => {
            let ids: Vec<String> = models
                .into_iter()
                .map(|model| model.id)
                .filter(|id| printable_model_id(id))
                .collect();
            if ids.is_empty() {
                out::error_line("the account has no cloud models");
                Err(2)
            } else {
                Ok(ids)
            }
        }
        Err(error) => {
            let cached = crate::account::cached_model_ids();
            if cached.is_empty() {
                out::error_line(&error);
                Err(2)
            } else {
                Ok(cached
                    .into_iter()
                    .filter(|id| printable_model_id(id))
                    .collect())
            }
        }
    }
}

fn pick_cloud(passed: &str, ids: &[String]) -> Result<String, i32> {
    if !passed.is_empty() {
        let use_passed = harness::confirm(&format!("Use {passed} as the cloud model? [y/N] "));
        if use_passed {
            return Ok(passed.to_string());
        }
    }
    out::status_line("Cloud models:");
    for (index, id) in ids.iter().enumerate() {
        eprintln!("  {}: {id}", index + 1);
    }
    loop {
        let answer = ask("Type the cloud model id, or its number: ")?;
        if let Ok(index) = answer.parse::<usize>() {
            if let Some(id) = index.checked_sub(1).and_then(|index| ids.get(index)) {
                return Ok(id.clone());
            }
        }
        if ids.iter().any(|id| id == &answer) {
            return Ok(answer);
        }
        out::error_line("that model is not in the list");
    }
}

fn custom_hybrid(harness: &str, passed_cloud: &str) -> Result<CustomChoice, i32> {
    let min = context_min_for(harness);
    let context = loop {
        let answer = ask(&format!("Context window ({min}-{CONTEXT_MAX}), or auto: "))?;
        if answer.eq_ignore_ascii_case("auto") {
            let Some(free) = available_bytes() else {
                out::error_line("could not read free memory; type a number");
                continue;
            };
            let picked = context_for_free_ram(free).max(min);
            out::status_line(&format!(
                "auto context window: {picked} ({:.1} GB free)",
                free as f64 / 1024.0 / 1024.0 / 1024.0
            ));
            break picked;
        }
        match answer.parse::<i64>() {
            Ok(value) if (min..=CONTEXT_MAX).contains(&value) => break value,
            _ => out::error_line(&format!(
                "type a number from {min} to {CONTEXT_MAX}, or auto"
            )),
        }
    };

    let ids = cloud_ids()?;
    let cloud = pick_cloud(passed_cloud, &ids)?;
    if harness::refuse_decisions_model(Some(cloud.as_str())) {
        return Err(2);
    }
    let save = harness::confirm("Make this the default hybrid configuration? [y/N] ");
    Ok(CustomChoice {
        context,
        cloud,
        save,
    })
}

#[cfg(test)]
mod custom_tests {
    use super::*;

    #[test]
    fn auto_context_fits_the_measured_35b_cost() {
        let available = PPLX_PEAK_AT_16K + (65_536 - CONTEXT_MIN as u64) * PPLX_PEAK_PER_TOKEN;
        assert_eq!(context_for_free_ram(available), 65_536);
    }

    #[test]
    fn auto_context_stays_inside_the_model_limits() {
        assert_eq!(context_for_free_ram(0), CONTEXT_MIN);
        assert_eq!(context_for_free_ram(u64::MAX), CONTEXT_MAX);
    }
}
