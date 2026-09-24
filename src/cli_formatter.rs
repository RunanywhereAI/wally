//! `--help` layout for wally (port of src/cli_formatter.cpp): a `Usage:` line,
//! sentence-case section headings, one `-m, --model TEXT` column for options,
//! commands grouped by intent with namespaces shown as full paths
//! (`models pull`), and a verbatim `Examples:` footer. On a terminal, headings
//! are bold and anything typeable is cyan; anywhere else the text is plain and
//! byte-identical.
//!
//! This is a from-scratch port, not just of `cli_formatter.cpp` but of every
//! piece of CLI11's own `Formatter`/`FormatterBase` it calls into (wally never
//! overrode `make_positionals`, `make_groups`, `make_description` or the
//! word-wrap helper, so those are reproduced from `CLI11.hpp` directly).
//! wally never calls `set_help_all_flag`, so `CLI::AppFormatMode::All`/`Sub`
//! are dead code in the C++ and have no counterpart here: `make_help` only
//! renders `AppFormatMode::Normal`.
//!
//! `crate::cli::Opt.names` is documented as already stripped of CLI11's
//! `{default}` flag-value suffixes, and already split into the short/long
//! names CLI11 would produce from `Option::get_name(false, true)` — so unlike
//! the C++, there is no name-joining/re-splitting round trip here, only a
//! short/long partition by `--` prefix.
//!
//! `-h, --help` and (root-only) `-V, --version` live in `App.help_flag` /
//! `App.version_flag`, not `App.options` (unlike CLI11, where they are real
//! `Option`s in `options_`), so they are synthesized into the `OPTIONS` group
//! here, always first.

/// True when ANSI color is safe to emit on stdout: not forced off by
/// --no-color or NO_COLOR, and stdout is actually a terminal.
pub fn color_output_enabled(no_color_flag: bool) -> bool {
    if no_color_flag || std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    crate::util::term::stdout_is_tty()
}

pub mod cli_color {
    /// The palette `--help` and other output (`wally about`, the default-model
    /// notice) style themselves with. Every field is "" when color is disabled,
    /// so a caller can always splice these in without an if/else.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct Palette {
        pub bold: &'static str,
        pub bold_cyan: &'static str,
        pub blue: &'static str,
        pub red: &'static str,
        pub green: &'static str,
        pub reset: &'static str,
    }

    pub fn make_palette(enabled: bool) -> Palette {
        if !enabled {
            return Palette::default();
        }
        Palette {
            bold: "\x1b[1m",
            bold_cyan: "\x1b[1;36m",
            blue: "\x1b[34m",
            red: "\x1b[1;31m",
            green: "\x1b[32m",
            reset: "\x1b[0m",
        }
    }
}

/// One line of an `Examples:` block: the command, and an optional note.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Example {
    pub command: String,
    pub note: String,
}

impl Example {
    pub fn new(command: &str, note: &str) -> Example {
        Example {
            command: command.to_string(),
            note: note.to_string(),
        }
    }
}

const EXAMPLE_NOTE_COLUMN: usize = 48;
const COLUMN_GAP: usize = 2;

/// The `Examples:` footer a command's help ends with. Commands sit at a
/// two-space indent and every note starts at the same column. No trailing newline.
pub fn examples_footer(rows: &[Example]) -> String {
    let mut out = String::from("Examples:");
    for row in rows {
        let mut line = format!("  {}", row.command);
        if !row.note.is_empty() {
            let pad = if line.len() + COLUMN_GAP <= EXAMPLE_NOTE_COLUMN {
                EXAMPLE_NOTE_COLUMN - line.len()
            } else {
                COLUMN_GAP
            };
            line.push_str(&" ".repeat(pad));
            line.push_str(&row.note);
        }
        out.push('\n');
        out.push_str(&line);
    }
    out
}

// ---------------------------------------------------------------------------
// CLI11's own Formatter internals (CLI11.hpp), reproduced verbatim.
// ---------------------------------------------------------------------------

/// `FormatterBase::column_width_`.
const COLUMN_WIDTH: usize = 30;
/// `FormatterBase::right_column_width_`.
const RIGHT_COLUMN_WIDTH: usize = 65;
// `FormatterBase::description_paragraph_width_` has no counterpart here:
// that width only matters in CLI11's own `Formatter::make_help`, which wraps
// `make_description(app)` through `streamOutAsParagraph` before using it.
// `CliFormatter::make_help` (cli_formatter.cpp) never calls the base
// `make_help` and splices `make_description(app)` straight in unwrapped, so
// a long one-line description (e.g. `telemetry emit`'s) is never reflowed —
// confirmed against `help__telemetry__emit.stdout`, whose description is one
// physical line well past 80 columns.

use crate::cli::{App, Opt, Validator, ValueType};

/// `CLI::detail::streamOutAsParagraph`: word-wraps `text` at `paragraph_width`
/// visible columns, indenting every wrapped line with `line_prefix`. Physical
/// `\n`s in `text` also start a fresh `line_prefix`'d line.
fn stream_out_as_paragraph(
    text: &str,
    paragraph_width: usize,
    line_prefix: &str,
    skip_prefix_on_first_line: bool,
) -> String {
    let mut out = String::new();
    if !skip_prefix_on_first_line {
        out.push_str(line_prefix);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        let mut chars_written: usize = 0;
        for word in line.split_whitespace() {
            if word.len() + chars_written > paragraph_width {
                out.push('\n');
                out.push_str(line_prefix);
                chars_written = 0;
            }
            out.push_str(word);
            out.push(' ');
            chars_written += word.len() + 1;
        }
        if i != last {
            out.push('\n');
            out.push_str(line_prefix);
        }
    }
    out
}

/// `CLI::detail::type_name<T>()`.
fn base_type_name(value_type: ValueType) -> &'static str {
    match value_type {
        ValueType::Text => "TEXT",
        ValueType::Int | ValueType::Int64 => "INT",
        ValueType::UInt | ValueType::UInt64 => "UINT",
        ValueType::Float | ValueType::Double => "FLOAT",
    }
}

/// A validator's `get_description()`, as CLI11's `Option::get_type_name()`
/// would concatenate it. `Range`/`RangeF` always render as lowercase text
/// with brackets and a space (`"INT in [1 - 5]"`), so `simplify_type_name`
/// below always discards it in favor of the base type name — the exact
/// wording is therefore never visible and is chosen only for plausibility.
fn validator_description(validator: &Validator) -> String {
    match validator {
        Validator::ExistingFile => "FILE".to_string(),
        Validator::PositiveNumber => "POSITIVE".to_string(),
        Validator::NonNegativeNumber => "NONNEGATIVE".to_string(),
        Validator::Range(min, max) => format!("in [{min} - {max}]"),
        Validator::RangeF(min, max) => format!("in [{min} - {max}]"),
        Validator::IsMember(values) => format!("{{{}}}", values.join(",")),
    }
}

/// `TEXT:FILE` -> `FILE`, `TEXT:{on,off}` -> `{on,off}`, `INT:INT in [1 - 5]`
/// -> `INT`: keep the validator suffix only when it is itself a type-shaped
/// token (a `{set}` or a single all-uppercase word); otherwise the validator
/// only narrows an already-named type, so fall back to the base type name.
fn simplify_type_name(type_name: &str) -> String {
    let Some(colon) = type_name.find(':') else {
        return type_name.to_string();
    };
    let suffix = &type_name[colon + 1..];
    if suffix.starts_with('{') {
        return suffix.to_string();
    }
    let one_word = !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_uppercase());
    if one_word {
        suffix.to_string()
    } else {
        type_name[..colon].to_string()
    }
}

/// `Option::get_type_name()` then `simplify_type_name`.
fn option_type_name(opt: &Opt) -> String {
    let base = opt
        .type_name
        .clone()
        .unwrap_or_else(|| base_type_name(opt.value_type).to_string());
    let mut full = base;
    for validator in &opt.validators {
        let desc = validator_description(validator);
        if !desc.is_empty() {
            full = format!("{full}:{desc}");
        }
    }
    simplify_type_name(&full)
}

/// Strips writes trailing whitespace from every line, collapses runs of blank
/// lines to one, drops leading blank lines and ends with exactly one newline.
fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut previous_blank = true; // suppresses blank lines at the top
    for raw_line in text.split('\n') {
        let line = raw_line.trim_end_matches([' ', '\t', '\r']);
        if line.is_empty() {
            if previous_blank {
                continue;
            }
            previous_blank = true;
        } else {
            previous_blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.len() >= 2
        && out.as_bytes()[out.len() - 1] == b'\n'
        && out.as_bytes()[out.len() - 2] == b'\n'
    {
        out.pop();
    }
    out
}

/// Left column padded to the description column, or the description dropped
/// to the next line when the left column is too wide to leave a gap. `left`
/// is the plain text the padding is measured from; `left_shown` is what is
/// printed, and may carry color codes.
fn stream_row(
    out: &mut String,
    left: &str,
    left_shown: &str,
    desc: &str,
    column_width: usize,
    right_width: usize,
) {
    out.push_str(left_shown);
    if desc.is_empty() {
        out.push('\n');
        return;
    }
    let mut skip_first_line_prefix = true;
    if left.len() + COLUMN_GAP <= column_width {
        out.push_str(&" ".repeat(column_width - left.len()));
    } else {
        out.push('\n');
        skip_first_line_prefix = false;
    }
    out.push_str(&stream_out_as_paragraph(
        desc,
        right_width,
        &" ".repeat(column_width),
        skip_first_line_prefix,
    ));
    out.push('\n');
}

fn colorize(text: &str, code: &str, reset: &str) -> String {
    if code.is_empty() || text.is_empty() {
        text.to_string()
    } else {
        format!("{code}{text}{reset}")
    }
}

/// A command that is listed as its children rather than as itself: a
/// namespace like `models` or `account`, where "models pull" is the thing you
/// type.
fn has_visible_children(app: &App) -> bool {
    app.subcommands
        .iter()
        .any(|child| !child.name.is_empty() && !child.group.is_empty())
}

// ---------------------------------------------------------------------------
// Sentence-case headings and label overrides `CliFormatter`'s constructor set
// (`label("POSITIONALS", "Arguments")`, `label("SUBCOMMAND", "COMMAND")`,
// `label("SUBCOMMANDS", "COMMANDS")`). "OPTIONS" and "REQUIRED" are never
// overridden, so they stay CLI11's own uppercase defaults.
// ---------------------------------------------------------------------------

/// Sentence-case headings for CLI11's upper-case defaults. Groups a command
/// registers itself pass through unchanged.
fn heading_for(group: &str) -> String {
    match group {
        "OPTIONS" => "Options".to_string(),
        "SUBCOMMANDS" => "Commands".to_string(),
        other => other.to_string(),
    }
}

/// CLI11 renders a `!--hide-thinking` flag as `--hide-thinking{false}`; the
/// brace suffix is noise to a reader. `Opt.names` is already stripped of this
/// by the parser, so this is mostly defensive.
fn strip_flag_default(name: &str) -> &str {
    match name.find('{') {
        Some(pos) => &name[..pos],
        None => name,
    }
}

/// `Opt.group` normalized: empty means the default "Options" section (per
/// `crate::cli::Opt::group`'s own doc comment), never "hidden" the way an
/// empty `CLI::Option` group would be in raw CLI11 — wally never actually
/// hides an individual option or positional, only subcommands.
fn group_key(opt: &Opt) -> &str {
    if opt.group.is_empty() {
        "OPTIONS"
    } else {
        &opt.group
    }
}

/// A synthetic `Opt` for the `-h, --help` / `-V, --version` rows, which live
/// in `App.help_flag` / `App.version_flag` rather than `App.options`.
fn synthetic_flag_opt(names_spec: &str, description: &str) -> Opt {
    Opt {
        names: names_spec
            .split(',')
            .map(str::trim)
            .map(strip_flag_default)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect(),
        is_flag: true,
        description: description.to_string(),
        ..Opt::default()
    }
}

// ---------------------------------------------------------------------------
// `Formatter::make_*` (CLI11.hpp) plus `CliFormatter::make_*` overrides
// (cli_formatter.cpp), in the same call order `make_help` uses.
// ---------------------------------------------------------------------------

/// `Formatter::make_description` (not overridden by wally) has a branch for
/// `App::get_required()` / `require_option_min/max()` ("[Exactly N of the
/// following options are required]"); wally never sets those on a command, so
/// it is not reproduced here — only the plain-description path is reachable.
fn make_description(app: &App) -> String {
    if app.description.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", app.description)
    }
}

/// `Formatter::make_option_usage`.
fn make_option_usage(opt: &Opt) -> String {
    let mut name = opt.positional.clone();
    if opt.multi {
        name.push_str("...");
    }
    if opt.required {
        name
    } else {
        format!("[{name}]")
    }
}

/// `CliFormatter::make_usage`: CLI11's `Formatter::make_usage` with its
/// leading newline stripped and "Usage: " prepended instead.
fn make_usage(app: &App, parents: &str) -> String {
    let mut usage = if parents.is_empty() {
        app.name.clone()
    } else {
        format!("{parents} {}", app.name)
    };
    // CLI11 only prints the `[OPTIONS]` badge when `app` has at least one
    // non-positional option; the always-inherited help flag (and, at root,
    // the version flag) makes that true for every command wally registers.
    usage.push_str(" [OPTIONS]");
    let positionals: Vec<&Opt> = app
        .options
        .iter()
        .filter(|o| !o.positional.is_empty())
        .collect();
    if !positionals.is_empty() {
        let names: Vec<String> = positionals.iter().map(|o| make_option_usage(o)).collect();
        usage.push(' ');
        usage.push_str(&names.join(" "));
    }
    // `Formatter::make_usage`: `[COMMAND]`/`COMMAND` for a single expected
    // subcommand, `[COMMANDS]`/`COMMANDS` for several; brackets iff optional
    // (`require_subcommand_min == 0`). `require_subcommand_max == 0` is
    // CLI11's "no explicit maximum", which reads as "< 2" here exactly like
    // the C++'s `get_require_subcommand_max() < 2` — not as "unlimited".
    let any_subcommand = app.subcommands.iter().any(|s| !s.name.is_empty());
    if any_subcommand {
        let bracketed = app.require_subcommand_min == 0;
        let singular = app.require_subcommand_max < 2 || app.require_subcommand_min > 1;
        let label = if singular { "COMMAND" } else { "COMMANDS" };
        usage.push(' ');
        if bracketed {
            usage.push('[');
        }
        usage.push_str(label);
        if bracketed {
            usage.push(']');
        }
    }
    format!("Usage: {usage}\n\n")
}

/// `CliFormatter::make_option`, positional and non-positional branches.
fn make_option_row(opt: &Opt, is_positional: bool, pal: &cli_color::Palette) -> String {
    let mut out = String::new();
    let (names, opts) = if is_positional {
        (format!("  {}", opt.positional), String::new())
    } else {
        let mut short_names: Vec<&str> = Vec::new();
        let mut long_names: Vec<&str> = Vec::new();
        for name in &opt.names {
            let stripped = strip_flag_default(name);
            if stripped.starts_with("--") {
                long_names.push(stripped);
            } else {
                short_names.push(stripped);
            }
        }
        let mut names = if short_names.is_empty() {
            "      ".to_string()
        } else {
            format!("  {}", short_names.join(", "))
        };
        if !short_names.is_empty() && !long_names.is_empty() {
            names.push_str(", ");
        }
        names.push_str(&long_names.join(", "));
        (names, make_option_opts(opt))
    };
    let left = format!("{names}{opts}");
    let left_shown = format!("{}{opts}", colorize(&names, pal.bold_cyan, pal.reset));
    stream_row(
        &mut out,
        &left,
        &left_shown,
        &opt.description,
        COLUMN_WIDTH,
        RIGHT_COLUMN_WIDTH,
    );
    out
}

/// `CliFormatter::make_option_opts`: type name, `...` for a vector, `REQUIRED`
/// — CLI11's own `Env:` / `Needs:` / `Excludes:` / `[default]` suffixes are
/// dropped, same as the C++.
fn make_option_opts(opt: &Opt) -> String {
    let mut out = String::new();
    if !opt.is_flag {
        let type_name = option_type_name(opt);
        if !type_name.is_empty() {
            out.push(' ');
            out.push_str(&type_name);
        }
        if opt.multi {
            out.push_str(" ...");
        }
        if opt.required {
            out.push_str(" REQUIRED");
        }
    }
    out
}

/// `Formatter::make_group` (not overridden for positionals) / the
/// `CliFormatter::make_group` override for a plain option group.
fn make_group(group: &str, is_positional: bool, opts: &[&Opt], pal: &cli_color::Palette) -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str(&colorize(&heading_for(group), pal.bold, pal.reset));
    out.push_str(":\n");
    for opt in opts {
        out.push_str(&make_option_row(opt, is_positional, pal));
    }
    out
}

/// `Formatter::make_positionals`.
fn make_positionals(app: &App, pal: &cli_color::Palette) -> String {
    let opts: Vec<&Opt> = app
        .options
        .iter()
        .filter(|o| !o.positional.is_empty())
        .collect();
    if opts.is_empty() {
        String::new()
    } else {
        // `Formatter::make_positionals` passes `get_label("POSITIONALS")`, not
        // the raw group key, to `make_group`; `CliFormatter`'s constructor set
        // that label to "Arguments" (`heading_for` below only maps the two
        // labels it *isn't* already substituted for: "OPTIONS"/"SUBCOMMANDS").
        make_group("Arguments", true, &opts, pal)
    }
}

/// `Formatter::make_groups`. `App::get_groups()` walks CLI11's real
/// `options_` (which holds the help/version flags as real `Option`s) in
/// registration order and always finds "OPTIONS" first, since the help flag
/// is always the first option registered; the synthetic rows below reproduce
/// that ordering without a real `options_` list to walk.
fn make_groups(app: &App, pal: &cli_color::Palette) -> String {
    let mut groups: Vec<String> = vec!["OPTIONS".to_string()];
    for opt in app.options.iter().filter(|o| o.positional.is_empty()) {
        let key = group_key(opt).to_string();
        if !groups.contains(&key) {
            groups.push(key);
        }
    }
    let mut out = String::new();
    for group in &groups {
        let mut synthetic: Vec<Opt> = Vec::new();
        if group == "OPTIONS" {
            if let Some((names, desc)) = &app.help_flag {
                synthetic.push(synthetic_flag_opt(names, desc));
            }
            if let Some((names, _version, desc)) = &app.version_flag {
                synthetic.push(synthetic_flag_opt(names, desc));
            }
        }
        let mut opts: Vec<&Opt> = synthetic.iter().collect();
        opts.extend(
            app.options
                .iter()
                .filter(|o| o.positional.is_empty() && group_key(o) == group.as_str()),
        );
        if !opts.is_empty() {
            out.push_str(&make_group(group, false, &opts, pal));
        }
    }
    out
}

/// `CliFormatter::make_subcommand_indented`.
fn make_subcommand_indented(sub: &App, indent: &str, pal: &cli_color::Palette) -> String {
    let mut out = String::new();
    let left = format!("{indent}{}", sub.name);
    stream_row(
        &mut out,
        &left,
        &colorize(&left, pal.bold_cyan, pal.reset),
        &sub.description,
        COLUMN_WIDTH,
        RIGHT_COLUMN_WIDTH,
    );
    out
}

/// `CliFormatter::make_subcommands`: groups in first-registration order
/// (case-insensitive dedup, matching `App::get_group()`/`to_lower` in the
/// C++), a namespace (`has_visible_children`) expands into its children at
/// `"  parent child"`, a leaf command prints as itself at `"  name"`.
fn make_subcommands(app: &App, pal: &cli_color::Palette) -> String {
    let mut groups_seen: Vec<String> = Vec::new();
    for sub in &app.subcommands {
        if sub.name.is_empty() || sub.group.is_empty() {
            continue;
        }
        if !groups_seen
            .iter()
            .any(|g| g.eq_ignore_ascii_case(&sub.group))
        {
            groups_seen.push(sub.group.clone());
        }
    }
    let mut out = String::new();
    for group in &groups_seen {
        out.push('\n');
        out.push_str(&colorize(&heading_for(group), pal.bold, pal.reset));
        out.push_str(":\n");
        for sub in &app.subcommands {
            if sub.name.is_empty() || !sub.group.eq_ignore_ascii_case(group) {
                continue;
            }
            if has_visible_children(sub) {
                for child in &sub.subcommands {
                    if child.name.is_empty() || child.group.is_empty() {
                        continue;
                    }
                    out.push_str(&make_subcommand_indented(
                        child,
                        &format!("  {} ", sub.name),
                        pal,
                    ));
                }
            } else {
                out.push_str(&make_subcommand_indented(sub, "  ", pal));
            }
        }
    }
    out
}

/// Render `app`'s help the way the C++ CliFormatter did. `parents` is the
/// command path above `app` ("wally models"), used in the Usage line; `app`'s
/// own name is appended to it here (matching `App::help(prev, mode)`, which
/// has already appended the current app's name to `prev` by the time it calls
/// the formatter).
pub fn make_help(app: &crate::cli::App, parents: &str, color_enabled: bool) -> String {
    let pal = cli_color::make_palette(color_enabled);
    let mut out = String::new();
    out.push_str(&make_description(app));
    out.push_str(&make_usage(app, parents));
    out.push_str(&make_positionals(app, &pal));
    let is_root = parents.is_empty();
    if is_root {
        // Root page: the commands are what someone came for; the global flags
        // are the same on every page and go last.
        out.push_str(&make_subcommands(app, &pal));
        out.push_str(&make_groups(app, &pal));
    } else {
        out.push_str(&make_groups(app, &pal));
        out.push_str(&make_subcommands(app, &pal));
    }
    if !app.footer.is_empty() {
        out.push('\n');
        out.push_str(&app.footer);
        out.push('\n');
    }
    tidy(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{App, Opt, Validator, ValueType};

    fn root_help_flag() -> Option<(String, String)> {
        Some(("-h,--help".to_string(), "Show help".to_string()))
    }

    fn opt_flag(names: &[&str], desc: &str, group: &str) -> Opt {
        Opt {
            names: names.iter().map(|s| s.to_string()).collect(),
            is_flag: true,
            description: desc.to_string(),
            group: group.to_string(),
            ..Opt::default()
        }
    }

    fn opt(names: &[&str], value_type: ValueType, desc: &str, group: &str) -> Opt {
        Opt {
            names: names.iter().map(|s| s.to_string()).collect(),
            value_type,
            description: desc.to_string(),
            group: group.to_string(),
            ..Opt::default()
        }
    }

    fn positional(name: &str, desc: &str, required: bool) -> Opt {
        Opt {
            positional: name.to_string(),
            value_type: ValueType::Text,
            description: desc.to_string(),
            required,
            ..Opt::default()
        }
    }

    fn read_golden(name: &str) -> String {
        let path = format!(
            "{}/tests/golden/expected/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    // Trailing newline: golden files are captured with a final `\n` (they are
    // ordinary text files); `make_help` (via `tidy`) also ends with exactly
    // one `\n`, so the comparison is exact-byte, no trimming either side.
    fn assert_matches_golden(actual: &str, golden_name: &str) {
        let expected = read_golden(golden_name);
        assert_eq!(actual, expected, "mismatch against {golden_name}");
    }

    #[test]
    fn heading_for_maps_cli11_defaults() {
        assert_eq!(heading_for("OPTIONS"), "Options");
        assert_eq!(heading_for("SUBCOMMANDS"), "Commands");
        assert_eq!(heading_for("Sampling"), "Sampling");
        assert_eq!(heading_for("Chat"), "Chat");
    }

    #[test]
    fn strip_flag_default_truncates_at_brace() {
        assert_eq!(
            strip_flag_default("--hide-thinking{false}"),
            "--hide-thinking"
        );
        assert_eq!(strip_flag_default("--model"), "--model");
    }

    #[test]
    fn simplify_type_name_keeps_braced_set() {
        assert_eq!(simplify_type_name("TEXT:{on,off}"), "{on,off}");
    }

    #[test]
    fn simplify_type_name_keeps_single_uppercase_word() {
        assert_eq!(simplify_type_name("INT:POSITIVE"), "POSITIVE");
        assert_eq!(simplify_type_name("TEXT:FILE"), "FILE");
    }

    #[test]
    fn simplify_type_name_falls_back_to_base_for_range() {
        // Range's generated description always has lowercase "in" and a
        // space, so it never survives simplify_type_name's one-word test.
        assert_eq!(simplify_type_name("INT:INT in [1 - 2147483647]"), "INT");
    }

    #[test]
    fn simplify_type_name_passthrough_without_validator() {
        assert_eq!(simplify_type_name("TEXT"), "TEXT");
    }

    #[test]
    fn tidy_collapses_blank_runs_and_trailing_newlines() {
        assert_eq!(tidy("a\n\n\n\nb\n\n\n"), "a\n\nb\n");
        assert_eq!(tidy("\n\na\n"), "a\n");
        assert_eq!(tidy("a   \nb\t\n"), "a\nb\n");
    }

    #[test]
    fn stream_out_as_paragraph_wraps_at_width() {
        // "three" (chars_written 4+3=7 <= 10) still fits after "one two ", but
        // "four" (6+4=10 <= 10) also fits after "three " -- CLI11 wraps only
        // once a word's length pushes charsWritten *past* paragraphWidth, so
        // "four" joins "three" on the wrapped line and only "five" wraps again.
        let wrapped = stream_out_as_paragraph("one two three four five", 10, "  ", true);
        assert_eq!(wrapped, "one two \n  three four \n  five ");
    }

    #[test]
    fn examples_footer_matches_two_rows_with_notes() {
        let rows = vec![
            Example::new("wally run qwen3-0.6b", "Chat interactively"),
            Example::new(
                "wally run qwen3-0.6b \"write a haiku\"",
                "Answer one prompt",
            ),
        ];
        let footer = examples_footer(&rows);
        assert_eq!(
            footer,
            "Examples:\n  wally run qwen3-0.6b                          Chat interactively\n  wally run qwen3-0.6b \"write a haiku\"          Answer one prompt"
        );
    }

    /// `run`: the terminal alias for `llm generate` in chat mode (port of
    /// `add_generation_options` + `register_llm_aliases` in cmd_run.cpp).
    fn build_run_app() -> App {
        let footer = examples_footer(&[
            Example::new("wally run qwen3-0.6b", "Chat interactively"),
            Example::new(
                "wally run qwen3-0.6b \"write a haiku\"",
                "Answer one prompt",
            ),
        ]);
        App {
            name: "run".to_string(),
            description: "Run a model".to_string(),
            footer,
            help_flag: root_help_flag(),
            options: vec![
                positional("model", "Model id, alias, hf.co/... ref or URL", true),
                positional(
                    "prompt",
                    "Prompt to answer (omit for an interactive chat)",
                    false,
                ),
                opt(
                    &["--system-prompt", "--system"],
                    ValueType::Text,
                    "System prompt",
                    "",
                ),
                opt(
                    &["--lora"],
                    ValueType::Text,
                    "LoRA adapter (.gguf) to attach",
                    "",
                ),
                opt(
                    &["--lora-scale"],
                    ValueType::Float,
                    "LoRA strength (default 1.0)",
                    "",
                ),
                opt(
                    &["--engine"],
                    ValueType::Text,
                    "Engine to run on (mlx, llamacpp, onnx, sherpa)",
                    "",
                ),
                {
                    let mut o = opt(
                        &["--image"],
                        ValueType::Text,
                        "Ask about this image instead (vision models)",
                        "",
                    );
                    o.validators.push(Validator::ExistingFile);
                    o
                },
                opt(
                    &["--temperature", "--temp"],
                    ValueType::Float,
                    "Sampling temperature (0 = engine default)",
                    "Sampling",
                ),
                opt(
                    &["--top-p"],
                    ValueType::Float,
                    "Keep the smallest token set above this probability",
                    "Sampling",
                ),
                opt(
                    &["--top-k"],
                    ValueType::Int,
                    "Sample from this many highest-probability tokens",
                    "Sampling",
                ),
                opt(
                    &["--min-p"],
                    ValueType::Float,
                    "Drop tokens below this share of the top token",
                    "Sampling",
                ),
                opt(
                    &["--repetition-penalty"],
                    ValueType::Float,
                    "Penalize tokens already in the context",
                    "Sampling",
                ),
                opt(
                    &["--seed"],
                    ValueType::Int,
                    "Fix the RNG for a repeatable answer",
                    "Sampling",
                ),
                opt(
                    &["--frequency-penalty"],
                    ValueType::Float,
                    "Penalize tokens by how often they appeared",
                    "Sampling",
                ),
                opt(
                    &["--presence-penalty"],
                    ValueType::Float,
                    "Penalize tokens that appeared at all",
                    "Sampling",
                ),
                {
                    let mut o = opt(
                        &["--stop"],
                        ValueType::Text,
                        "Stop at this text (repeat for several)",
                        "Sampling",
                    );
                    o.multi = true;
                    o
                },
                {
                    let mut o = opt(
                        &["--max-output-tokens", "--max-tokens"],
                        ValueType::Int,
                        "Cap on generated tokens (default 1024)",
                        "Sampling",
                    );
                    o.validators.push(Validator::Range(1, i32::MAX as i64));
                    o
                },
                {
                    let mut o = opt(
                        &["--reasoning"],
                        ValueType::Text,
                        "Model thinking phase (default on)",
                        "Reasoning",
                    );
                    o.validators.push(Validator::IsMember(vec![
                        "on".to_string(),
                        "off".to_string(),
                    ]));
                    o
                },
                opt_flag(
                    &["--show-thinking", "--hide-thinking"],
                    "Print thinking tokens on stderr (default on)",
                    "Reasoning",
                ),
                opt_flag(&["--no-think"], "Same as --reasoning off", "Reasoning"),
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_help_matches_golden_run() {
        let app = build_run_app();
        let rendered = make_help(&app, "wally", false);
        assert_matches_golden(&rendered, "help__run.stdout");
    }

    /// `serve` (port of `register_serve` in cmd_serve.cpp, as captured by the
    /// golden corpus).
    fn build_serve_app() -> App {
        let footer = examples_footer(&[
            Example::new("wally serve qwen3-0.6b", ""),
            Example::new("wally serve granite-4.2-8b --port 8000", ""),
        ]);
        App {
            name: "serve".to_string(),
            description: "Serve a model over an OpenAI-compatible API".to_string(),
            footer,
            help_flag: root_help_flag(),
            options: vec![
                positional("model", "Model to serve (default qwen3-0.6b)", false),
                opt(
                    &["-H", "--host"],
                    ValueType::Text,
                    "Address to bind (default 127.0.0.1)",
                    "",
                ),
                opt(
                    &["-p", "--port"],
                    ValueType::UInt,
                    "Port to listen on (default 8080)",
                    "",
                ),
                opt(
                    &["-c", "--context-length", "--context"],
                    ValueType::Int,
                    "Context window in tokens (default 8192)",
                    "",
                ),
                opt(
                    &["-t", "--threads"],
                    ValueType::Int,
                    "Inference threads (default 4)",
                    "",
                ),
                opt(
                    &["--gpu-layers", "--ngl"],
                    ValueType::Int,
                    "Layers to offload to the GPU",
                    "",
                ),
                opt_flag(
                    &["--cors"],
                    "Allow cross-origin browser requests (off by default)",
                    "",
                ),
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_help_matches_golden_serve() {
        let app = build_serve_app();
        let rendered = make_help(&app, "wally", false);
        assert_matches_golden(&rendered, "help__serve.stdout");
    }

    /// `telemetry emit` (port of the `emit` subcommand in cmd_telemetry.cpp).
    fn build_telemetry_emit_app() -> App {
        App {
            name: "emit".to_string(),
            description: "Track N events of one modality, flush to /api/v2/sdk/telemetry/{modality} \
                and report the backend's accounting. Production runs the auth handshake first; \
                development is keyless. Exits non-zero when any POST fails."
                .to_string(),
            help_flag: root_help_flag(),
            options: vec![
                {
                    let mut o = opt(&["--modality"], ValueType::Text, "Telemetry modality", "");
                    o.required = true;
                    o.validators.push(Validator::IsMember(
                        [
                            "llm", "stt", "tts", "vlm", "rag", "imagegen", "embeddings", "vad", "voice", "lora",
                            "model", "system",
                        ]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    ));
                    o
                },
                opt(
                    &["--event-type"],
                    ValueType::Text,
                    "Event type string (default: the modality's terminal event, e.g. llm.generation.completed)",
                    "",
                ),
                {
                    let mut o = opt(&["--count"], ValueType::Int, "Number of events to emit (default 1)", "");
                    o.validators.push(Validator::PositiveNumber);
                    o
                },
                opt(
                    &["--session-id"],
                    ValueType::Text,
                    "Session id attached to every event (default: fresh UUID)",
                    "",
                ),
                opt(&["--processing-ms"], ValueType::Float, "processing_time_ms metric for every event", ""),
                opt(&["--input-tokens"], ValueType::Int, "input_tokens metric (llm/vlm modalities)", ""),
                opt(&["--output-tokens"], ValueType::Int, "output_tokens metric (llm/vlm modalities)", ""),
                opt(&["--audio-duration-ms"], ValueType::Float, "audio_duration_ms metric (stt modality)", ""),
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_help_matches_golden_telemetry_emit() {
        let app = build_telemetry_emit_app();
        let rendered = make_help(&app, "wally telemetry", false);
        assert_matches_golden(&rendered, "help__telemetry__emit.stdout");
    }

    /// The root page (port of `app.cpp`'s registration): exercises group
    /// ordering, namespace expansion (`models pull`) alongside plain leaves
    /// (`run`), the synthesized `-h/-V` rows ahead of declared options, and
    /// the verbatim footer with its own embedded blank line.
    fn build_root_app() -> App {
        fn leaf(name: &str, description: &str, group: &str) -> App {
            App {
                name: name.to_string(),
                description: description.to_string(),
                group: group.to_string(),
                help_flag: root_help_flag(),
                ..App::default()
            }
        }
        fn namespace(name: &str, group: &str, children: Vec<(&str, &str)>) -> App {
            App {
                name: name.to_string(),
                group: group.to_string(),
                require_subcommand_min: 1,
                require_subcommand_max: 1,
                help_flag: root_help_flag(),
                subcommands: children
                    .into_iter()
                    .map(|(n, d)| App {
                        name: n.to_string(),
                        description: d.to_string(),
                        group: "SUBCOMMANDS".to_string(),
                        help_flag: root_help_flag(),
                        ..App::default()
                    })
                    .collect(),
                ..App::default()
            }
        }
        App {
            name: "wally".to_string(),
            description: "Run models on this machine or on your RunAnywhere account".to_string(),
            footer: "Get started:\n  wally models pull qwen3-0.6b && wally run qwen3-0.6b\n  \
                wally account login && wally opencode --cloud -m glm-5.3-flash\n\n\
                Run \"wally <command> --help\" for details."
                .to_string(),
            help_flag: root_help_flag(),
            version_flag: Some((
                "-V,--version".to_string(),
                "0.6.0".to_string(),
                "Show the wally version".to_string(),
            )),
            require_subcommand_min: 0,
            require_subcommand_max: 1,
            options: vec![
                opt_flag(&["--json"], "Print results as JSON", ""),
                opt_flag(&["-v", "--verbose"], "Debug logging on stderr", ""),
                opt_flag(&["-q", "--quiet"], "Errors only", ""),
                opt_flag(&["--no-progress"], "Disable progress rendering", ""),
                opt_flag(&["--no-color"], "Disable colored --help output", ""),
                opt(
                    &["--home"],
                    ValueType::Text,
                    "Where models live (default $RUNANYWHERE_HOME or ~/.local/share/runanywhere)",
                    "",
                ),
            ],
            subcommands: vec![
                leaf("run", "Run a model", "Chat"),
                leaf(
                    "serve",
                    "Serve a model over an OpenAI-compatible API",
                    "Chat",
                ),
                namespace(
                    "models",
                    "Models",
                    vec![
                        ("list", "List downloaded models (--all for the catalog)"),
                        ("show", "Show a model's details"),
                        ("pull", "Download a model"),
                        ("rm", "Delete a downloaded model"),
                        ("default", "Show or set the default model for coding tools"),
                    ],
                ),
                leaf(
                    "claude-code",
                    "Open Claude Code with a model",
                    "Coding tools",
                ),
                leaf(
                    "claude-desktop",
                    "Open Claude Desktop with a model",
                    "Coding tools",
                ),
                leaf("opencode", "Open opencode with a model", "Coding tools"),
                leaf("hermes", "Open Hermes with a model", "Coding tools"),
                leaf("openclaw", "Open OpenClaw with a model", "Coding tools"),
                leaf(
                    "deepseek",
                    "Open DeepSeek Harness with a model",
                    "Coding tools",
                ),
                namespace(
                    "account",
                    "Account",
                    vec![
                        ("login", "Sign in through the browser"),
                        ("logout", "Sign out and revoke the session"),
                        ("whoami", "Show the signed-in account"),
                        ("usage", "Show remaining credit and the last day's spend"),
                    ],
                ),
                leaf("about", "Versions, backends, paths and account", "Wally"),
                leaf("update", "Update wally to the latest release", "Wally"),
                leaf(
                    "uninstall",
                    "Remove wally, its models and its config",
                    "Wally",
                ),
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_help_matches_golden_root() {
        let app = build_root_app();
        let rendered = make_help(&app, "", false);
        assert_matches_golden(&rendered, "help__ROOT.stdout");
    }

    /// `claude-code` (a coding-tool harness command): exercises the vector
    /// positional usage line (`[args...]`).
    fn build_claude_code_app() -> App {
        let footer = examples_footer(&[Example::new("wally claude-code -m qwen3-0.6b", "")]);
        App {
            name: "claude-code".to_string(),
            description: "Open Claude Code with a model".to_string(),
            footer,
            help_flag: root_help_flag(),
            options: vec![
                opt(&["-m", "--model"], ValueType::Text, "Model to use", ""),
                opt_flag(
                    &["--serve"],
                    "Serve the model instead of connecting to a cloud endpoint",
                    "",
                ),
                {
                    let mut o = positional("args", "", false);
                    o.multi = true;
                    o
                },
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_usage_renders_vector_positional_with_ellipsis() {
        let app = build_claude_code_app();
        let usage = make_usage(&app, "wally");
        assert_eq!(usage, "Usage: wally claude-code [OPTIONS] [args...]\n\n");
    }

    /// `models` (a namespace leaf reached with no subcommand): exercises
    /// `require_subcommand(1, 1)` and hidden (`group("")`) children.
    fn build_models_app() -> App {
        App {
            name: "models".to_string(),
            description: "Manage local models".to_string(),
            help_flag: root_help_flag(),
            require_subcommand_min: 1,
            require_subcommand_max: 1,
            subcommands: vec![
                App {
                    name: "list".to_string(),
                    description: "List downloaded models (--all for the catalog)".to_string(),
                    group: "SUBCOMMANDS".to_string(),
                    ..App::default()
                },
                App {
                    name: "show".to_string(),
                    description: "Show a model's details".to_string(),
                    group: "SUBCOMMANDS".to_string(),
                    ..App::default()
                },
                App {
                    name: "pull".to_string(),
                    description: "Download a model".to_string(),
                    group: "SUBCOMMANDS".to_string(),
                    ..App::default()
                },
                App {
                    name: "rm".to_string(),
                    description: "Delete a downloaded model".to_string(),
                    group: "SUBCOMMANDS".to_string(),
                    ..App::default()
                },
                App {
                    name: "default".to_string(),
                    description: "Show or set the default model for coding tools".to_string(),
                    group: "SUBCOMMANDS".to_string(),
                    ..App::default()
                },
                // Hidden: reachable ("models load ...") but not listed.
                App {
                    name: "load".to_string(),
                    description: "Load a model into a running server".to_string(),
                    group: String::new(),
                    ..App::default()
                },
            ],
            ..App::default()
        }
    }

    #[test]
    fn make_usage_root_subcommand_is_singular_and_not_bracketed() {
        let app = build_models_app();
        let usage = make_usage(&app, "wally");
        assert_eq!(usage, "Usage: wally models [OPTIONS] COMMAND\n\n");
    }

    #[test]
    fn make_subcommands_hides_empty_group_children() {
        let app = build_models_app();
        let pal = cli_color::make_palette(false);
        let rendered = make_subcommands(&app, &pal);
        assert!(rendered.contains("  pull"));
        assert!(!rendered.contains("  load"));
    }
}
