//! Line editor with history (port of src/repl/repl.cpp: linenoise on
//! macOS/Linux, plain stdin on Windows as WALLY_NO_LINENOISE did). Owner: the
//! run/llm/tool/serve port.

#[cfg(not(windows))]
use rustyline::config::Configurer;
#[cfg(not(windows))]
use rustyline::error::ReadlineError;
#[cfg(not(windows))]
use rustyline::history::DefaultHistory;
#[cfg(not(windows))]
use rustyline::Editor;

#[cfg(not(windows))]
pub struct LineEditor {
    history_path: String,
    editor: Editor<(), DefaultHistory>,
}

#[cfg(windows)]
pub struct LineEditor {
    history_path: String,
}

/// MSVC's <filesystem>/<fstream> decode a narrow std::string through the ANSI
/// code page in the C++; Rust's std::path is always UTF-8-clean, so no
/// equivalent conversion is needed here.
fn create_history_dir(history_path: &str) {
    if let Some(parent) = std::path::Path::new(history_path).parent() {
        // Errors are swallowed, mirroring the C++ `std::error_code ec` that is
        // passed and never checked.
        let _ = std::fs::create_dir_all(parent);
    }
}

#[cfg(not(windows))]
impl LineEditor {
    /// `history_path` may be empty (no persistence, e.g. RUNANYWHERE_NOHISTORY).
    pub fn new(history_path: &str) -> Self {
        if !history_path.is_empty() {
            create_history_dir(history_path);
        }
        // Terminal setup only fails on a host with no working tty driver at
        // all, which linenoise could not run on either; there is no
        // meaningful fallback for an interactive REPL at that point.
        let mut editor: Editor<(), DefaultHistory> =
            Editor::new().expect("failed to initialize the line editor");
        // C++ only raises linenoise's history cap (from its compiled-in
        // default of 100) when persistence is enabled; with an empty
        // history_path (e.g. RUNANYWHERE_NOHISTORY) it leaves the default
        // untouched, so a no-history session can still only scroll back 100
        // lines. Gate this the same way instead of raising it unconditionally.
        if !history_path.is_empty() {
            let _ = editor.set_max_history_size(512);
            let _ = editor.load_history(history_path);
        }
        LineEditor {
            history_path: history_path.to_string(),
            editor,
        }
    }

    /// `None` on EOF (Ctrl-D). Empty lines are returned as empty strings.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        match self.editor.readline(prompt) {
            Ok(line) => Some(line),
            // EOF / Ctrl-D and Ctrl-C both return NULL from linenoise; mirror
            // that here so both keys end the REPL the same way.
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => None,
            Err(_) => None,
        }
    }

    /// Record a line in history (skips empties/duplicates of last entry).
    pub fn add_history(&mut self, line: &str) {
        if !line.is_empty() {
            let _ = self.editor.add_history_entry(line);
        }
    }
}

#[cfg(not(windows))]
impl Drop for LineEditor {
    fn drop(&mut self) {
        if !self.history_path.is_empty() {
            let _ = self.editor.save_history(&self.history_path);
        }
    }
}

#[cfg(windows)]
impl LineEditor {
    /// `history_path` may be empty (no persistence, e.g. RUNANYWHERE_NOHISTORY).
    pub fn new(history_path: &str) -> Self {
        if !history_path.is_empty() {
            create_history_dir(history_path);
        }
        LineEditor {
            history_path: history_path.to_string(),
        }
    }

    /// `None` on EOF (Ctrl-D). Empty lines are returned as empty strings.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => None,
            Ok(_) => {
                while line.ends_with('\n') || line.ends_with('\r') {
                    line.pop();
                }
                Some(line)
            }
            Err(_) => None,
        }
    }

    /// Record a line in history (skips empties/duplicates of last entry).
    pub fn add_history(&mut self, line: &str) {
        use std::io::Write;
        if !line.is_empty() && !self.history_path.is_empty() {
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.history_path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}
