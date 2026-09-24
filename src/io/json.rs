//! JSON text exactly as nlohmann::json 3.11.3 `dump()` wrote it.
//!
//! The C++ wrote every JSON file (preferences, credentials, the model cache,
//! opencode/openclaw/deepseek configs, the Claude Desktop profile) and every
//! shim payload with `nlohmann::json::dump()`. serde_json agrees on everything
//! that matters for byte parity — sorted object keys (nlohmann's default object
//! is a `std::map`; serde_json's default map is a `BTreeMap`), escaping, the
//! pretty layout — except floating-point numbers: nlohmann prints `1.5e-06`
//! and `2.0` where serde_json prints `1.5e-6` and `2.0`/`2`. This module keeps
//! serde_json's serializer and swaps in nlohmann's float format.
//!
//! Use `dump`/`dump_pretty` wherever the C++ called `.dump()`/`.dump(n)`.
//! (Never enable serde_json's `preserve_order` feature: keys would stop being
//! sorted. `object_keys_are_sorted` below catches it.)

use serde::Serialize;
use serde_json::ser::{CompactFormatter, Formatter, PrettyFormatter};
use serde_json::Value;

/// `json.dump()` — compact, no spaces.
pub fn dump(value: &Value) -> String {
    serialize(value, NlohmannFormatter(CompactFormatter))
}

/// `json.dump(indent)` — `indent` spaces per level, `": "` after keys.
pub fn dump_pretty(value: &Value, indent: usize) -> String {
    let indent = " ".repeat(indent);
    serialize(
        value,
        NlohmannFormatter(PrettyFormatter::with_indent(indent.as_bytes())),
    )
}

fn serialize<F: Formatter>(value: &Value, formatter: F) -> String {
    let mut out = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    value
        .serialize(&mut serializer)
        .expect("serializing a serde_json::Value cannot fail");
    String::from_utf8(out).expect("serde_json writes UTF-8")
}

/// Delegates everything to the inner formatter except floats.
struct NlohmannFormatter<F>(F);

macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*)),* $(,)?) => {
        $(fn $name<W: ?Sized + std::io::Write>(&mut self, writer: &mut W $(, $arg: $ty)*) -> std::io::Result<()> {
            self.0.$name(writer $(, $arg)*)
        })*
    };
}

impl<F: Formatter> Formatter for NlohmannFormatter<F> {
    delegate!(
        write_null(), write_bool(value: bool), write_i8(value: i8), write_i16(value: i16),
        write_i32(value: i32), write_i64(value: i64), write_i128(value: i128), write_u8(value: u8),
        write_u16(value: u16), write_u32(value: u32), write_u64(value: u64), write_u128(value: u128),
        write_number_str(value: &str), begin_string(), end_string(),
        write_string_fragment(fragment: &str),
        write_char_escape(char_escape: serde_json::ser::CharEscape),
        write_byte_array(value: &[u8]),
        begin_array(), end_array(), begin_array_value(first: bool), end_array_value(),
        begin_object(), end_object(), begin_object_key(first: bool), end_object_key(),
        begin_object_value(), end_object_value(), write_raw_fragment(fragment: &str),
    );

    fn write_f32<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f32,
    ) -> std::io::Result<()> {
        writer.write_all(format_float(f64::from(value)).as_bytes())
    }

    fn write_f64<W: ?Sized + std::io::Write>(
        &mut self,
        writer: &mut W,
        value: f64,
    ) -> std::io::Result<()> {
        writer.write_all(format_float(value).as_bytes())
    }
}

/// nlohmann::detail::to_chars + format_buffer (min_exp -4, max_exp 15), with
/// its `null` for non-finite values (serializer::dump_float).
pub fn format_float(value: f64) -> String {
    if !value.is_finite() {
        return "null".into();
    }
    let mut out = String::new();
    let mut value = value;
    if value.is_sign_negative() {
        out.push('-');
        value = -value;
    }
    if value == 0.0 {
        out.push_str("0.0");
        return out;
    }
    // Shortest round-trip digits and the decimal exponent of the first digit.
    let sci = format!("{value:e}");
    let (mantissa, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let first_exponent: i32 = exp.parse().expect("{:e} exponent is an integer");
    let k = digits.len() as i32;
    // nlohmann's `n`: position of the decimal point relative to the digits.
    let n = first_exponent + 1;
    const MIN_EXP: i32 = -4;
    const MAX_EXP: i32 = 15;
    if k <= n && n <= MAX_EXP {
        // digits, zeros, ".0"
        out.push_str(&digits);
        out.push_str(&"0".repeat((n - k) as usize));
        out.push_str(".0");
    } else if 0 < n && n <= MAX_EXP {
        // dig.its
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if MIN_EXP < n && n <= 0 {
        // 0.[000]digits
        out.push_str("0.");
        out.push_str(&"0".repeat((-n) as usize));
        out.push_str(&digits);
    } else {
        // d[.igits]e+XX — at least two exponent digits
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = n - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_match_nlohmann() {
        let cases = [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (2.5, "2.5"),
            (1.5e-6, "1.5e-06"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123456789012345.0, "123456789012345.0"),
            (1234567890123456.0, "1.234567890123456e+15"),
            (1e21, "1e+21"),
            (3.14159, "3.14159"),
            (-42.75, "-42.75"),
            (f64::NAN, "null"),
            // Straight from nlohmann 3.11.3 (the same header the C++ build fetched).
            (0.1 + 0.2, "0.30000000000000004"),
            (1e-300, "1e-300"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (1e15, "1e+15"),
            (1e16, "1e+16"),
            (0.3, "0.3"),
            (2.0 / 3.0, "0.6666666666666666"),
            (1234.5678, "1234.5678"),
            (9.99e-5, "9.99e-05"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_float(value), expected, "{value}");
        }
    }

    #[test]
    fn strings_and_layout_match_nlohmann() {
        // nlohmann 3.11.3: {{"b",1},{"a",{1,2}},{"e",{}},{"c",[]},{"s","q\"\\\n\t\x01\x7f/é"},{"f",1.5}}
        let v = json!({"b": 1, "a": [1, 2], "e": {}, "c": [], "s": "q\"\\\n\t\u{1}\u{7f}/é", "f": 1.5});
        assert_eq!(dump(&v), "{\"a\":[1,2],\"b\":1,\"c\":[],\"e\":{},\"f\":1.5,\"s\":\"q\\\"\\\\\\n\\t\\u0001\u{7f}/é\"}");
    }

    #[test]
    fn object_keys_are_sorted() {
        // Fails if some dependency turned on serde_json/preserve_order.
        let v: Value = serde_json::from_str(r#"{"b":1,"a":2}"#).unwrap();
        assert_eq!(dump(&v), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn pretty_layout_matches_nlohmann() {
        let v = json!({"a": [1, 2], "b": {}, "c": [], "d": 1.5});
        assert_eq!(
            dump_pretty(&v, 2),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": [],\n  \"d\": 1.5\n}"
        );
    }
}
