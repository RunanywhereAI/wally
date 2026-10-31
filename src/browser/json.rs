//! JSON that keeps the order it was built in.
//!
//! `serde_json::Value` sorts object keys in this crate (no `preserve_order`,
//! which other output depends on). The decision model reads option order: the
//! same Google Flights request with its keys sorted kept every choice but moved
//! the click target's top score from 0.71 to 0.47. So request bodies are built
//! as `Json` and serialized in insertion order.
//!
//! `to_python` writes what Python's `json.dumps(value)` writes (`", "` and
//! `": "` separators, ASCII-only with `\uXXXX` escapes, Python's float repr).
//! The page fingerprint and the text model's prompt are defined in those bytes.

use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::{Number, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn object() -> Self {
        Json::Object(Vec::new())
    }

    /// Adds a member, replacing an existing one in place, as a Python dict
    /// assignment does.
    pub fn set(&mut self, key: &str, value: impl Into<Json>) {
        if let Json::Object(members) = self {
            let value = value.into();
            match members.iter_mut().find(|(name, _)| name == key) {
                Some(member) => member.1 = value,
                None => members.push((key.to_string(), value)),
            }
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Json>) -> Self {
        self.set(key, value);
        self
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(name, _)| name == key).map(|m| &m.1),
            _ => None,
        }
    }

    /// The value with objects in key order, as `json.dumps(..., sort_keys=True)` sees it.
    pub fn sorted(value: &Value) -> Json {
        match value {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Number(n) => Json::Number(n.clone()),
            Value::String(s) => Json::String(s.clone()),
            Value::Array(items) => Json::Array(items.iter().map(Json::sorted).collect()),
            Value::Object(map) => {
                let mut members: Vec<(String, Json)> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Json::sorted(v)))
                    .collect();
                members.sort_by(|a, b| a.0.cmp(&b.0));
                Json::Object(members)
            }
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Json::Null => Value::Null,
            Json::Bool(b) => Value::Bool(*b),
            Json::Number(n) => Value::Number(n.clone()),
            Json::String(s) => Value::String(s.clone()),
            Json::Array(items) => Value::Array(items.iter().map(Json::to_value).collect()),
            Json::Object(members) => Value::Object(
                members
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_value()))
                    .collect(),
            ),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a Json tree always serializes")
    }

    pub fn to_python(&self) -> String {
        let mut out = String::new();
        write_python(self, &mut out);
        out
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::String(value.to_string())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::String(value)
    }
}

impl From<&String> for Json {
    fn from(value: &String) -> Self {
        Json::String(value.clone())
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Self {
        Json::Number(value.into())
    }
}

impl From<Vec<Json>> for Json {
    fn from(value: Vec<Json>) -> Self {
        Json::Array(value)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(value: Option<T>) -> Self {
        value.map_or(Json::Null, Into::into)
    }
}

impl From<&Value> for Json {
    /// Keeps `serde_json`'s own (sorted) member order.
    fn from(value: &Value) -> Self {
        Json::sorted(value)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Json::Null => serializer.serialize_unit(),
            Json::Bool(b) => serializer.serialize_bool(*b),
            Json::Number(n) => n.serialize(serializer),
            Json::String(s) => serializer.serialize_str(s),
            Json::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(members) => {
                let mut map = serializer.serialize_map(Some(members.len()))?;
                for (key, value) in members {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

fn write_python(value: &Json, out: &mut String) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Number(n) => match (n.as_u64(), n.as_i64(), n.as_f64()) {
            (Some(u), _, _) => out.push_str(&u.to_string()),
            (_, Some(i), _) => out.push_str(&i.to_string()),
            (_, _, Some(f)) => out.push_str(&python_float(f)),
            _ => out.push_str("null"),
        },
        Json::String(s) => write_python_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python(item, out);
            }
            out.push(']');
        }
        Json::Object(members) => {
            out.push('{');
            for (i, (key, item)) in members.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_python_string(key, out);
                out.push_str(": ");
                write_python(item, out);
            }
            out.push('}');
        }
    }
}

fn write_python_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python's `repr(float)`: the shortest digits that round-trip, positional
/// from 1e-4 up to 1e16 and scientific (`1e-05`, `1.5e+16`) outside it.
fn python_float(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    let scientific = format!("{f:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("{:e} always has an exponent");
    let exponent: i32 = exponent.parse().expect("{:e} exponent is an integer");
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            format!("{whole}.{fraction}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let fraction = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let exp_sign = if exponent < 0 { '-' } else { '+' };
        format!("{sign}{first}{fraction}e{exp_sign}{:02}", exponent.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floats_print_like_python_repr() {
        let cases = [
            (12.5, "12.5"),
            (0.1, "0.1"),
            (100.0, "100.0"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (123456789012345.6, "123456789012345.6"),
            (-2.25, "-2.25"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float(value), expected, "{value}");
        }
    }

    #[test]
    fn strings_escape_like_python_ensure_ascii() {
        let json = Json::from("₹ \"a\"\n\u{1F600}\u{7f}");
        assert_eq!(
            json.to_python(),
            "\"\\u20b9 \\\"a\\\"\\n\\ud83d\\ude00\\u007f\""
        );
    }

    #[test]
    fn objects_keep_insertion_order_and_python_separators() {
        let json = Json::object()
            .with("b", 1u64)
            .with("a", vec![Json::Null, true.into()]);
        assert_eq!(json.to_python(), r#"{"b": 1, "a": [null, true]}"#);
        assert_eq!(
            String::from_utf8(json.to_bytes()).unwrap(),
            r#"{"b":1,"a":[null,true]}"#
        );
    }

    #[test]
    fn setting_an_existing_key_keeps_its_position() {
        let mut json = Json::object().with("value", "x").with("index", "1");
        json.set("value", "y");
        assert_eq!(json.to_python(), r#"{"value": "y", "index": "1"}"#);
    }
}
