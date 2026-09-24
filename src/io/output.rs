//! Output discipline helpers + minimal JSON emission.
//!
//! Contract (see AGENTS.md):
//!   - command RESULTS go to stdout;
//!   - logs, progress, banners, prompts go to stderr;
//!   - --json mode prints exactly ONE JSON document on stdout.

use std::io::Write;
use std::sync::OnceLock;

use crate::sys;

/// JSON-escape a UTF-8 string (quotes not included).
pub fn json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04x}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Format a double the way C's `printf("%g", value)` does (6 significant
/// digits, `%e` style below 1e-4 or from 1e6, trailing zeros removed).
pub fn format_g(value: f64) -> String {
    const P: i32 = 6;
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0".into()
        } else {
            "0".into()
        };
    }
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan".into()
        } else {
            "nan".into()
        };
    }
    if value.is_infinite() {
        return if value < 0.0 {
            "-inf".into()
        } else {
            "inf".into()
        };
    }
    // The exponent after rounding to P significant digits decides the style.
    let sci = format!("{:.*e}", (P - 1) as usize, value);
    let (mantissa, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let x: i32 = exp.parse().expect("{:e} exponent is an integer");
    if x < -4 || x >= P {
        let mantissa = strip_fraction_zeros(mantissa);
        let sign = if x < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", x.abs())
    } else {
        let decimals = (P - 1 - x).max(0) as usize;
        strip_fraction_zeros(&format!("{:.*}", decimals, value)).to_string()
    }
}

fn strip_fraction_zeros(text: &str) -> &str {
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.')
}

/// Minimal JSON document builder — enough for wally's flat objects/arrays
/// without pulling a JSON dependency into command output.
#[derive(Debug, Default, Clone)]
pub struct JsonWriter {
    buffer: String,
    first_in_scope: Vec<bool>,
}

impl JsonWriter {
    pub fn new() -> Self {
        Self::default()
    }

    fn comma(&mut self) {
        if let Some(first) = self.first_in_scope.last_mut() {
            if !*first {
                self.buffer.push(',');
            }
            *first = false;
        }
    }

    fn key(&mut self, key: &str) {
        self.buffer.push('"');
        self.buffer.push_str(&json_escape(key));
        self.buffer.push_str("\":");
    }

    pub fn begin_object(&mut self) -> &mut Self {
        self.comma();
        self.buffer.push('{');
        self.first_in_scope.push(true);
        self
    }

    pub fn end_object(&mut self) -> &mut Self {
        self.buffer.push('}');
        self.first_in_scope.pop();
        self
    }

    /// Opens an array; with a non-empty `key` it is an object member.
    pub fn begin_array(&mut self, key: &str) -> &mut Self {
        self.comma();
        if !key.is_empty() {
            self.key(key);
        }
        self.buffer.push('[');
        self.first_in_scope.push(true);
        self
    }

    pub fn end_array(&mut self) -> &mut Self {
        self.buffer.push(']');
        self.first_in_scope.pop();
        self
    }

    /// Object element inside an array.
    pub fn begin_array_object(&mut self) -> &mut Self {
        self.begin_object()
    }

    /// Opens an object as a member of the current object (`"key":{`).
    /// C++ spelled this `begin_array("")`-style by hand; kept as one call.
    pub fn begin_object_field(&mut self, key: &str) -> &mut Self {
        self.comma();
        self.key(key);
        self.buffer.push('{');
        self.first_in_scope.push(true);
        self
    }

    pub fn field_str(&mut self, key: &str, value: &str) -> &mut Self {
        self.comma();
        self.key(key);
        self.buffer.push('"');
        self.buffer.push_str(&json_escape(value));
        self.buffer.push('"');
        self
    }

    pub fn field_i64(&mut self, key: &str, value: i64) -> &mut Self {
        self.comma();
        self.key(key);
        self.buffer.push_str(&value.to_string());
        self
    }

    /// NaN and Infinity have no JSON literal; an engine that doesn't compute a
    /// metric (e.g. sherpa's STT confidence) hands back a raw NaN, and %g would
    /// print it verbatim as the bareword `nan` — invalid JSON on every call.
    pub fn field_f64(&mut self, key: &str, value: f64) -> &mut Self {
        self.comma();
        self.key(key);
        self.push_f64(value);
        self
    }

    pub fn field_bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.comma();
        self.key(key);
        self.buffer.push_str(if value { "true" } else { "false" });
        self
    }

    pub fn value_str(&mut self, value: &str) -> &mut Self {
        self.comma();
        self.buffer.push('"');
        self.buffer.push_str(&json_escape(value));
        self.buffer.push('"');
        self
    }

    pub fn value_i64(&mut self, value: i64) -> &mut Self {
        self.comma();
        self.buffer.push_str(&value.to_string());
        self
    }

    pub fn value_f64(&mut self, value: f64) -> &mut Self {
        self.comma();
        self.push_f64(value);
        self
    }

    pub fn value_bool(&mut self, value: bool) -> &mut Self {
        self.comma();
        self.buffer.push_str(if value { "true" } else { "false" });
        self
    }

    fn push_f64(&mut self, value: f64) {
        if value.is_finite() {
            self.buffer.push_str(&format_g(value));
        } else {
            self.buffer.push_str("null");
        }
    }

    pub fn str(&self) -> &str {
        &self.buffer
    }
}

/// Print a result line to stdout (newline appended) and flush.
pub fn result_line(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Print a status/notice line to stderr (newline appended).
pub fn status_line(line: &str) {
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

/// Print an error to stderr as "Error: <message>" (a red "Error:" on a TTY).
pub fn error_line(message: &str) {
    // One error shape everywhere: a red "Error:" then the message. Colour is
    // dropped under NO_COLOR or when stderr is not a terminal (pipes, CI logs).
    static COLOR: OnceLock<bool> = OnceLock::new();
    let color = *COLOR.get_or_init(|| {
        if crate::util::getenv("NO_COLOR").is_some() {
            return false;
        }
        crate::util::term::stderr_is_tty()
    });
    let mut err = std::io::stderr().lock();
    if color {
        let _ = writeln!(err, "\x1b[1;31mError:\x1b[0m {message}");
    } else {
        let _ = writeln!(err, "Error: {message}");
    }
}

/// Human message for a rac_result_t (falls back to the numeric code).
pub fn describe_result(result: sys::rac_result_t) -> String {
    // SAFETY: the kit returns a static string (or null) for any code.
    let message = unsafe { sys::rac_error_message(result) };
    if !message.is_null() {
        // SAFETY: non-null, NUL-terminated static string.
        let text = unsafe { std::ffi::CStr::from_ptr(message) }.to_string_lossy();
        if !text.is_empty() {
            return format!("{text} ({result})");
        }
    }
    format!("rac error {result}")
}

/// "1.4 GB" / "532 MB" style size formatting.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < 4 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Simple left-aligned column table rendered to stdout. Widths are byte
/// lengths, as in the C++ (`std::string::size`).
pub fn table(header: &[String], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(String::len).collect();
    for row in rows {
        for (c, cell) in row.iter().enumerate().take(widths.len()) {
            widths[c] = widths[c].max(cell.len());
        }
    }
    let print_row = |row: &[String]| {
        let mut line = String::new();
        for (c, width) in widths.iter().enumerate() {
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            line.push_str(cell);
            if c + 1 < widths.len() {
                line.push_str(&" ".repeat(width - cell.len() + 4));
            }
        }
        result_line(&line);
    };
    print_row(header);
    for row in rows {
        print_row(row);
    }
}
