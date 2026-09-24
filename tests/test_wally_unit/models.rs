//! test_wally_unit.cpp cases owned by the models port.

use wally::cli::App;
use wally::commands::model_labels;
use wally::config::preferences::{self, DefaultModelSource};
use wally::io::proto::v1;

use super::common::{env_lock, EnvGuard};

/// C++ test_models_ls_is_primary_name: `ls` is only ever an alias — the
/// primary registered name (what `--help` and `get_display_name()` show) is
/// `list`, and `models ls` resolves to the very same subcommand as `models
/// list`.
#[test]
fn models_ls_is_primary_name() {
    let mut app = App::new("wally test app", "");
    wally::app::configure_app(&mut app);

    assert!(app.get_subcommand("ls").is_none());

    let models = app
        .get_subcommand("models")
        .expect("models subcommand registered");
    let list = models
        .get_subcommand("list")
        .expect("list subcommand registered");
    assert_eq!(list.name, "list");

    let via_alias = models.get_subcommand("ls").expect("ls resolves to list");
    assert_eq!(via_alias.name, list.name);
}

/// C++ test_model_labels_format.
#[test]
fn model_labels_format() {
    assert_eq!(model_labels::format(v1::ModelFormat::Gguf), "GGUF");
    assert_eq!(model_labels::format(v1::ModelFormat::Coreml), "Core ML");
    assert_eq!(
        model_labels::format(v1::ModelFormat::Safetensors),
        "SafeTensors"
    );
    assert_eq!(model_labels::format(v1::ModelFormat::Unspecified), "?");
}

/// C++ TempProfileDir: an isolated WALLY_PROFILE_DIR for one test, removed on
/// drop. tempfile::TempDir already does both halves.
fn temp_profile_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp profile dir")
}

/// C++ test_default_model_resolution.
#[test]
fn default_model_resolution() {
    let _lock = env_lock();
    let dir = temp_profile_dir();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", dir.path().to_string_lossy().as_ref());
    env.unset("RCLI_PROFILE_DIR");

    {
        let mut env = EnvGuard::new();
        env.unset("WALLY_DEFAULT_MODEL");

        // An explicit model passes straight through.
        assert_eq!(preferences::resolve_model("qwen3-0.6b"), "qwen3-0.6b");

        // Nothing set anywhere: falls back to the built-in default.
        assert_eq!(
            preferences::resolve_model(""),
            wally::WALLY_DEFAULT_MODEL_ID
        );
        assert_eq!(
            preferences::effective_default_model().source,
            DefaultModelSource::BuiltIn
        );

        preferences::set_default_model("glm-5.3-flash").expect("set default model");

        // The file default fills in for an omitted model, never overrides one
        // that was actually passed.
        assert_eq!(preferences::resolve_model(""), "glm-5.3-flash");
        assert_eq!(preferences::resolve_model("qwen3-0.6b"), "qwen3-0.6b");
    }
    {
        let mut env = EnvGuard::new();
        env.set("WALLY_DEFAULT_MODEL", "env-model");

        // The environment override outranks the file.
        let effective = preferences::effective_default_model();
        assert_eq!(effective.id, "env-model");
        assert_eq!(effective.source, DefaultModelSource::Environment);
        assert_eq!(preferences::resolve_model(""), "env-model");
    }
    {
        let mut env = EnvGuard::new();
        env.unset("WALLY_DEFAULT_MODEL");

        // The file default returns once the environment override is gone.
        assert_eq!(
            preferences::effective_default_model().source,
            DefaultModelSource::File
        );
    }
}

/// C++ test_default_model_store.
#[test]
fn default_model_store() {
    let _lock = env_lock();
    let dir = temp_profile_dir();
    let mut env = EnvGuard::new();
    env.set("WALLY_PROFILE_DIR", dir.path().to_string_lossy().as_ref());
    env.unset("RCLI_PROFILE_DIR");
    env.unset("WALLY_DEFAULT_MODEL");

    // Set then clear round-trips, and clearing an already-empty default is fine.
    preferences::set_default_model("glm-5.3-flash").expect("set default model");
    assert_eq!(
        preferences::file_default_model().as_deref(),
        Some("glm-5.3-flash")
    );
    preferences::clear_default_model().expect("clear default model");
    assert!(preferences::file_default_model().is_none());
    preferences::clear_default_model().expect("clear an already-empty default");

    // An unsafe or empty id is rejected, and nothing is written.
    assert!(preferences::set_default_model("bad<id>").is_err());
    assert!(preferences::set_default_model("").is_err());
    assert!(preferences::file_default_model().is_none());

    // A corrupt preferences file degrades gracefully, never a panic.
    std::fs::write(preferences::preferences_path(), "{ this is not json")
        .expect("write corrupt preferences file");
    assert!(preferences::file_default_model().is_none());
    let corrupt_effective = preferences::effective_default_model();
    assert_eq!(corrupt_effective.source, DefaultModelSource::BuiltIn);
    assert_eq!(corrupt_effective.id, wally::WALLY_DEFAULT_MODEL_ID);
}
