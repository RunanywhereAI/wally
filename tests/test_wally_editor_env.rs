//! What a bundle launch tells the wrapped tool (port of
//! tests/test_wally_editor_env.cpp).
//!
//! `wally claude-code` has two launch paths and they wire the environment
//! differently. A terminal launch exports variables into its own process and the
//! child inherits them; a macOS bundle inherits nothing, because launchd starts
//! it from the reader's login session, so every variable has to be listed on
//! `open --env` by hand. That asymmetry is how the bundle path came to run
//! unwired: it is not enough to set a variable once, it has to be set in two
//! places, and nothing failed when only one of them was.
//!
//! These assert on the argument vector rather than on a launched process. A test
//! that really started Claude Code would need Claude Code, a signed-in cloud
//! session and a GPU at the other end, which is why this wiring had no test at
//! all. The argument vector is the whole of what the function decides, so it is
//! the right thing to assert on.

use wally::anthropic::Shim;
use wally::commands::editor_env::open_args;

fn running_shim() -> Shim {
    Shim {
        running: true,
        base_url: "http://127.0.0.1:8765".to_string(),
        auth_token: "sk-runa-test-token".to_string(),
    }
}

/// The value `open --env NAME=value` carries for `name`, or "" when absent.
///
/// Reads the vector the way `open(1)` does -- the value is the argument AFTER
/// each `--env` -- rather than searching the whole vector for a substring. A
/// substring search would pass on a value that appeared anywhere at all,
/// including inside a passthrough argument, which is the shape of assertion
/// `.claude/rules` calls out as not being a test.
fn env_value(args: &[String], name: &str) -> String {
    let prefix = format!("{name}=");
    for pair in args.windows(2) {
        if pair[0] == "--env" && pair[1].starts_with(&prefix) {
            return pair[1][prefix.len()..].to_string();
        }
    }
    String::new()
}

fn has_any_env(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "--env")
}

#[test]
fn the_model_is_carried_to_a_bundle() {
    let args = open_args(
        "/Applications/Claude.app",
        &running_shim(),
        &[],
        "glm-5.3-flash",
    );

    // The defect this closes: Wally 0.5.6 printed `using glm-5.3-flash`, served
    // GLM, and the wrapped tool reported `claude-sonnet-5` as the model that
    // answered -- because nothing here ever told it otherwise.
    assert_eq!(env_value(&args, "ANTHROPIC_MODEL"), "glm-5.3-flash");

    // The one that is easy to miss. The wrapped tool makes background requests of
    // its own -- titles, summaries -- and resolves them through its `haiku` alias
    // rather than the main model. Those reach us too and are served by the
    // selected model like everything else, so without this they stay labelled as
    // a model nothing ever contacted. Measured 2026-09-12: an auxiliary GLM pair
    // of 766/599 tokens costing 1,778 micros, and 816/821 for Qwen at 2,790.
    assert_eq!(
        env_value(&args, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
        "glm-5.3-flash"
    );
}

#[test]
fn the_endpoint_still_goes_with_it() {
    let args = open_args(
        "/Applications/Claude.app",
        &running_shim(),
        &[],
        "glm-5.3-flash",
    );

    // Regression guard on what was already right: adding variables must not
    // displace the two that make the launch reach us at all.
    assert_eq!(
        env_value(&args, "ANTHROPIC_BASE_URL"),
        "http://127.0.0.1:8765"
    );
    assert_eq!(
        env_value(&args, "ANTHROPIC_AUTH_TOKEN"),
        "sk-runa-test-token"
    );
    // Never a key beside the token: the token already outranks a key, and setting
    // one is what makes the wrapped tool warn that claude.ai connectors are off.
    assert!(env_value(&args, "ANTHROPIC_API_KEY").is_empty());
}

#[test]
fn no_model_means_no_model_variables() {
    // `wally claude-code` with no model named does no wiring at all, so the tool
    // runs exactly as the reader configured it. An empty value would not be
    // neutral: the wrapped tool reads `ANTHROPIC_MODEL=` as a model named "",
    // which is not the same as leaving its own configuration alone.
    let args = open_args("/Applications/Claude.app", &Shim::default(), &[], "");
    assert!(
        !has_any_env(&args),
        "an unwired launch exported environment variables"
    );
}

#[test]
fn a_stopped_shim_carries_nothing() {
    let stopped = Shim {
        running: false,
        ..Shim::default()
    };
    let args = open_args("/Applications/Claude.app", &stopped, &[], "glm-5.3-flash");
    // There is nothing to point the tool at, so naming a model would be worse
    // than silence: it would report a model it cannot reach.
    assert!(
        !has_any_env(&args),
        "a stopped shim still exported environment variables"
    );
}

#[test]
fn variables_stay_before_the_apps_own_arguments() {
    let passthrough = vec!["--resume".to_string(), "abc".to_string()];
    let args = open_args(
        "/Applications/Claude.app",
        &running_shim(),
        &passthrough,
        "glm-5.3-flash",
    );

    let app_args_at = args.iter().position(|arg| arg == "--args");
    let app_args_at = app_args_at.expect("passthrough arguments were dropped");
    // `open` hands everything after `--args` to the app as its own argv, so a
    // `--env` appearing after it would become a parameter to the app instead of
    // an environment variable -- set nowhere, and silently.
    assert!(
        !args[app_args_at..].iter().any(|arg| arg == "--env"),
        "an --env pair landed after --args, where open ignores it"
    );
}
