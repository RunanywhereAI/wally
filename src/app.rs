//! Shared wally app wiring for the binary and in-process tests (port of
//! src/app.cpp).

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, Outcome};
use crate::cli_formatter::{examples_footer_with_heading, Example};
use crate::io::output;

#[cfg(not(test))]
const WALLY_VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(test)]
const WALLY_VERSION: &str = "0.0.0-dev";

/// Registers the whole command tree on `app` (C++ configure_app).
///
/// Registration order is the --help print order: run first (the primary
/// verb), then llm and models, serve, the coding agents, the cloud account,
/// then diagnostics and maintenance. Every `commands::register_*` call is
/// owned by another port; this only wires root-level flags and the grouped
/// help sections, exactly as C++'s configure_app did.
pub fn configure_app(app: &mut App) {
    app.set_help_flag("-h,--help", "Show help");
    app.set_version_flag(
        "--version,-V",
        &format!("wally {WALLY_VERSION}"),
        "Show the wally version",
    );
    app.require_subcommand(0, 1);
    app.fallthrough(true);

    app.add_flag("--json", "Print results as JSON");
    app.add_flag("-v,--verbose", "Debug logging on stderr");
    app.add_flag("-q,--quiet", "Errors only");
    app.add_flag("--no-progress", "Disable progress rendering");
    app.add_flag("--no-color", "Disable colored --help output");
    app.add_option(
        "--home",
        crate::cli::ValueType::Text,
        "Where models live (default $RUNANYWHERE_HOME or ~/.local/share/runanywhere)",
    );

    // `-u`/`-U` (aliases for `update`/`uninstall`) are deliberately NOT
    // registered as flags here — see run()'s argv check, which is the only
    // place they're honoured (the entire command line and nothing else).

    crate::commands::register_llm_aliases(app); // `run`
    crate::commands::register_llm(app); // `llm` (must precede register_tool)
    crate::commands::register_tool(app); // attaches to `llm`

    // TEMP(llm-only cut): every non-LLM modality is hidden from --help and from
    // execution for this release, as in app.cpp. Uncommenting this block is not
    // enough to bring them back; the same cut also lives in:
    //   - `is_llm` in src/catalog/catalog.rs (catalog, lookups, SDK registration),
    //   - the language-only row filter in src/commands/cmd_list.rs,
    //   - `collect_llm_backend_rows` in cmd_about.rs and cmd_info.rs,
    //   - the `#[ignore]`s in tests/test_wally_unit/diarize.rs,
    //     tests/test_wally_unit/catalog.rs and tests/test_wally_mlx_e2e.rs,
    //   - the TTS/STT/VLM skips in scripts/test/smoke-mlx.sh.
    // crate::commands::register_vlm(app);
    // crate::commands::register_stt(app);
    // crate::commands::register_tts(app);
    // crate::commands::register_vad(app);
    // crate::commands::register_embed(app);
    // crate::commands::register_rerank(app);
    // crate::commands::register_image(app);
    // crate::commands::register_diarize(app);
    // crate::commands::register_segment(app);
    // crate::commands::register_voice(app);
    // crate::commands::register_rag(app);
    // crate::commands::register_lora(app);
    crate::commands::register_models(app);
    crate::commands::register_serve(app);

    crate::commands::register_editors(app);
    crate::commands::register_harness(app); // coding agents
    crate::commands::register_default_models(app);

    crate::commands::register_account(app); // account login/logout/whoami/usage
    crate::commands::register_usage(app); // attaches `usage` under `account`

    // `auth` (device sign-in against the control plane) is a developer path
    // that duplicates `account login`; unregister it, matching app.cpp.
    // crate::commands::register_auth(app);

    crate::commands::register_info(app);
    crate::commands::register_about(app);
    crate::commands::register_version(app);
    crate::commands::register_update(app);
    crate::commands::register_uninstall(app);
    // register_help's callback is bound now, before bench/backends/telemetry
    // (and anything else below) exist; `help_tree` is filled with the
    // COMPLETE tree at the end of this function so the callback still sees
    // them at call time.
    let help_tree = crate::commands::register_help(app);

    crate::commands::register_bench(app); // hidden below
    crate::commands::register_backends(app); // hidden below
    crate::commands::register_telemetry(app); // hidden below

    // Grouped help, by what the reader is trying to do. Sections print in the
    // order their first command was registered above; commands inside a
    // section keep registration order too. A namespace (models, account) is
    // listed as its children with full paths ("models pull"), which the
    // formatter does when a command has visible children.
    //
    // Everything unnamed here is hidden from the page but still parses. Set
    // by name with a guard, so a rename can never leave a stale string.
    struct Section {
        group: &'static str,
        names: &'static [&'static str],
    }
    const SECTIONS: &[Section] = &[
        Section {
            group: "Chat",
            names: &["run", "serve"],
        },
        Section {
            group: "Models",
            names: &["models"],
        },
        Section {
            group: "Coding tools",
            names: &[
                "opencode",
                "claude-code",
                "claude-desktop",
                "hermes",
                "openclaw",
                "deepseek",
            ],
        },
        Section {
            group: "Account",
            names: &["account"],
        },
        Section {
            group: "Wally",
            names: &["about", "update", "uninstall"],
        },
    ];
    for sub in app.subcommands.iter_mut() {
        if !sub.name.is_empty() {
            sub.group.clear();
        }
    }
    for section in SECTIONS {
        for name in section.names {
            if let Some(sub) = app.get_subcommand_mut(name) {
                sub.group(section.group);
            }
            // Not registered in this build; nothing to place — same guard as
            // configure_app()'s try/catch around CLI::OptionNotFound.
        }
    }

    // Last step: hand `help`'s callback the complete, fully-grouped tree.
    *help_tree.borrow_mut() = Some(app.clone());
}

/// The subcommands that hand the terminal to another tool and forward the
/// rest of the command line to it. Kept in step with register_editors and
/// register_harness; a name here that is not a real subcommand is harmless.
fn is_passthrough_command(token: &str) -> bool {
    const NAMES: &[&str] = &[
        "claude-code",
        "claude-desktop",
        "opencode",
        "hermes",
        "openclaw",
        "deepseek",
    ];
    NAMES.contains(&token)
}

/// wally's own flags on those subcommands. `-m`/`--model` take a following
/// value; the rest are booleans. The `=` forms carry their value inline.
fn consumes_following_value(token: &str) -> bool {
    token == "-m" || token == "--model"
}

fn is_wally_flag(token: &str) -> bool {
    token == "-m"
        || token == "--model"
        || token.starts_with("--model=")
        || token.starts_with("-m=")
        || token == "--serve"
        || token == "--restore"
        || token == "--cloud"
        || token == "-h"
        || token == "--help"
}

/// Inserts a `--` ahead of the first token that belongs to the wrapped tool,
/// so the parser stops reading the tool's own flags
/// (`--dangerously-skip-permissions`, `-p`) as unknown wally options and
/// rejecting the whole line. Left untouched when this is not a passthrough
/// command, a `--` is already present, or nothing but wally flags follow.
/// `argv` includes the program name at index 0.
pub fn split_passthrough_argv(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();

    let mut sub = 0usize;
    for (i, token) in out.iter().enumerate().skip(1) {
        if is_passthrough_command(token) {
            sub = i;
            break;
        }
    }
    if sub == 0 {
        return out;
    }

    let mut i = sub + 1;
    while i < out.len() {
        if out[i] == "--" {
            return out; // the reader separated it already
        }
        if !is_wally_flag(&out[i]) {
            out.insert(i, "--".to_string());
            return out;
        }
        i += if consumes_following_value(&out[i]) {
            2
        } else {
            1
        };
    }
    out // only wally flags, nothing to forward
}

/// Builds GlobalOptions from the root command's parse (C++ bound the root
/// options straight into GlobalOptions).
pub fn global_options_from(root: &crate::cli::Parsed) -> GlobalOptions {
    GlobalOptions {
        json: root.flag("--json"),
        verbose: root.flag("--verbose"),
        quiet: root.flag("--quiet"),
        no_progress: root.flag("--no-progress"),
        no_color: root.flag("--no-color"),
        home_override: root.get_str("--home").unwrap_or_default(),
        ..GlobalOptions::default()
    }
}

/// Puts Claude Desktop back on Anthropic when a previous run could not.
///
/// The gateway profile is the one piece of wiring wally leaves on disk rather
/// than in a child process, so it is the one that survives wally being
/// killed. Every later wally run heals it here, whatever the person actually
/// typed. Runs before the parse, so the invoked subcommand is read off argv
/// by hand: the first token that is neither a root option nor a root
/// option's value.
fn restore_stale_desktop_gateway(argv: &[String]) {
    let mut subcommand = "";
    let mut quiet = false;
    let mut i = 1usize;
    while i < argv.len() {
        let arg = argv[i].as_str();
        if arg == "--home" {
            i += 2; // its value, not a subcommand
            continue;
        }
        if arg == "-q" || arg == "--quiet" {
            quiet = true;
            i += 1;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue; // any other root flag
        }
        subcommand = arg;
        break;
    }
    if subcommand == "claude-desktop" {
        return;
    }
    if !crate::desktop::claude_profile::gateway_applied() {
        return;
    }
    if crate::desktop::claude_profile::restore_gateway().is_ok() && !quiet {
        output::status_line("claude desktop was still pointed at a wally endpoint; put it back");
    }
}

/// Run wally with `args` (program name at index 0). Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    restore_stale_desktop_gateway(args);

    // `-u`/`-U` top-level shortcuts, guarded to the one shape that can't be
    // confused with a subcommand's own arguments: the entire command line is
    // the shortcut and nothing else. The documented spelling is still
    // `wally update` / `wally uninstall`.
    if args.len() == 2 {
        let only_arg = args[1].as_str();
        if only_arg == "-u" || only_arg == "--update" {
            let code = crate::commands::run_update(false);
            bootstrap::shutdown();
            return code;
        }
        if only_arg == "-U" || only_arg == "--uninstall" {
            let code = crate::commands::run_uninstall(false);
            bootstrap::shutdown();
            return code;
        }
    }

    // Decided ahead of the parse: a subcommand's help/color decision is
    // settled once, at construction, so this has to be resolved before
    // configure_app() runs. Plain argv scan rather than parsing --no-color
    // for real.
    let no_color_requested = args.iter().skip(1).any(|a| a == "--no-color");

    // Named "wally" outright rather than from argv[0], so the usage line
    // reads the same whether the binary was run through the install
    // wrapper, by full path, or as wally-cxx.
    let mut app = App::new(
        "Run models on this machine or on your RunAnywhere account",
        "wally",
    );
    configure_app(&mut app);
    // Show the local chat and hosted coding-tool paths together. Each example
    // can be pasted, including its explanatory shell comment.
    let footer = examples_footer_with_heading(
        &[
            Example::new(
                "wally models pull qwen3-4b-instruct-2507 && wally run qwen3-4b-instruct-2507",
                "Download a local model and chat on this machine",
            ),
            Example::new(
                "wally account login && wally opencode --cloud -m glm-5.3-flash",
                "Sign in and use a cloud model",
            ),
        ],
        "Get started",
    );
    app.footer(&format!(
        "{footer}\n\nRun \"wally <command> --help\" for details."
    ));

    // `wally help [command]` is a plain-word alias for `--help`, answered
    // here before the parse so the named command is looked up without
    // invoking its callback. Prints to stdout as `--help` does, and reaches
    // shutdown() the same way the Outcome::Help path below does.
    if args.len() >= 2 && args[1] == "help" {
        if args.len() >= 3
            && !args[2].is_empty()
            && !args[2].starts_with('-')
            && app.get_subcommand(&args[2]).is_some()
        {
            let text = app.render_help(&[args[2].clone()], color_enabled(no_color_requested));
            output::result_line(text.trim_end_matches('\n'));
            bootstrap::shutdown();
            return 0;
        }
        // No such command (or none given): fall back to the top-level help.
        let text = app.render_help(&[], color_enabled(no_color_requested));
        output::result_line(text.trim_end_matches('\n'));
        bootstrap::shutdown();
        return 0;
    }

    let forwarded = split_passthrough_argv(args);
    let outcome = app.parse(&forwarded[1..]);

    let exit_code = match outcome {
        Outcome::Ran { code, path } => {
            if path.is_empty() {
                // Bare `wally` prints the top-level help: `out::status_line(app.help())`
                // in C++. `app.help()` already ends in one "\n" of its own, and
                // `status_line`'s `%s\n` appends another, so this one call site
                // deliberately double-newlines (one trailing blank line) unlike
                // every other help-printing path, which trims first. Do not trim
                // here — `render_help` is guaranteed to end in exactly one "\n"
                // (`cli_formatter::tidy`), so passing it straight through
                // reproduces the doubled newline exactly.
                let text = app.render_help(&[], color_enabled(no_color_requested));
                output::status_line(&text);
                0
            } else {
                code
            }
        }
        Outcome::Help { path } => {
            let text = app.render_help(&path, color_enabled(no_color_requested));
            output::result_line(text.trim_end_matches('\n'));
            0
        }
        Outcome::Version { text } => {
            output::result_line(&text);
            0
        }
        Outcome::Required { message, path } => {
            print_parse_error_help(&app, &message, &path, no_color_requested);
            2
        }
        Outcome::Extras { message, path } => {
            print_parse_error_help(&app, &message, &path, no_color_requested);
            2
        }
        Outcome::ParseErr { message } => {
            eprintln!("{message}\nRun with --help for more information.");
            2
        }
    };

    bootstrap::shutdown();
    exit_code
}

/// True when ANSI color is safe to emit: matches
/// `cli_formatter::color_output_enabled`, computed once ahead of the parse
/// the way C++ decided it before `app.formatter(...)`.
fn color_enabled(no_color_requested: bool) -> bool {
    crate::cli_formatter::color_output_enabled(no_color_requested)
}

/// Friendly reply to a parse error on a (sub)command: prints CLI11's own
/// message for what went wrong, followed by the deepest command's help,
/// instead of stopping at the terse top-level usage line.
fn print_parse_error_help(app: &App, message: &str, path: &[String], no_color_requested: bool) {
    output::error_line(message);
    let text = app.render_help(path, color_enabled(no_color_requested));
    eprint!("{}", text);
}
