//! Port of tests/test_wally_formatter.cpp.
//!
//! `crate::cli_formatter::make_help` never has a `CLI::AppFormatMode::Sub`
//! counterpart (see the module doc comment on cli_formatter.rs: wally never
//! calls `set_help_all_flag`, so that path is dead code in the C++ and every
//! real `--help` -- including a subcommand's own -- renders through what the
//! C++ called `Normal`). The C++ test's `mode` parameter, which alternated
//! between rendering the root app and directly invoking the `Sub` path on a
//! child, is therefore ported as rendering the root app vs. rendering that
//! same child through the one path Rust has: `App::render_help`.

use wally::cli_formatter::{examples_footer, examples_footer_with_heading, Example};

/// Strips ANSI SGR escape sequences (`\x1b[...m`), the same substitution
/// `StripColor` (tests/test_wally_formatter.cpp) makes with its regex.
fn strip_color(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next(); // '['
            for next in chars.by_ref() {
                if next == 'm' {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Builds the same little app the C++ `Render` helper does: a root command
/// with a long-wrapping option and its own "Get started" footer, and an
/// "opencode" subcommand with its own "Examples" footer.
fn build_test_app() -> wally::cli::App {
    let mut app = wally::cli::App::new("Run local and cloud models", "wally");
    app.set_help_flag("-h,--help", "Show help");
    app.add_flag(
        "--flag",
        "A long explanation of an option that should wrap within an ordinary terminal \
         without moving the next line back to the left margin.",
    );
    app.add_subcommand("opencode", "Open a coding tool")
        .footer(&examples_footer(&[
            Example::new("wally opencode -m qwen3-0.6b", "Use a downloaded model"),
            Example::new(
                "wally opencode --cloud -m glm-5.3-flash",
                "Use a cloud model",
            ),
        ]));
    app.footer(&examples_footer_with_heading(
        &[Example::new("wally models list", "Browse models")],
        "Get started",
    ));
    app
}

#[test]
fn examples_are_copyable() {
    let command =
        "wally opencode -m \"/a path containing spaces/model.gguf\" -- run \"explain this project\"";
    let footer = examples_footer(&[
        Example::new(
            command,
            "A long explanation that wraps onto a second line without adding prose to the \
             command or changing any of its arguments when the example is pasted into a shell",
        ),
        Example::new("wally account login", ""),
    ]);
    let mut notes = 0;
    let mut commands = 0;
    for line in footer.lines() {
        assert_eq!(line, line.trim_end(), "footer has trailing whitespace");
        if let Some(rest) = line.strip_prefix("  # ") {
            let _ = rest;
            notes += 1;
            assert!(line.len() <= 80, "note exceeds the terminal width");
        } else if line.starts_with("  ") {
            assert!(
                line == format!("  {command}") || line == "  wally account login",
                "command was wrapped or its arguments changed"
            );
            commands += 1;
        }
    }
    assert!(notes >= 2, "expected wrapped comments");
    assert_eq!(commands, 2, "expected two intact commands");
    assert!(!footer.ends_with('\n'));
}

#[test]
fn color_preserves_readable_text() {
    // (path, expected footer heading) -- the root app's own footer vs. the
    // "opencode" subcommand's, exactly what the C++ test's Normal/Sub modes
    // rendered.
    for (path, heading) in [
        (&[][..], "Get started:"),
        (&["opencode".to_string()][..], "Examples:"),
    ] {
        let plain = build_test_app().render_help(path, false);
        let colored = build_test_app().render_help(path, true);
        assert!(
            !plain.contains('\u{1b}'),
            "plain-text help must carry no escape codes"
        );
        assert_eq!(
            strip_color(&colored),
            plain,
            "color must not change text or layout"
        );
        assert!(
            colored.contains("\u{1b}[1;36m  wally "),
            "footer commands must be cyan"
        );
        assert!(
            colored.contains(&format!("\u{1b}[1m{heading}")),
            "footer heading must be bold"
        );
    }
}

#[test]
fn help_fits_standard_terminal() {
    let help = build_test_app().render_help(&[], false);
    for line in help.lines() {
        assert!(line.len() <= 80, "line exceeds 80 columns: {line}");
        assert_eq!(line, line.trim_end(), "help has trailing whitespace");
    }
    assert!(!help.contains("\n\n\n"), "help has excess blank lines");
    let bytes = help.as_bytes();
    assert_eq!(bytes.last(), Some(&b'\n'));
    assert_ne!(bytes.get(bytes.len().saturating_sub(2)), Some(&b'\n'));
}
