//! `--help` layout for wally (port of src/cli_formatter.cpp): a `Usage:` line,
//! sentence-case section headings, one `-m, --model TEXT` column for options,
//! commands grouped by intent with namespaces shown as full paths
//! (`models pull`), and a verbatim `Examples:` footer. On a terminal, headings
//! are bold and anything typeable is cyan; anywhere else the text is plain and
//! byte-identical. Owner: the CLI port.

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

/// Render `app`'s help the way the C++ CliFormatter did. `parents` is the
/// command path above `app` ("wally models"), used in the Usage line.
pub fn make_help(app: &crate::cli::App, parents: &str, color_enabled: bool) -> String {
    let _ = (app.name.as_str(), parents, color_enabled);
    todo!("CLI port: CliFormatter::make_help")
}
