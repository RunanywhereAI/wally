#!/usr/bin/env python3
"""Carve the CLI-facing slice out of the full control-plane contract.

The CLI uses nine of the control plane's operations. Rather than vendor the whole
8000-line `control-plane-v1.openapi.json`, this extracts those operations and the
transitive closure of the schemas they reference into a self-contained, valid
OpenAPI document, `wally-cli-v1.openapi.json`, which is what gets pinned and fed
to `generate_console_binding.py`.

    python3 contracts/extract-cli-contract.py \\
        ../RA-Cloud-WorkSpace/InferenceInfra/contracts/control-plane-v1.openapi.json

Run this only when re-vendoring after the upstream contract changes; then run
generate_console_binding.py and commit both outputs together.
"""

from __future__ import annotations

import argparse
import collections
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
OUT = ROOT / "wally-cli-v1.openapi.json"

CLI_OPERATION_IDS = {
    "startCliAuthorization",
    "pollCliAuthorization",
    "refreshCliAuthorization",
    "revokeCliAuthorization",
    "getCurrentIdentity",
    "getCliUsage",
    # GET /v1/models and GET /v1/models/catalog: console.cpp's FetchModels and
    # FetchCatalog (#75). The committed artifact carried both since #75 while
    # this set still said six -- the artifact and the extractor had drifted,
    # which is exactly what a hash pin cannot see. Listed now.
    "listModels",
    "getModelCatalog",
    # POST /v1/requests/{request_id}/cancel (InferenceInfra #440): the shim
    # calls it when the editor abandons a stream (wally #81).
    "cancelRequest",
}
HTTP_METHODS = {"get", "post", "put", "delete", "patch"}
# The component sections a kept operation may reference. Until wally #81 only
# `schemas` was carried, so every `#/components/{parameters,responses}/...`
# reference in a kept operation dangled in the extract; the generator reads only
# `schemas`, so nothing broke, but the artifact was not the "self-contained,
# valid OpenAPI document" the docstring promised. Now the closure follows every
# section, and the extract is checked for dangling references before it is
# written.
COMPONENT_SECTIONS = ("schemas", "parameters", "responses", "requestBodies", "headers")


def _refs(value: object, out: set[str]) -> None:
    """Collect every `$ref` under `value`, as `(section, name)` pairs joined by '/'."""
    if isinstance(value, dict):
        if "$ref" in value:
            ref = value["$ref"]
            parts = ref.split("/")
            if len(parts) == 4 and parts[:2] == ["#", "components"]:
                out.add(parts[2] + "/" + parts[3])
            else:
                raise SystemExit(f"unsupported $ref shape in the source contract: {ref}")
        for child in value.values():
            _refs(child, out)
    elif isinstance(value, list):
        for child in value:
            _refs(child, out)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="path to control-plane-v1.openapi.json")
    args = parser.parse_args()

    source = json.loads(args.source.read_text(encoding="utf-8"))
    components = source["components"]

    closure: set[str] = set()

    def visit(key: str) -> None:
        section, name = key.split("/", 1)
        if key in closure:
            return
        if section not in COMPONENT_SECTIONS or name not in components.get(section, {}):
            raise SystemExit(f"kept operation references missing component {key}")
        closure.add(key)
        found: set[str] = set()
        _refs(components[section][name], found)
        for dependency in found:
            visit(dependency)

    paths: "collections.OrderedDict[str, dict]" = collections.OrderedDict()
    for path, item in source["paths"].items():
        kept = {}
        for method, operation in item.items():
            if method in HTTP_METHODS and operation.get("operationId") in CLI_OPERATION_IDS:
                kept[method] = operation
                found: set[str] = set()
                _refs(operation, found)
                for dependency in found:
                    visit(dependency)
        # Path-level parameters apply to every operation under the path.
        if kept and "parameters" in item:
            kept_params = item["parameters"]
            found = set()
            _refs(kept_params, found)
            for dependency in found:
                visit(dependency)
            kept = {"parameters": kept_params, **kept}
        if kept:
            paths[path] = kept

    kept_operations = {op for op in CLI_OPERATION_IDS}
    seen = {
        operation.get("operationId")
        for methods in paths.values()
        for method, operation in methods.items()
        if method in HTTP_METHODS
    }
    missing = sorted(kept_operations - seen)
    if missing:
        raise SystemExit(f"source contract lacks operations the CLI needs: {missing}")

    extracted_components: dict[str, dict] = {}
    for key in sorted(closure):
        section, name = key.split("/", 1)
        extracted_components.setdefault(section, {})[name] = components[section][name]

    extract = {
        "openapi": source["openapi"],
        "info": {
            "title": "Wally CLI control-plane contract (extract)",
            "version": source["info"]["version"],
            "description": (
                "CLI-facing operations extracted from control-plane-v1.openapi.json by "
                "contracts/extract-cli-contract.py. Do not hand-edit."
            ),
        },
        "paths": paths,
        "components": {section: extracted_components[section] for section in sorted(extracted_components)},
    }
    # Self-contained means self-contained: every $ref in the extract resolves
    # inside the extract.
    dangling: set[str] = set()
    _refs(extract, dangling)
    unresolved = sorted(key for key in dangling if key not in closure)
    if unresolved:
        raise SystemExit(f"extract would carry dangling references: {unresolved}")
    OUT.write_text(json.dumps(extract, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    operations = sum(1 for methods in paths.values() for method in methods if method in HTTP_METHODS)
    print(f"wrote {OUT}: {operations} operations, {len(closure)} components")


if __name__ == "__main__":
    main()
