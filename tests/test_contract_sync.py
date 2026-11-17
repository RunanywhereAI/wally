#!/usr/bin/env python3
"""The CLI contract sync wrapper describes itself and refuses a pin with no provenance; the extractor carves the slice."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SYNC = REPO / "contracts" / "sync_from_inferenceinfra.py"
EXTRACT = REPO / "contracts" / "wally-cli-v1.openapi.json"
EXTRACTOR = REPO / "contracts" / "extract-cli-contract.py"


def _check(*extra: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(SYNC), "--check", *extra],
        cwd=REPO,
        capture_output=True,
        text=True,
        check=False,
    )


class ContractSyncTests(unittest.TestCase):
    def test_help_lists_from_and_check(self):
        result = subprocess.run(
            [sys.executable, str(SYNC), "--help"],
            cwd=REPO,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--from", result.stdout)
        self.assertIn("--check", result.stdout)

    def test_committed_pin_passes_check(self):
        """The pin that ships is a good one. This is the hermetic CI gate."""
        result = _check()
        self.assertEqual(0, result.returncode, result.stderr + result.stdout)
        self.assertIn("Wally CLI lock OK", result.stdout)

    def test_alternate_extract_passes_but_is_not_the_lock_verdict(self):
        """The alternate-extract success branch, and the words it may not use.

        Two regressions this catches. If check_local() started refusing every
        alternate extract, the refusal tests below would all still pass and
        nothing would notice. And if the alternate path went back to printing
        "Wally CLI lock OK", a run that never looked at the committed pin or at
        console_contract.h would claim the shipping lock is good.
        """
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        with tempfile.TemporaryDirectory() as directory:
            extract = Path(directory) / "wally-cli-v1.openapi.json"
            extract.write_text(json.dumps(document), encoding="utf-8")
            result = _check("--extract", str(extract))

        self.assertEqual(0, result.returncode, result.stderr + result.stdout)
        self.assertIn("alternate extract provenance OK", result.stdout)
        # The lock verdict is reserved for the committed extract. stdout is
        # what a script greps, so the claim must not appear there at all.
        self.assertNotIn("Wally CLI lock OK", result.stdout)
        self.assertIn("NOT checked", result.stderr)

    def _assert_refused(self, document: dict, missing: str) -> None:
        """--check must refuse `document` and name the field it wanted."""
        with tempfile.TemporaryDirectory() as directory:
            extract = Path(directory) / "wally-cli-v1.openapi.json"
            extract.write_text(json.dumps(document), encoding="utf-8")
            result = _check("--extract", str(extract))
        self.assertNotEqual(
            0, result.returncode, f"expected a refusal for missing {missing}"
        )
        self.assertIn("x-runanywhere-source", result.stderr)

    def test_check_refuses_extract_with_no_provenance(self):
        """Runs the refusal path every time, whatever the committed pin holds.

        The previous version of this test branched on the committed extract:
        once the pin was stamped -- the normal state -- the refusal path stopped
        executing, so dropping the provenance requirement from check_local()
        would not have failed anything.
        """
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        document.pop("x-runanywhere-source", None)
        self._assert_refused(document, "the whole x-runanywhere-source block")

    def test_check_refuses_each_missing_provenance_field(self):
        """Every field check_local() requires is load-bearing, not just one."""
        for field in ("repository", "commit", "branch", "artifact"):
            with self.subTest(field=field):
                document = json.loads(EXTRACT.read_text(encoding="utf-8"))
                source = dict(document.get("x-runanywhere-source") or {})
                # A present-but-empty value must be refused too: that is exactly
                # what a half-stamped extract carries.
                source[field] = ""
                document["x-runanywhere-source"] = source
                self._assert_refused(document, field)


class ExtractorNarrowingTests(unittest.TestCase):
    """`GET /v1/models` is carved to the one form the CLI reads (extract-cli-contract.py)."""

    def _extract(self, source: dict) -> dict:
        with tempfile.TemporaryDirectory() as directory:
            source_path = Path(directory) / "control-plane-v1.openapi.json"
            source_path.write_text(json.dumps(source), encoding="utf-8")
            output = Path(directory) / "extract.json"
            result = subprocess.run(
                [sys.executable, str(EXTRACTOR), str(source_path), "--output", str(output)],
                cwd=REPO,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(0, result.returncode, result.stderr + result.stdout)
            return json.loads(output.read_text(encoding="utf-8"))

    def _source_with_a_format_query(self) -> dict:
        """The committed extract, with listModels as the control plane describes it once the
        models list can also answer `?format=openrouter-v2`: a format query, and a `oneOf`
        response with no discriminator whose second branch is a shape the generator cannot read."""
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        components = document["components"]
        components["parameters"]["ModelListFormatQuery"] = {
            "in": "query",
            "name": "format",
            "required": False,
            "schema": {"enum": ["openrouter-v2"], "type": "string"},
        }
        components["schemas"]["OpenRouterModelDocumentList"] = {
            "type": "object",
            "properties": {"data": {"type": "array", "items": {"type": "object"}}},
        }
        components["schemas"]["ModelListResponse"] = {
            "oneOf": [
                {"$ref": "#/components/schemas/ModelList"},
                {"$ref": "#/components/schemas/OpenRouterModelDocumentList"},
            ]
        }
        operation = document["paths"]["/v1/models"]["get"]
        operation["parameters"] = [{"$ref": "#/components/parameters/ModelListFormatQuery"}]
        operation["responses"]["200"]["content"]["application/json"]["schema"] = {
            "$ref": "#/components/schemas/ModelListResponse"
        }
        return document

    def test_models_list_keeps_only_the_default_form(self):
        extract = self._extract(self._source_with_a_format_query())
        operation = extract["paths"]["/v1/models"]["get"]
        self.assertEqual(
            {"$ref": "#/components/schemas/ModelList"},
            operation["responses"]["200"]["content"]["application/json"]["schema"],
        )
        self.assertNotIn("parameters", operation)
        self.assertNotIn("ModelListFormatQuery", extract["components"]["parameters"])
        self.assertNotIn("ModelListResponse", extract["components"]["schemas"])
        self.assertNotIn("OpenRouterModelDocumentList", extract["components"]["schemas"])

    def test_the_other_form_changing_does_not_move_the_slice(self):
        before = self._source_with_a_format_query()
        after = self._source_with_a_format_query()
        after["components"]["schemas"]["OpenRouterModelDocumentList"]["properties"]["next"] = {
            "type": "string"
        }
        self.assertEqual(self._extract(before), self._extract(after))

    def test_a_source_without_the_format_query_is_carried_unchanged(self):
        document = json.loads(EXTRACT.read_text(encoding="utf-8"))
        self.assertEqual(
            document["paths"], self._extract(document)["paths"]
        )


GENERATOR = REPO / "contracts" / "generate_console_binding.py"
LEGACY_EXTRACT = REPO / "contracts" / "wally-cli-v1.legacy.openapi.json"
DECISIONS_LEGACY = REPO / "contracts" / "wally-decisions-public-v1.legacy.openapi.json"


def _tiny_contract(**schemas: dict) -> dict:
    return {
        "openapi": "3.1.0",
        "info": {"title": "t", "version": "1"},
        "paths": {},
        "components": {"schemas": schemas},
    }


class LegacyMergeTests(unittest.TestCase):
    """Until the DNS flip the CLI talks to legacy; after it, to new infra. The binding reads both.

    generate_console_binding.py --legacy-contract adds what only legacy sends (enum values,
    optional properties, whole schemas) to the binding of the pinned contract, and refuses a
    property the two type differently, since no one reader parses both."""

    def _render(self, current: dict, legacy: dict | None) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as directory:
            current_path = Path(directory) / "current.json"
            current_path.write_text(json.dumps(current), encoding="utf-8")
            output = Path(directory) / "out.rs"
            argv = [
                sys.executable,
                str(GENERATOR),
                "--contract",
                str(current_path),
                "--output",
                str(output),
            ]
            if legacy is not None:
                legacy_path = Path(directory) / "legacy.json"
                legacy_path.write_text(json.dumps(legacy), encoding="utf-8")
                argv += ["--legacy-contract", str(legacy_path)]
            result = subprocess.run(argv, cwd=REPO, capture_output=True, text=True, check=False)
            if result.returncode == 0:
                result.stdout = output.read_text(encoding="utf-8")
            return result

    def _pair(self) -> tuple[dict, dict]:
        current = _tiny_contract(
            Code={"type": "string", "enum": ["forbidden", "organization_limit_reached"]},
            Me={
                "type": "object",
                "required": ["email"],
                "properties": {"email": {"type": "string"}},
            },
        )
        legacy = _tiny_contract(
            Code={"type": "string", "enum": ["forbidden", "coding_plan_full"]},
            Kind={"type": "string", "enum": ["personal", "team"]},
            Me={
                "type": "object",
                "required": ["email", "kind"],
                "properties": {
                    "email": {"type": "string"},
                    "kind": {"$ref": "#/components/schemas/Kind"},
                },
            },
        )
        return current, legacy

    def test_legacy_only_values_properties_and_schemas_are_added(self):
        current, legacy = self._pair()
        result = self._render(current, legacy)
        self.assertEqual(0, result.returncode, result.stderr)
        binding = result.stdout
        # Both servers' codes parse: the new one first, then what only legacy sends.
        self.assertIn('"organization_limit_reached" => Ok(Code::KOrganizationLimitReached)', binding)
        self.assertIn('"coding_plan_full" => Ok(Code::KCodingPlanFull)', binding)
        # A legacy-only schema is emitted, and a legacy-only property is read as optional
        # (new infra never sends it), even where legacy marks it required.
        self.assertIn("pub enum Kind", binding)
        self.assertIn("pub kind: Option<Kind>,", binding)
        self.assertIn("pub email: String,", binding)
        self.assertIn("pub const LEGACY_CONTRACT_SHA256", binding)

    def test_no_legacy_contract_renders_the_pinned_contract_alone(self):
        current, _legacy = self._pair()
        result = self._render(current, None)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertNotIn("coding_plan_full", result.stdout)
        self.assertNotIn("LEGACY_CONTRACT_SHA256", result.stdout)

    def test_a_property_typed_differently_is_refused(self):
        current, legacy = self._pair()
        legacy["components"]["schemas"]["Me"]["properties"]["email"] = {"type": "integer"}
        result = self._render(current, legacy)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("Me.email", result.stderr)

    def test_a_schema_that_changes_kind_is_refused(self):
        current, legacy = self._pair()
        legacy["components"]["schemas"]["Code"] = {"type": "object", "properties": {}}
        result = self._render(current, legacy)
        self.assertNotEqual(0, result.returncode)
        self.assertIn("Code", result.stderr)

    def test_committed_legacy_pins_carry_provenance(self):
        """The legacy extract names the InferenceInfra commit legacy production runs."""
        document = json.loads(LEGACY_EXTRACT.read_text(encoding="utf-8"))
        source = document.get("x-runanywhere-source") or {}
        for key in ("repository", "commit", "branch", "artifact"):
            self.assertTrue(source.get(key), key)
        self.assertEqual("main", source["branch"])
        self.assertTrue(DECISIONS_LEGACY.is_file())

    def test_committed_binding_reads_both_servers_codes(self):
        binding = (REPO / "src" / "account" / "console_contract.rs").read_text(encoding="utf-8")
        self.assertIn('"organization_limit_reached"', binding)
        self.assertIn('"coding_plan_full"', binding)
        self.assertIn("pub account: Option<IdentityAccount>,", binding)


if __name__ == "__main__":
    unittest.main()
