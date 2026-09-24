//! Line editor with history (port of src/repl/repl.cpp: linenoise on
//! macOS/Linux, plain stdin on Windows as WALLY_NO_LINENOISE did). Owner: the
//! run/llm/tool/serve port.

pub struct LineEditor {
    history_path: String,
}

impl LineEditor {
    /// `history_path` may be empty (no persistence, e.g. RUNANYWHERE_NOHISTORY).
    pub fn new(history_path: &str) -> Self {
        let _ = history_path;
        todo!("run port: LineEditor::new")
    }

    /// `None` on EOF (Ctrl-D). Empty lines are returned as empty strings.
    pub fn read_line(&mut self, prompt: &str) -> Option<String> {
        let _ = (prompt, &self.history_path);
        todo!("run port: LineEditor::read_line")
    }

    /// Record a line in history (skips empties/duplicates of last entry).
    pub fn add_history(&mut self, line: &str) {
        let _ = line;
        todo!("run port: LineEditor::add_history")
    }
}
