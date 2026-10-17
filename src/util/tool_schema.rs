//! Tool-schema cleanup shared by the Anthropic and Responses translators.

use serde_json::Value;

/// `schema` with every null-valued object field removed, at any depth.
///
/// MCP servers commonly declare optional parameters as `"default": null`. The
/// MLX engine renders tools through a Jinja chat template that cannot take a
/// JSON null and fails the whole request, so Claude Code with such a tool
/// could not reach an MLX model. A null field says nothing a schema reader
/// needs, so dropping it changes no tool's meaning.
pub fn without_null_fields(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), without_null_fields(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(without_null_fields).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::without_null_fields;
    use serde_json::json;

    #[test]
    fn null_defaults_are_dropped_at_any_depth() {
        let schema = json!({
            "type": "object",
            "properties": {
                "image": {"anyOf": [{"type": "string"}, {"type": "null"}], "default": null},
                "nested": {"type": "object", "properties": {"x": {"default": null, "type": "integer"}}}
            },
            "required": []
        });
        assert_eq!(
            without_null_fields(&schema),
            json!({
                "type": "object",
                "properties": {
                    "image": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                    "nested": {"type": "object", "properties": {"x": {"type": "integer"}}}
                },
                "required": []
            })
        );
    }

    #[test]
    fn a_schema_without_nulls_is_unchanged() {
        let schema =
            json!({"type": "object", "properties": {"p": {"type": "string", "default": "a"}}});
        assert_eq!(without_null_fields(&schema), schema);
    }

    #[test]
    fn nulls_inside_arrays_of_objects_are_dropped_but_array_nulls_stay() {
        assert_eq!(
            without_null_fields(&json!({"enum": [null, "a"], "items": [{"default": null}]})),
            json!({"enum": [null, "a"], "items": [{}]})
        );
    }
}
