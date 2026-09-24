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

/// {ProfileDirectory}/preferences.json, or empty when $HOME is unresolvable.
pub fn preferences_path() -> String {
    todo!("models port: PreferencesPath")
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

/// The default model in effect. A malformed file is treated as no default (a
/// warning goes to stderr); it never fails a launch.
pub fn effective_default_model() -> DefaultModel {
    todo!("models port: EffectiveDefaultModel")
}

/// The default model recorded in the file, ignoring the environment override.
pub fn file_default_model() -> Option<String> {
    todo!("models port: FileDefaultModel")
}

/// `explicit_model` unchanged when non-empty, else the effective default, else "".
pub fn resolve_model(explicit_model: &str) -> String {
    todo!("models port: ResolveModel ({explicit_model})")
}

/// Persist `id` as the default model (rejects ids failing harness::model_id_is_safe).
pub fn set_default_model(id: &str) -> Result<(), String> {
    todo!("models port: SetDefaultModel ({id})")
}

/// Remove the default model from the file. Succeeds when none was set.
pub fn clear_default_model() -> Result<(), String> {
    todo!("models port: ClearDefaultModel")
}
