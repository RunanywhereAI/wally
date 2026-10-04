//! Line editor with history (port of src/repl/repl.cpp: linenoise on
//! macOS/Linux, plain stdin on Windows as WALLY_NO_LINENOISE did).

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

/// Set while a prompt read is in flight and Ctrl-C arrives: the read then
/// resolves to an empty line (the REPL reprompts) instead of the process
/// dying with exit 130, which is what Ctrl-C at the `»` prompt used to do.
#[cfg(windows)]
static PROMPT_INTERRUPTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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

    /// `None` on EOF (Ctrl-D). Ctrl-C cancels the line and reprompts, so it
    /// resolves to an empty line; empty lines are returned as empty strings.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        match self.editor.readline(prompt) {
            Ok(line) => Some(line),
            Err(ReadlineError::Eof) => None,
            Err(ReadlineError::Interrupted) => Some(String::new()),
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

    /// `None` on EOF (Ctrl-D). Ctrl-C cancels the line and reprompts: the
    /// guard below swallows it for the duration of the read (otherwise the
    /// process dies), and the flag turns it into an empty line. A read error
    /// with no interrupt is still EOF.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        use std::io::Write;
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let _prompt_guard = crate::util::interrupt::on_interrupt(|| {
            PROMPT_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        PROMPT_INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line);
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        resolve_windows_read(
            PROMPT_INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst),
            read,
            line,
        )
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

/// What a Windows prompt read resolves to. An interrupt (Ctrl-C) wins over
/// whatever the read did, even a half-typed line: it becomes the empty line
/// the REPL loop reprompts on. A clean EOF ends the session; anything else
/// is the typed line, and a bare read error exits like EOF before it.
#[cfg(windows)]
fn resolve_windows_read(
    interrupted: bool,
    read: std::io::Result<usize>,
    line: String,
) -> Option<String> {
    if interrupted {
        return Some(String::new());
    }
    match read {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::resolve_windows_read;

    #[test]
    fn interrupt_discards_even_a_half_typed_line() {
        assert_eq!(
            resolve_windows_read(true, Ok(4), "half".to_string()),
            Some(String::new())
        );
        assert_eq!(
            resolve_windows_read(true, Err(std::io::Error::other("gone")), String::new()),
            Some(String::new())
        );
    }

    #[test]
    fn clean_reads_keep_prior_meaning() {
        assert_eq!(
            resolve_windows_read(false, Ok(4), "hi".to_string()),
            Some("hi".to_string())
        );
        assert_eq!(resolve_windows_read(false, Ok(0), String::new()), None);
        assert_eq!(
            resolve_windows_read(false, Err(std::io::Error::other("gone")), String::new()),
            None
        );
    }
}
