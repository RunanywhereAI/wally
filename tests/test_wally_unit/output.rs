//! test_wally_unit.cpp: JSON escaping, JsonWriter shape and non-finite numbers,
//! human-readable sizes.

use wally::io::output::JsonWriter;

#[test]
fn json_escape() {
    let cases = [
        ("plain", "plain"),
        ("quote\"backslash\\", "quote\\\"backslash\\\\"),
        ("line\nbreak\ttab", "line\\nbreak\\ttab"),
        ("ctl\x01", "ctl\\u0001"),
    ];
    for (input, expected) in cases {
        assert_eq!(wally::io::output::json_escape(input), expected);
    }
}

#[test]
fn json_writer_shape() {
    let mut json = JsonWriter::new();
    json.begin_object()
        .field_str("name", "qwen3-0.6b")
        .field_i64("size", 640)
        .field_bool("downloaded", true);
    json.begin_array("files");
    json.begin_array_object()
        .field_str("path", "a.gguf")
        .end_object();
    json.begin_array_object()
        .field_str("path", "b.gguf")
        .end_object();
    json.end_array();
    json.begin_array("scores")
        .value_f64(1.0)
        .value_f64(0.5)
        .end_array();
    json.end_object();
    assert_eq!(
        json.str(),
        concat!(
            r#"{"name":"qwen3-0.6b","size":640,"downloaded":true,"#,
            r#""files":[{"path":"a.gguf"},{"path":"b.gguf"}],"#,
            r#""scores":[1,0.5]}"#
        )
    );
}

#[test]
fn json_writer_nan_is_null() {
    // A NaN/Inf double (e.g. sherpa's unset STT confidence) has no JSON literal;
    // %g used to print it verbatim as the bareword `nan`.
    let mut json = JsonWriter::new();
    json.begin_object()
        .field_f64("confidence", f64::NAN)
        .field_f64("gain", f64::INFINITY);
    json.begin_array("scores")
        .value_f64(f64::NAN)
        .value_f64(0.5)
        .end_array();
    json.end_object();
    assert_eq!(
        json.str(),
        r#"{"confidence":null,"gain":null,"scores":[null,0.5]}"#
    );
}

#[test]
fn human_bytes() {
    let cases = [
        (512u64, "512 B"),
        (2048, "2.0 KB"),
        (640 * 1024 * 1024, "640.0 MB"),
        (3 * 1024 * 1024 * 1024, "3.0 GB"),
    ];
    for (input, expected) in cases {
        assert_eq!(wally::io::output::human_bytes(input), expected);
    }
}

#[test]
fn format_g_matches_printf() {
    // The C++ used printf("%g"); these are its outputs.
    let cases = [
        (1.0, "1"),
        (0.5, "0.5"),
        (123456.0, "123456"),
        (1234567.0, "1.23457e+06"),
        (0.0001, "0.0001"),
        (0.00001, "1e-05"),
        #[allow(clippy::approx_constant)]
        (3.14159265, "3.14159"),
        (-2.5, "-2.5"),
        (100.0, "100"),
        (0.1 + 0.2, "0.3"),
    ];
    for (value, expected) in cases {
        assert_eq!(wally::io::output::format_g(value), expected, "{value}");
    }
}
