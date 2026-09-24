//! Regression coverage for the "models" fix group's cross-cutting finding:
//! `wally help <topic>` reached through a leading global flag (id 44).
//!
//! `app::run`'s pre-parse `args[1] == "help"` shortcut only fires when "help"
//! is literally the first token; any leading global flag (`--json`, `-q`,
//! `-v`, `--verbose`, ...) skips it, so the normal parser runs and the `help`
//! subcommand's own registered callback executes instead. That callback is
//! bound at REGISTRATION time, before later subcommands (bench, backends,
//! telemetry, ...) are registered, and previously fell back to the top-level
//! help on any empty/unknown topic — both bugs are covered here. Hermetic:
//! spawns the built binary in an isolated `TempHome` with no network reached
//! (`help` never talks to the control plane) and no real keys.

mod common;

use common::{run_wally, TempHome};

// A leading flag must still resolve a topic registered AFTER `register_help`
// (bench is registered well after it in configure_app), not just fall back
// to the top-level help.
#[test]
fn leading_global_flag_still_resolves_a_late_registered_topic() {
    let home = TempHome::new();
    let (code, stdout, _stderr) = run_wally(&home, &["--json", "help", "bench"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Measure throughput and load time of downloaded models"),
        "expected bench's own help, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Usage: wally bench"),
        "expected bench's own usage line, got:\n{stdout}"
    );
}

#[test]
fn leading_global_flag_resolves_backends_topic_too() {
    let home = TempHome::new();
    let (code, stdout, _stderr) = run_wally(&home, &["-q", "help", "backends"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("List registered inference backends"),
        "expected backends' own help, got:\n{stdout}"
    );
}

// No topic, or an unknown one, must render the `help` subcommand's OWN help
// ("Show help for a command" / "Usage: wally help ..."), matching CLI11's
// app.help() delegating to whichever subcommand was actually parsed — never
// the top-level "Run models on this machine..." help.
#[test]
fn leading_global_flag_with_no_topic_renders_helps_own_help_not_top_level() {
    let home = TempHome::new();
    let (code, stdout, _stderr) = run_wally(&home, &["-v", "help"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Show help for a command"),
        "expected help's own description, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Usage: wally help"),
        "expected help's own usage line, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("Run models on this machine or on your RunAnywhere account"),
        "must not fall back to the top-level help, got:\n{stdout}"
    );
}

#[test]
fn leading_global_flag_with_an_unknown_topic_renders_helps_own_help() {
    let home = TempHome::new();
    let (code, stdout, _stderr) = run_wally(&home, &["--json", "help", "nonexistent"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Show help for a command"),
        "expected help's own description, got:\n{stdout}"
    );
}

// "pull" is only reachable as "models pull", not a bare top-level name, so
// this is also an "unknown topic" case: help-of-help, not top-level.
#[test]
fn leading_global_flag_with_a_nested_only_name_renders_helps_own_help() {
    let home = TempHome::new();
    let (code, stdout, _stderr) = run_wally(&home, &["--verbose", "help", "pull"]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("Show help for a command"),
        "expected help's own description, got:\n{stdout}"
    );
}
