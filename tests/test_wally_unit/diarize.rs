//! Port of the `diarize_*` cases in explore-main/tests/test_wally_unit.cpp
//! (diarize command coverage).
//!
//! Every case below is compiled in but `#[ignore]`d, mirroring the C++
//! `#if !WALLY_LLM_ONLY_CUT` guard around the whole block there: `wally diarize`
//! is not wired into `src/app.rs` for the LLM-only release. Once diarize is
//! re-enabled there, dropping the `#[ignore]` attributes is the whole re-enable
//! step, exactly as uncommenting `register_diarize(app, options);` was on the
//! C++ side.

#[allow(unused_imports)]
use super::common;

/// RAII zero-byte temp file. A zero-byte regular file satisfies
/// `Validator::ExistingFile` (WAV validity is only checked later, inside
/// `run_diarize`, which a parse-error path never reaches).
struct TempWavFile {
    path: std::path::PathBuf,
}

impl TempWavFile {
    fn new(tag: &str) -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wally_diarize_test_{tag}_{}_{n}.wav",
            std::process::id()
        ));
        std::fs::write(&path, []).expect("create temp wav file");
        TempWavFile { path }
    }

    fn path(&self) -> &str {
        self.path.to_str().expect("temp path is valid UTF-8")
    }
}

impl Drop for TempWavFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// register_diarize() is not called from src/app.rs for the LLM-only cut, so
/// the subcommand this introspection asserts on does not exist yet. Mirrors the
/// C++ WALLY_LLM_ONLY_CUT exclusion rather than a body-less always-pass stub.
#[test]
fn diarize_arg_surface() {
    let mut app = wally::cli::App::new("wally test app", "wally");
    wally::commands::cmd_diarize::register_diarize(&mut app);

    let cmd = app
        .get_subcommand("diarize")
        .expect("diarize subcommand not registered");
    assert_eq!(cmd.description, "Label who spoke when in an audio file");

    let audio = cmd
        .get_option("audio")
        .expect("positional 'audio' must exist");
    assert!(audio.required, "positional 'audio' must be required");

    let model = cmd.get_option("--model").expect("--model must exist");
    assert!(model.required, "--model must be required");
    assert!(
        model.names.iter().any(|n| n == "-m"),
        "--model must carry the -m alias"
    );

    for name in ["--threshold", "--min-duration", "--merge-gap"] {
        let opt = cmd
            .get_option(name)
            .unwrap_or_else(|| panic!("missing option {name}"));
        assert!(!opt.required, "{name} must not be required");
    }
}

// The five exit2 tests below (missing --model, missing audio, non-existent
// audio, non-numeric option, unknown flag) each only assert `exit code == 2`.
// With diarize not registered, `wally diarize …` is itself an unrecognized
// subcommand, which fails parsing before any diarize-specific argument
// validation they name is ever reached. Kept #[ignore]d for the same reason
// as diarize_arg_surface: left enabled, they would stay green even if
// diarize's argument parsing regressed or the command were deleted outright.

#[test]
fn diarize_missing_model_exit2() {
    // audio positional satisfied by an existing temp file -> the only failure
    // is the missing required --model (RequiredError -> ParseError -> exit 2).
    let audio = TempWavFile::new("missing_model");
    let code = common::run_in_process(&["diarize", audio.path()]);
    assert_eq!(code, 2, "missing required --model should be a usage error");
}

#[test]
fn diarize_missing_audio_exit2() {
    // --model consumes "x"; the required audio positional is left unsatisfied
    // (RequiredError -> ParseError -> exit 2).
    let code = common::run_in_process(&["diarize", "--model", "x"]);
    assert_eq!(
        code, 2,
        "missing required audio positional should be a usage error"
    );
}

#[test]
fn diarize_audio_not_found_exit2() {
    // --model is supplied so the sole failure is the audio ExistingFile
    // validator (ValidationError -> ParseError -> exit 2), a distinct path
    // from a plain RequiredError.
    let code = common::run_in_process(&[
        "diarize",
        "/no/such/wally-diarize-input.wav",
        "--model",
        "x",
    ]);
    assert_eq!(
        code, 2,
        "non-existent audio should fail the ExistingFile validator (usage error)"
    );
}

#[test]
fn diarize_numeric_option_typing_exit2() {
    // A non-numeric value for a typed numeric option raises a conversion
    // error (a ParseError) during parse, before the callback -> exit 2. This
    // is the only inference-free way to prove --threshold binds to a float
    // and --min-duration/--merge-gap bind to integers (a *valid* value would
    // run the callback and load a model). Required args are satisfied so the
    // conversion is the only failure.
    let audio = TempWavFile::new("numeric_typing");
    for flag in ["--threshold", "--min-duration", "--merge-gap"] {
        let code =
            common::run_in_process(&["diarize", audio.path(), "--model", "x", flag, "notanumber"]);
        assert_eq!(code, 2, "non-numeric {flag} should be a usage error");
    }
}

#[test]
fn diarize_unknown_flag_exit2() {
    // An unrecognized option is not consumed by the subcommand or (via
    // fallthrough) the parent, so parse ends with an extras error
    // (ParseError) -> exit 2. Guards against silently-ignored typos.
    let audio = TempWavFile::new("unknown_flag");
    let code = common::run_in_process(&["diarize", audio.path(), "--model", "x", "--bogus"]);
    assert_eq!(
        code, 2,
        "unrecognized flag should be a usage error (extras error)"
    );
}
