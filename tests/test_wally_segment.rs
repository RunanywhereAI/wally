//! wally `segment` command tests — port of tests/test_wally_segment.cpp. Owner:
//! the dormant vision/text modalities port.
//!
//! Covers exactly what is reachable for the `segment` command WITHOUT a
//! segmentation backend:
//!   - io/image_io.rs read_ppm(): the CLI-owned input gate for `segment`.
//!   - io/image_io.rs write_png(): the sibling encoder (smoke + guards).
//!   - The CLI parse/validation layer of `wally segment` (fires before the
//!     callback), asserting the documented 0/1/2 exit-code contract for usage
//!     errors via the app's exact error-to-exit-code mapping.
//!   - Structural wiring of register_segment (arg/flag spec) via introspection.
//!   - The documented --json output shape (JsonWriter sequence reproduced from
//!     cmd_segment.rs's print_result; see the note on that test).
//!
//! run_segment(), resolve_model_path() and print_result() are private to
//! cmd_segment.rs and unreachable from an integration test; anything that
//! needs to *execute* them requires a segmentation backend and belongs in a
//! separate backend-gated e2e test.
//!
//! `segment` is not registered in the shared app (LLM-only release, see
//! src/app.rs's configure_app): `segment_option_spec` below registers it on a
//! standalone App via cmd_segment::register_segment directly instead of going
//! through the shared app the way the C++ test's `#if !WALLY_LLM_ONLY_CUT`
//! guard needed to. `segment_usage_errors` still drives the real, shared app
//! in-process (common::run_in_process): every case here collapses to the same
//! exit code (2) whether or not `segment` is registered, since an unregistered
//! "segment" token is itself an extras/parse error under the shared app's
//! `require_subcommand(0, 1)` + no-fallthrough-match config — the same as
//! CLI11's ExtrasError/RequiredError/ValidationError all mapping to 2.

mod common;

use std::ffi::{CStr, CString};

use wally::cli::App;
use wally::io::image_io::{self, write_png};
use wally::io::output::JsonWriter;
use wally::sys::{rac_segmentation_class_summary_t, rac_segmentation_result_t};

// -----------------------------------------------------------------------------
// read_ppm(): the CLI's dependency-free image decoder and the `segment` input
// gate. Highest-value coverage available in the pure suite (public, pure,
// model-free).
// -----------------------------------------------------------------------------
#[test]
fn read_ppm() {
    let dir = tempfile::tempdir().expect("temp dir");

    // (a) Valid P6 round-trips width/height and the exact RGB bytes.
    {
        let pixels: [u8; 6] = [10, 20, 30, 40, 50, 60];
        let mut content = b"P6\n2 1\n255\n".to_vec();
        content.extend_from_slice(&pixels);
        let path = dir.path().join("valid.ppm");
        std::fs::write(&path, &content).expect("could not write valid PPM fixture");

        let img = image_io::read_ppm(path.to_str().expect("utf8 path")).expect("valid P6 was rejected");
        assert_eq!(img.width, 2, "expected width 2");
        assert_eq!(img.height, 1, "expected height 1");
        assert_eq!(img.rgb, pixels.to_vec(), "RGB payload mismatch");
    }

    // (g) '#'-comment lines in the header are skipped (next_ppm_uint comment
    // branch) and parsing still succeeds.
    {
        let pixels: [u8; 6] = [1, 2, 3, 4, 5, 6];
        let mut content =
            b"P6\n# wally comment before dims\n2 1\n# and a trailing one\n255\n".to_vec();
        content.extend_from_slice(&pixels);
        let path = dir.path().join("comment.ppm");
        std::fs::write(&path, &content).expect("could not write commented PPM fixture");

        let img =
            image_io::read_ppm(path.to_str().expect("utf8 path")).expect("commented P6 header was rejected");
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgb.len(), 6);
        assert_eq!(img.rgb[0], 1);
        assert_eq!(img.rgb[5], 6);
    }

    // (b)-(f) Malformed/format-violating inputs must fail with an actionable
    // error. Non-empty payloads keep the P3 case a real (if mis-tagged) pixmap.
    let three = vec![0x01u8; 3];
    let six = vec![0x01u8; 6];
    let five = vec![0x01u8; 5];
    let negatives: Vec<(Vec<u8>, &str, &str)> = vec![
        (
            [b"P3\n1 1\n255\n".as_slice(), &three].concat(),
            "not a binary PPM (P6)",
            "wrong magic P3",
        ),
        (Vec::new(), "not a binary PPM (P6)", "empty file"),
        (
            b"P6\n64\n".to_vec(),
            "malformed PPM header",
            "header ends before all three ints",
        ),
        (
            b"P6\nWxH\n255\n".to_vec(),
            "malformed PPM header",
            "non-numeric dimension",
        ),
        (
            [b"P6\n2 1\n254\n".as_slice(), &six].concat(),
            "unsupported PPM",
            "maxval != 255",
        ),
        (b"P6\n0 1\n255\n".to_vec(), "unsupported PPM", "zero width"),
        (b"P6\n1 0\n255\n".to_vec(), "unsupported PPM", "zero height"),
        (
            [b"P6\n4 4\n255\n".as_slice(), &five].concat(),
            "truncated PPM pixel data",
            "payload shorter than header promises",
        ),
    ];
    for (i, (content, want_substr, note)) in negatives.into_iter().enumerate() {
        let path = dir.path().join(format!("neg-{i}.ppm"));
        std::fs::write(&path, &content)
            .unwrap_or_else(|_| panic!("could not write negative fixture: {note}"));
        let err = image_io::read_ppm(path.to_str().expect("utf8 path"))
            .expect_err(&format!("expected failure but read_ppm succeeded: {note}"));
        assert!(
            err.contains(want_substr),
            "wrong error for {note}: expected substring \"{want_substr}\", got \"{err}\""
        );
    }

    // (h) Nonexistent path -> Err with "cannot open" (file never created).
    {
        let missing = dir.path().join("missing.ppm");
        let err = image_io::read_ppm(missing.to_str().expect("utf8 path"))
            .expect_err("read_ppm succeeded on a nonexistent path");
        assert!(
            err.contains("cannot open"),
            "expected error containing \"cannot open\", got \"{err}\""
        );
    }
}

// -----------------------------------------------------------------------------
// write_png(): public sibling encoder in io/image_io.rs. Smoke + input guards.
// -----------------------------------------------------------------------------
#[test]
fn write_png_smoke() {
    let dir = tempfile::tempdir().expect("temp dir");

    let width = 2;
    let height = 2;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    for (i, byte) in rgba.iter_mut().enumerate() {
        *byte = ((i * 7 + 3) % 256) as u8;
    }

    let path = dir.path().join("out.png");
    write_png(path.to_str().expect("utf8 path"), &rgba, width, height)
        .expect("write_png failed on a valid RGBA buffer");

    let written = std::fs::read(&path).expect("written PNG could not be reopened");
    assert!(
        written.len() >= 8,
        "PNG shorter than its 8-byte signature"
    );
    let signature: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];
    assert_eq!(&written[..8], &signature, "PNG signature mismatch");

    // width <= 0 and empty data are rejected before any file is opened. (Rust
    // has no null pointer for a safe slice; an empty slice mirrors the C++
    // null-data guard, per image_io.rs's own doc comment.)
    let throwaway = dir.path().join("bad.png");
    let err_dim = write_png(throwaway.to_str().expect("utf8 path"), &rgba, 0, height)
        .expect_err("width<=0 must fail");
    assert!(
        err_dim.contains("invalid image dimensions or data"),
        "got: {err_dim}"
    );
    let err_empty = write_png(throwaway.to_str().expect("utf8 path"), &[], width, height)
        .expect_err("empty data must fail");
    assert!(
        err_empty.contains("invalid image dimensions or data"),
        "got: {err_empty}"
    );
}

// -----------------------------------------------------------------------------
// Exit-code contract for `segment` usage errors.
//
// Drives the real, shared app in-process (common::run_in_process, the Rust
// counterpart of the C++ test's app.parse() + exact catch ladder) and asserts
// the documented 0/1/2 exit-code contract for CLI usage errors.
// -----------------------------------------------------------------------------
#[test]
fn segment_usage_errors() {
    let dir = tempfile::tempdir().expect("temp dir");

    let mut ppm_content = b"P6\n1 1\n255\n".to_vec();
    ppm_content.extend_from_slice(&[9, 9, 9]);
    let existing = dir.path().join("wally-seg-usage.ppm");
    std::fs::write(&existing, &ppm_content).expect("could not write usage-test PPM fixture");
    let existing = existing.to_str().expect("utf8 path").to_string();

    let missing = dir
        .path()
        .join("wally-seg-usage-missing.ppm")
        .to_str()
        .expect("utf8 path")
        .to_string();

    struct Case<'a> {
        args: Vec<&'a str>,
        expected: i32,
        note: &'a str,
    }
    let cases = [
        Case {
            args: vec!["segment"],
            expected: 2,
            note: "image + --model both missing (RequiredError -> 2)",
        },
        Case {
            args: vec!["segment", "--model", "seg-model"],
            expected: 2,
            note: "image missing (RequiredError -> 2)",
        },
        Case {
            args: vec!["segment", existing.as_str()],
            expected: 2,
            note: "--model missing (RequiredError -> 2)",
        },
        Case {
            args: vec!["segment", missing.as_str(), "--model", "seg-model"],
            expected: 2,
            note: "image not on disk (ExistingFile -> ValidationError -> 2)",
        },
        Case {
            args: vec![
                "segment",
                existing.as_str(),
                "--model",
                "seg-model",
                "--bogus",
            ],
            expected: 2,
            note: "unknown flag (ExtrasError -> 2)",
        },
    ];
    for case in &cases {
        let code = common::run_in_process(&case.args);
        assert_eq!(code, case.expected, "wrong exit code for: {}", case.note);
    }
}

// -----------------------------------------------------------------------------
// Structural wiring of register_segment — positive spec assertion with ZERO
// callback execution (the only way to verify the happy-path option spec
// without a segmentation model). `segment` is not registered in the shared
// app (LLM-only release), so this registers it directly on a standalone App
// rather than going through the app's full configure_app the way the C++
// test's #if !WALLY_LLM_ONLY_CUT-guarded version did.
// -----------------------------------------------------------------------------
#[test]
fn segment_option_spec() {
    let mut app = App::new("RunAnywhere on-device AI CLI — run, manage and serve local models", "wally");
    wally::commands::register_segment(&mut app);

    let seg = app
        .get_subcommand("segment")
        .expect("segment subcommand not registered by register_segment");
    assert_eq!(
        seg.description, "Label every pixel of an image by class",
        "wrong description"
    );

    let image = seg
        .get_option("image")
        .expect("positional 'image' option missing");
    assert!(image.required, "'image' positional must be required");

    let model_long = seg.get_option("--model").expect("'--model' option missing");
    assert!(model_long.required, "'--model' must be required");

    let model_short = seg.get_option("-m").expect("'-m' option missing");
    assert!(
        std::ptr::eq(model_long, model_short),
        "'-m' must resolve to the same Option as '--model'"
    );
}

// -----------------------------------------------------------------------------
// --json output-shape guard (mirrors json_writer_shape in test_wally_unit).
//
// print_result() is private to cmd_segment.rs and cannot be called here, so
// this reproduces the EXACT JsonWriter sequence it emits for the --json
// branch and asserts the serialized string. It is fed real
// rac_segmentation_result_t / rac_segmentation_class_summary_t structs so the
// test stays coupled to the actual field names/types.
//
// NOTE: this LOCKS the documented --json contract (one flat JSON document) but
// does NOT execute print_result(). Genuine coverage of print_result /
// --no-progress / the human table + "(no classes)" + verbose "(N ms)"
// rendering requires driving run_segment(), which needs a SEGMENT backend and
// belongs in a separate backend-gated e2e test.
// -----------------------------------------------------------------------------
#[test]
fn segment_json_shape() {
    let model_id = CString::new("segformer-b0-ade20k").expect("no NUL");
    let label_background = CString::new("background").expect("no NUL");
    let label_person = CString::new("person").expect("no NUL");

    let mut classes = [
        rac_segmentation_class_summary_t {
            class_id: 0,
            pixel_count: 200_000,
            fraction: 0.75, // exactly representable -> emits "0.75"
            label: label_background.as_ptr() as *mut _,
        },
        rac_segmentation_class_summary_t {
            class_id: 15,
            pixel_count: 67_000,
            fraction: 0.25, // exactly representable -> emits "0.25"
            label: label_person.as_ptr() as *mut _,
        },
    ];

    // SAFETY: plain-data C struct; every field is set explicitly below except
    // the pointer/count pairs left at their zeroed defaults where unused.
    let mut result: rac_segmentation_result_t = unsafe { std::mem::zeroed() };
    result.width = 640;
    result.height = 480;
    result.class_summaries = classes.as_mut_ptr();
    result.class_summary_count = classes.len();
    result.processing_time_ms = 12;
    result.model_id = model_id.as_ptr() as *mut _;
    // Not freed: every pointer above is stack/CString-owned, not malloc-owned.

    let model_ref = "unused-fallback-ref"; // model_id is non-null

    // SAFETY: result.model_id points at `model_id`, and each class's `label`
    // points at its own CString; all three outlive this block.
    let model_field = if result.model_id.is_null() {
        model_ref.to_string()
    } else {
        unsafe { CStr::from_ptr(result.model_id) }
            .to_string_lossy()
            .into_owned()
    };
    // SAFETY: class_summaries points at class_summary_count valid entries
    // (the `classes` array above), which outlives this block.
    let classes_slice: &[rac_segmentation_class_summary_t] =
        unsafe { std::slice::from_raw_parts(result.class_summaries, result.class_summary_count) };

    // Reproduced verbatim from cmd_segment.rs's print_result() --json branch.
    let mut json = JsonWriter::new();
    json.begin_object();
    json.field_str("model", &model_field)
        .field_i64("width", i64::from(result.width))
        .field_i64("height", i64::from(result.height))
        .field_i64("class_count", classes_slice.len() as i64)
        .field_i64("processing_time_ms", result.processing_time_ms);
    json.begin_array("classes");
    for cls in classes_slice {
        // SAFETY: cls.label points at one of the CStrings above.
        let label = if cls.label.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(cls.label) }
                .to_string_lossy()
                .into_owned()
        };
        json.begin_array_object();
        json.field_i64("class_id", i64::from(cls.class_id))
            .field_str("label", &label)
            .field_i64("pixel_count", cls.pixel_count as i64)
            .field_f64("fraction", f64::from(cls.fraction));
        json.end_object();
    }
    json.end_array();
    json.end_object();

    let expected = concat!(
        r#"{"model":"segformer-b0-ade20k","width":640,"height":480,"class_count":2,"#,
        r#""processing_time_ms":12,"classes":["#,
        r#"{"class_id":0,"label":"background","pixel_count":200000,"fraction":0.75},"#,
        r#"{"class_id":15,"label":"person","pixel_count":67000,"fraction":0.25}]}"#
    );
    assert_eq!(json.str(), expected);
}
