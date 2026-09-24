//! Non-secret per-profile CLI preferences (port of src/config/preferences.cpp).
//! Owner: the models port.
//!
//! Distinct from the credential store (`account`), which is the secret file at
//! mode 0600. The only key today is the default model. Resolution precedence,
//! highest first:
//!   1. an explicit model the reader passed
//!   2. the `WALLY_DEFAULT_MODEL` environment variable (a one-off shell override)
//!   3. `default_model` in the preferences file
//!   4. the id compiled into the binary (crate::WALLY_DEFAULT_MODEL_ID)

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::io::output;

const FILE_NAME: &str = "preferences.json";
const DEFAULT_MODEL_KEY: &str = "default_model";

/// {ProfileDirectory}/preferences.json, or empty when $HOME is unresolvable.
pub fn preferences_path() -> String {
    let directory = crate::account::credentials::profile_directory();
    if directory.is_empty() {
        return String::new();
    }
    Path::new(&directory)
        .join(FILE_NAME)
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DefaultModelSource {
    /// no default anywhere (only if no built-in id is compiled in)
    #[default]
    None,
    /// WALLY_DEFAULT_MODEL is set
    Environment,
    /// default_model in the preferences file
    File,
    /// the id compiled into the binary
    BuiltIn,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefaultModel {
    pub id: String,
    pub source: DefaultModelSource,
}

impl DefaultModel {
    pub fn set(&self) -> bool {
        self.source != DefaultModelSource::None
    }
}

/// Reads and parses the preferences file. Returns an empty object for a
/// missing file, and — on a parse error — an empty object plus a stderr
/// warning, so a corrupt file degrades to "no preferences" rather than
/// breaking a launch.
fn read_file() -> Value {
    let path = preferences_path();
    if path.is_empty() {
        return Value::Object(Default::default());
    }
    let contents = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Value::Object(Default::default()),
    };
    match serde_json::from_slice::<Value>(&contents) {
        Ok(parsed) if parsed.is_object() => parsed,
        _ => {
            output::status_line(&format!("ignoring unreadable preferences file at {path}"));
            Value::Object(Default::default())
        }
    }
}

/// Writes `body` to `temp`, distinguishing open failure from a
/// write/flush failure the same way C++'s preferences.cpp does: it opens the
/// temp file first and checks `!out.good()` right after open (`"cannot write
/// " + temp.string()`), then separately checks the stream again after
/// `out << ...; out.flush();` (`"failed writing " + temp.string()"` when the
/// open succeeded but the write/flush did not, e.g. disk full).
/// `std::fs::write()` folds open+write+flush into one call and would
/// collapse both onto the same message, so open and write/flush are kept as
/// distinct fallible steps here to preserve the two distinct texts.
fn write_temp_file(temp: &Path, body: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut file =
        std::fs::File::create(temp).map_err(|_| format!("cannot write {}", temp.display()))?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|_| format!("failed writing {}", temp.display()))
}

/// Writes `document` to the preferences file atomically: a sibling temp file
/// then a rename, so a reader never sees a half-written file.
fn write_file(document: &Value) -> Result<(), String> {
    let path = preferences_path();
    if path.is_empty() {
        return Err("cannot resolve a home directory for preferences".to_string());
    }

    let target = PathBuf::from(&path);
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;

    let temp = parent.join(format!("{FILE_NAME}.tmp"));
    let body = format!("{}\n", crate::io::json::dump_pretty(document, 2));
    write_temp_file(&temp, &body)?;

    std::fs::rename(&temp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot replace {}: {e}", target.display())
    })
}

/// The default model recorded in the file, ignoring the environment override.
pub fn file_default_model() -> Option<String> {
    let document = read_file();
    let value = document.get(DEFAULT_MODEL_KEY)?.as_str()?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

/// The default model in effect. A malformed file is treated as no default (a
/// warning goes to stderr); it never fails a launch.
pub fn effective_default_model() -> DefaultModel {
    let env = crate::util::getenv_or_empty("WALLY_DEFAULT_MODEL");
    if !env.is_empty() {
        return DefaultModel {
            id: env,
            source: DefaultModelSource::Environment,
        };
    }
    if let Some(file) = file_default_model() {
        return DefaultModel {
            id: file,
            source: DefaultModelSource::File,
        };
    }
    if !crate::WALLY_DEFAULT_MODEL_ID.is_empty() {
        return DefaultModel {
            id: crate::WALLY_DEFAULT_MODEL_ID.to_string(),
            source: DefaultModelSource::BuiltIn,
        };
    }
    DefaultModel::default()
}

/// `explicit_model` unchanged when non-empty, else the effective default, else "".
pub fn resolve_model(explicit_model: &str) -> String {
    if !explicit_model.is_empty() {
        return explicit_model.to_string();
    }
    effective_default_model().id
}

/// Persist `id` as the default model (rejects ids failing harness::model_id_is_safe).
pub fn set_default_model(id: &str) -> Result<(), String> {
    if !crate::harness::model_id_is_safe(id) {
        return Err(format!("not a usable model id: {id}"));
    }
    let mut document = read_file();
    if let Value::Object(map) = &mut document {
        map.insert(DEFAULT_MODEL_KEY.to_string(), Value::String(id.to_string()));
    }
    write_file(&document)
}

/// Remove the default model from the file. Succeeds when none was set.
pub fn clear_default_model() -> Result<(), String> {
    let mut document = read_file();
    let Value::Object(map) = &mut document else {
        return Ok(());
    };
    if !map.contains_key(DEFAULT_MODEL_KEY) {
        return Ok(());
    }
    map.remove(DEFAULT_MODEL_KEY);
    write_file(&document)
}

#[cfg(test)]
mod write_temp_file_tests {
    use super::write_temp_file;

    // id 30: an open failure (temp file cannot even be created) must produce
    // "cannot write {temp}", distinct from a write/flush failure.
    #[test]
    fn open_failure_uses_cannot_write_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Parent directory does not exist, so File::create fails to open.
        let temp = dir.path().join("missing-subdir").join("preferences.json.tmp");
        let error = write_temp_file(&temp, "{}\n").unwrap_err();
        assert_eq!(error, format!("cannot write {}", temp.display()));
    }

    #[test]
    fn successful_open_and_write_returns_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let temp = dir.path().join("preferences.json.tmp");
        write_temp_file(&temp, "{}\n").expect("write should succeed");
        assert_eq!(std::fs::read_to_string(&temp).unwrap(), "{}\n");
    }
}
