#!/usr/bin/env python3
"""Generate the typed Rust binding for the CLI's control-plane HTTP calls.

The source of truth is the pinned OpenAPI extract `wally-cli-v1.openapi.json`
(itself carved from InferenceInfra's `control-plane-v1.openapi.json`). This
reads that artifact and emits `src/account/console_contract.rs`: a Rust enum
per string enum and a Rust struct per object schema, each with a hand-rolled
`to_json`/`from_json` pair (over `serde_json::Value`, not `serde`'s derive
macros, so the tolerant-response / strict-request asymmetry below is explicit
rather than fought with attributes) so `console.rs` never hand-builds a
request body or parses a response field by name. A `CONTRACT_SHA256` constant
pins the exact artifact the file was built from; `test_wally_contract` fails
the build if the two drift.

    python3 contracts/generate_console_binding.py            # write the file
    python3 contracts/generate_console_binding.py --check    # fail if stale

Run it and commit the file whenever the pinned contract changes. Never edit
console_contract.rs by hand.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CONTRACT = ROOT / "wally-cli-v1.openapi.json"
OUTPUT = ROOT.parent / "src" / "account" / "console_contract.rs"

INT = "i64"

# Rust 2021 reserved/keyword identifiers among this contract's field names.
# Escaped as a raw identifier (`r#type`) everywhere the field is named; the
# wire key ("type") is untouched.
RUST_KEYWORDS = {"type"}


def _snake_to_pascal(name: str) -> str:
    return "".join(part.capitalize() for part in name.replace("-", "_").split("_"))


def _enum_variant(value: str) -> str:
    # "24h" -> K24h, "claude_code" -> KClaudeCode. A digit-leading value is
    # still a legal identifier once prefixed with K.
    return "K" + _snake_to_pascal(value)


def _field_ident(prop: str) -> str:
    return f"r#{prop}" if prop in RUST_KEYWORDS else prop


def _resolve_type(schema: dict, schemas: dict) -> tuple[str, bool]:
    """Return (rust type, is_optional). Nullable/anyOf-null collapses to optional."""
    if "$ref" in schema:
        name = schema["$ref"].split("/")[-1]
        target = schemas.get(name, {})
        # A constrained/plain string newtype (type "string", not an enum, not an
        # object) has no emitted type of its own -- only objects and enums get
        # one -- so inline it as String rather than name an undefined type.
        if target.get("type") == "string" and "enum" not in target:
            return "String", False
        return name, False
    if "const" in schema:
        # A fixed literal (e.g. `object: {const: "model"}`). Typed by its value;
        # the reader still parses it, it just can only be that one value.
        const = schema["const"]
        if isinstance(const, bool):
            return "bool", False
        if isinstance(const, int):
            return INT, False
        return "String", False
    if "anyOf" in schema:
        branches = [b for b in schema["anyOf"] if b.get("type") != "null"]
        had_null = any(b.get("type") == "null" for b in schema["anyOf"])
        inner, _ = _resolve_type(branches[0], schemas)
        return inner, had_null
    kind = schema.get("type")
    # JSON Schema nullable form `type: ["integer", "null"]`: strip the null,
    # resolve the remaining type, and mark it optional. Same meaning as an
    # anyOf-with-null, just spelled the compact way OpenAPI 3.1 emits.
    if isinstance(kind, list):
        non_null = [t for t in kind if t != "null"]
        had_null = "null" in kind
        inner, _ = _resolve_type({**schema, "type": non_null[0]}, schemas)
        return inner, had_null
    if kind == "string":
        return "String", False
    if kind == "integer":
        return INT, False
    if kind == "boolean":
        return "bool", False
    if kind == "array":
        item, _ = _resolve_type(schema["items"], schemas)
        return f"Vec<{item}>", False
    raise SystemExit(f"unsupported schema shape: {schema}")


def _is_enum(schema: dict) -> bool:
    return "enum" in schema and schema.get("type") == "string"


def _to_value_expr(base: str, var: str, enums: set[str], objects: set[str], is_ref: bool) -> str:
    """`var` names a place of type `base` (is_ref=False) or `&base` (is_ref=True,
    e.g. the binding from `if let Some(item) = &self.field` or a `Vec::iter()`
    item). Method calls (`.clone()`, `.as_str()`, `.to_json()`, `.iter()`) auto-
    (de)ref either way; only `Value::from`/`Value::Bool`, which take an owned
    primitive by value, need an explicit deref when `var` is a reference.
    """
    if base == "String":
        return f"Value::String({var}.clone())"
    if base == INT:
        return f"Value::from(*{var})" if is_ref else f"Value::from({var})"
    if base == "bool":
        return f"Value::Bool(*{var})" if is_ref else f"Value::Bool({var})"
    if base in enums:
        return f"Value::String({var}.as_str().to_string())"
    if base in objects:
        return f"{var}.to_json()"
    if base.startswith("Vec<"):
        inner = base[len("Vec<") : -1]
        inner_expr = _to_value_expr(inner, "item", enums, objects, is_ref=True)
        return f"Value::Array({var}.iter().map(|item| {inner_expr}).collect())"
    raise SystemExit(f"unsupported rust type for to_value: {base}")


def _from_value_expr(base: str, var: str, enums: set[str], objects: set[str]) -> str:
    if base == "String":
        return f'{var}.as_str().ok_or_else(|| "expected a string".to_string())?.to_string()'
    if base == INT:
        return f'{var}.as_i64().ok_or_else(|| "expected an integer".to_string())?'
    if base == "bool":
        return f'{var}.as_bool().ok_or_else(|| "expected a boolean".to_string())?'
    if base in enums:
        return (
            f'{base}::parse({var}.as_str()'
            f'.ok_or_else(|| "expected a string".to_string())?)?'
        )
    if base in objects:
        return f"{base}::from_json({var})?"
    if base.startswith("Vec<"):
        inner = base[len("Vec<") : -1]
        inner_expr = _from_value_expr(inner, "item", enums, objects)
        return (
            f'{{ let array = {var}.as_array()'
            f'.ok_or_else(|| "expected an array".to_string())?; '
            f"let mut items = Vec::with_capacity(array.len()); "
            f"for item in array {{ items.push({inner_expr}); }} items }}"
        )
    raise SystemExit(f"unsupported rust type for from_value: {base}")


def _emit_enum(name: str, schema: dict) -> str:
    values = schema["enum"]
    variants = [(v, _enum_variant(v)) for v in values]

    lines = ["#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]"]
    lines.append(f"pub enum {name} {{")
    for i, (_v, variant) in enumerate(variants):
        if i == 0:
            lines.append("    #[default]")
        lines.append(f"    {variant},")
    lines.append("}")
    lines.append("")
    lines.append(f"impl {name} {{")
    lines.append("    pub fn as_str(&self) -> &'static str {")
    lines.append("        match self {")
    for v, variant in variants:
        lines.append(f'            {name}::{variant} => "{v}",')
    lines.append("        }")
    lines.append("    }")
    lines.append("")
    # Rejects an unknown value rather than silently defaulting -- the same
    # posture as the from_json this replaces.
    lines.append("    pub fn parse(raw: &str) -> Result<Self, String> {")
    lines.append("        match raw {")
    for v, variant in variants:
        lines.append(f'            "{v}" => Ok({name}::{variant}),')
    lines.append(
        f'            other => Err(format!("unknown {name}: {{other}}")),'
    )
    lines.append("        }")
    lines.append("    }")
    lines.append("}")
    return "\n".join(lines)


def _emit_struct(name: str, schema: dict, schemas: dict, enums: set[str], objects: set[str]) -> str:
    required = set(schema.get("required", []))
    props = schema.get("properties", {})
    fields = []
    for prop, pschema in props.items():
        base, nullable = _resolve_type(pschema, schemas)
        optional = nullable or prop not in required
        rust_type = f"Option<{base}>" if optional else base
        fields.append((prop, _field_ident(prop), rust_type, base, optional))

    lines = ["#[derive(Debug, Clone, Default, PartialEq)]"]
    lines.append(f"pub struct {name} {{")
    for _prop, ident, rust_type, _base, _optional in fields:
        lines.append(f"    pub {ident}: {rust_type},")
    lines.append("}")
    lines.append("")

    lines.append(f"impl {name} {{")

    # from_json is a tolerant reader: a missing or null field defaults rather
    # than erroring, so a server that predates a field this build knows about
    # still parses. A present field is strictly typed -- a wrong type or an
    # unknown enum value is still an error. Requests never go through here
    # (they are built in code), so only responses feel the tolerance, which is
    # the right posture for a client that deploys independently of the
    # server. A defaulted enum is its first member, a fine default.
    lines.append(
        "    pub fn from_json(value: &Value) -> Result<Self, String> {"
    )
    lines.append(
        '        let object = value.as_object().ok_or_else(|| "expected a JSON object".to_string())?;'
    )
    lines.append("        let mut result = Self::default();")
    for prop, ident, _rust_type, base, optional in fields:
        lines.append(f'        match object.get("{prop}") {{')
        lines.append("            Some(field) if !field.is_null() => {")
        from_expr = _from_value_expr(base, "field", enums, objects)
        if optional:
            lines.append(f"                result.{ident} = Some({from_expr});")
        else:
            lines.append(f"                result.{ident} = {from_expr};")
        lines.append("            }")
        lines.append("            _ => {}")
        lines.append("        }")
    lines.append("        Ok(result)")
    lines.append("    }")
    lines.append("")

    # to_json: emit required fields always, optionals only when set.
    lines.append("    pub fn to_json(&self) -> Value {")
    lines.append("        let mut map = serde_json::Map::new();")
    for prop, ident, _rust_type, base, optional in fields:
        if optional:
            lines.append(f"        if let Some(item) = &self.{ident} {{")
            to_expr = _to_value_expr(base, "item", enums, objects, is_ref=True)
            lines.append(f'            map.insert("{prop}".to_string(), {to_expr});')
            lines.append("        }")
        else:
            to_expr = _to_value_expr(base, f"self.{ident}", enums, objects, is_ref=False)
            lines.append(f'        map.insert("{prop}".to_string(), {to_expr});')
    lines.append("        Value::Object(map)")
    lines.append("    }")
    lines.append("}")
    return "\n".join(lines)


def _rustfmt(source: str) -> str:
    # The committed file is `cargo fmt`ted like everything else in the crate
    # (AGENTS.md: "cargo fmt applied"), so `--check` must diff against
    # rustfmt's own output, not this script's raw concatenation -- otherwise
    # every checked-in (formatted) file looks stale the moment it's generated.
    try:
        result = subprocess.run(
            ["rustfmt", "--edition=2021", "--emit=stdout"],
            input=source,
            capture_output=True,
            text=True,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"rustfmt failed while formatting the generated binding: {error}")
    return result.stdout


def render() -> str:
    raw = CONTRACT.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    document = json.loads(raw)
    schemas = document["components"]["schemas"]

    enums = {n for n in schemas if _is_enum(schemas[n])}
    objects = {
        n for n in schemas if schemas[n].get("type") == "object" and not _is_enum(schemas[n])
    }

    # Objects in dependency order: readable top-to-bottom the way the C++
    # header was, though Rust items may reference each other in any order.
    # A topological sort over $ref edges between object schemas.
    ordered: list[str] = []
    visiting: set[str] = set()

    def refs(name: str) -> list[str]:
        found: list[str] = []

        def walk(v: object) -> None:
            if isinstance(v, dict):
                if "$ref" in v:
                    found.append(v["$ref"].split("/")[-1])
                for x in v.values():
                    walk(x)
            elif isinstance(v, list):
                for x in v:
                    walk(x)

        walk(schemas[name])
        return found

    def visit(name: str) -> None:
        if name in ordered or name not in objects:
            return
        visiting.add(name)
        for dependency in refs(name):
            if dependency in objects and dependency not in visiting:
                visit(dependency)
        visiting.discard(name)
        if name not in ordered:
            ordered.append(name)

    for name in sorted(objects):
        visit(name)

    out = [
        "// Generated by contracts/generate_console_binding.py from",
        "// contracts/wally-cli-v1.openapi.json. DO NOT EDIT.",
        "//",
        "// Typed request and response models for the CLI's control-plane calls, so",
        "// console.rs neither builds a request body by hand nor reads a response",
        "// field by name. Regenerate and commit whenever the pinned contract moves.",
        "#![allow(clippy::all)]",
        "",
        "use serde_json::Value;",
        "",
        "/// SHA-256 of contracts/wally-cli-v1.openapi.json this file was built from.",
        f'pub const CONTRACT_SHA256: &str = "{digest}";',
        "",
    ]
    for name in sorted(enums):
        out.append(_emit_enum(name, schemas[name]))
        out.append("")
    for name in ordered:
        out.append(_emit_struct(name, schemas[name], schemas, enums, objects))
        out.append("")
    return _rustfmt("\n".join(out).rstrip("\n") + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail if the file is stale")
    args = parser.parse_args()
    rendered = render()
    if args.check:
        current = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else ""
        if current != rendered:
            sys.stderr.write(
                "console_contract.rs is stale. Run:\n"
                "  python3 contracts/generate_console_binding.py\n"
                "and commit the result.\n"
            )
            sys.exit(1)
        print("console_contract.rs matches the pinned contract")
        return
    OUTPUT.write_text(rendered, encoding="utf-8")
    print(f"wrote {OUTPUT}")


if __name__ == "__main__":
    main()
