//! test_wally_unit.cpp cases owned by the run/llm/tool/serve port.
//!
//! - [x] engine_hint_parsing
//! - [x] run_max_tokens_zero_exit2 (blocked at runtime: see the test body)
//! - [x] run_max_tokens_negative_exit2 (blocked at runtime: see the test body)

#[allow(unused_imports)]
use super::common;

use wally::commands::engine_options::{engine_choices, parse_engine_hint};
use wally::io::proto::v1;

#[test]
fn engine_hint_parsing() {
    let mut cases: Vec<(&str, v1::InferenceFramework)> = vec![
        ("", v1::InferenceFramework::Unspecified),
        ("mlx", v1::InferenceFramework::Mlx),
        ("llama.cpp", v1::InferenceFramework::LlamaCpp),
        ("llama-cpp", v1::InferenceFramework::LlamaCpp),
        ("onnx", v1::InferenceFramework::Onnx),
        ("sherpa", v1::InferenceFramework::Sherpa),
        ("qhexrt", v1::InferenceFramework::Qhexrt),
        ("npu", v1::InferenceFramework::Qhexrt),
    ];
    // The Apple Neural Engine names parse only when the kit linked NeuRT; the
    // refusal on every other build is asserted below.
    if cfg!(wally_has_neurt) {
        cases.push(("neurt", v1::InferenceFramework::Coreml));
        cases.push(("ane", v1::InferenceFramework::Coreml));
    }
    for (input, expected) in &cases {
        let actual = parse_engine_hint(input);
        assert_eq!(
            actual,
            Ok(*expected),
            "input: {input} (got {actual:?}, want Ok({expected:?}))"
        );
    }

    let error = parse_engine_hint("banana").expect_err("unsupported engine should be an error");
    assert!(
        error.contains("unsupported engine"),
        "unsupported engine should fail with an actionable error, got: {error}"
    );

    // `--engine ane` on a build with no NeuRT used to be accepted and then
    // fall through to MLX, which failed on a Core ML tree with a misleading
    // "config.json not found". It must be refused here, naming the build.
    #[cfg(not(wally_has_neurt))]
    {
        for name in ["ane", "neurt", "coreml"] {
            let error =
                parse_engine_hint(name).expect_err("must be refused in a kit without NeuRT");
            assert!(
                error.contains("not in this build"),
                "--engine {name} must be refused in a kit without NeuRT; got: {error}"
            );
        }
        // And the help text must not advertise it either.
        assert!(
            !engine_choices().contains("ane"),
            "engine_choices() lists ane in a kit without NeuRT"
        );
    }
}

// Both tests below drive the real `wally` binary end to end (same shape as
// the C++ run_wally() harness): `--max-tokens 0` / `-5` must fail CLI11's own
// Range(1, INT32_MAX) validator as a usage error (exit 2) before any model
// resolution, bootstrap, or network access happens, so neither test needs a
// real model or network.
//
// BLOCKED as of this port: both currently fail because
// App::add_option/add_subcommand (the CLI11-parser port) and
// bootstrap::bootstrap (the bootstrap/device-info/progress port) are still
// todo!() in their owning files, so the spawned binary panics instead of
// returning exit code 2. Listed in the final report; not something this
// file's owner can fix (rule 4).

#[test]
fn run_max_tokens_zero_exit2() {
    let home = common::TempHome::new();
    let (code, _stdout, stderr) =
        common::run_wally(&home, &["run", "qwen3-0.6b", "hi", "--max-tokens", "0"]);
    assert_eq!(
        code, 2,
        "`run --max-tokens 0` should be a usage error, not an uncapped run (stderr: {stderr})"
    );
}

#[test]
fn run_max_tokens_negative_exit2() {
    let home = common::TempHome::new();
    let (code, _stdout, stderr) =
        common::run_wally(&home, &["run", "qwen3-0.6b", "hi", "--max-tokens", "-5"]);
    assert_eq!(
        code, 2,
        "`run --max-tokens -5` should be a usage error (stderr: {stderr})"
    );
}
