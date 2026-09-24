#!/usr/bin/env python3
"""Fail if a version that cannot read versions.toml at its own build step has
drifted from it.

CMake reads versions.toml directly, so the build always agrees with it. Cargo
and rustup cannot: Cargo.toml's `version` and rust-toolchain.toml's `channel`
are each their own tool's source of truth and never open versions.toml. Same
for the Homebrew formula's `version` line and the download URLs it builds from
that version, and the Swift package's exact SDK pin. This checks all of them
against versions.toml so a bump in one place that misses the others fails CI
rather than shipping a mismatch.

    python3 scripts/ci/check-versions.py

Run from anywhere in the repo. Exits non-zero on the first mismatch.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
VERSIONS = ROOT / "versions.toml"
FORMULA = ROOT / "Formula" / "wally.rb"
PACKAGE = ROOT / "swift" / "Package.swift"
CMAKELISTS = ROOT / "CMakeLists.txt"
CARGO_TOML = ROOT / "Cargo.toml"
RUST_TOOLCHAIN = ROOT / "rust-toolchain.toml"
WORKFLOWS = ROOT / ".github" / "workflows"


def read_toml_value(key: str, path: Path = VERSIONS) -> str:
    pattern = re.compile(rf'^\s*{re.escape(key)}\s*=\s*"([^"]*)"', re.M)
    match = pattern.search(path.read_text(encoding="utf-8"))
    if not match:
        sys.exit(f"{path} is missing '{key}'")
    return match.group(1)


def read_toml_section(name: str, path: Path = VERSIONS) -> dict[str, str]:
    text = path.read_text(encoding="utf-8")
    body = re.search(rf'^\[{re.escape(name)}\]\n(.*?)(?=^\[|\Z)', text, re.M | re.S)
    if not body:
        sys.exit(f"{path} is missing section '[{name}]'")
    return dict(re.findall(r'^\s*(\w+)\s*=\s*"([^"]*)"', body.group(1), re.M))


def main() -> None:
    product = read_toml_value("version")
    swift_pin = read_toml_value("sdk_package_version")
    ci_ref = read_toml_value("sdk_ci_ref")
    failures: list[str] = []

    # The workflows that build the Apple MLX host check out runanywhere-sdks at
    # a tag. Both must name the tracked ref, so a bump here reaches CI and the
    # release together.
    for workflow in ("ci.yml", "release.yml"):
        path = ROOT / ".github" / "workflows" / workflow
        text = path.read_text(encoding="utf-8")
        if "runanywhere-sdks" in text:
            for ref in re.findall(r"ref:\s*v([0-9]+\.[0-9]+\.[0-9]+)", text):
                if ref != ci_ref:
                    failures.append(f"{path}: runanywhere-sdks ref v{ref} != versions.toml v{ci_ref}")

    # The formula's version line, and every release URL it builds, must name the
    # product version. update-tap.sh re-stamps these from a real release; this
    # catches the checked-in copy drifting from versions.toml between releases.
    formula = FORMULA.read_text(encoding="utf-8")
    formula_version = re.search(r'^\s*version\s+"([^"]*)"', formula, re.M)
    if not formula_version:
        failures.append(f"{FORMULA}: no version line found")
    elif formula_version.group(1) != product:
        failures.append(
            f"{FORMULA}: version \"{formula_version.group(1)}\" != versions.toml \"{product}\""
        )
    for url in re.findall(r'url\s+"([^"]*)"', formula):
        # A placeholder release with no published asset can name the product
        # version in its path; only flag a URL that names a different one.
        found = re.search(r"/v(\d+\.\d+\.\d+)/wally-(\d+\.\d+\.\d+)-", url)
        if found and (found.group(1) != product or found.group(2) != product):
            failures.append(f"{FORMULA}: url names {found.group(1)}/{found.group(2)}, not {product}")

    # The Swift package's exact SDK pin.
    package = PACKAGE.read_text(encoding="utf-8")
    package_pin = re.search(r'runanywhere-swift\.git",\s*exact:\s*"([^"]*)"', package)
    if not package_pin:
        failures.append(f"{PACKAGE}: no exact runanywhere-swift pin found")
    elif package_pin.group(1) != swift_pin:
        failures.append(
            f"{PACKAGE}: swift SDK pin \"{package_pin.group(1)}\" != versions.toml \"{swift_pin}\""
        )

    # CMake declares its own floor and C++ standard; hold them to the pins here.
    toolchain = read_toml_section("toolchain")

    # Cargo's own version, and rustup's own channel pin, each read by a tool
    # that never opens versions.toml.
    cargo_version = read_toml_section("package", CARGO_TOML).get("version")
    if cargo_version != product:
        failures.append(f"{CARGO_TOML}: version \"{cargo_version}\" != versions.toml \"{product}\"")
    rust_channel = read_toml_section("toolchain", RUST_TOOLCHAIN).get("channel")
    if rust_channel != toolchain.get("rust"):
        failures.append(
            f"{RUST_TOOLCHAIN}: channel \"{rust_channel}\" != versions.toml toolchain.rust \"{toolchain.get('rust')}\""
        )

    cmake = CMAKELISTS.read_text(encoding="utf-8")
    for label, pattern, key in (
        ("cmake_minimum_required", r"cmake_minimum_required\(VERSION\s+([0-9.]+)", "cmake_minimum"),
        ("CMAKE_CXX_STANDARD", r"CMAKE_CXX_STANDARD\s+([0-9]+)\)", "cxx_standard"),
    ):
        found = re.search(pattern, cmake)
        if not found:
            failures.append(f"{CMAKELISTS}: no {label} found")
        elif found.group(1) != toolchain.get(key):
            failures.append(
                f"{CMAKELISTS}: {label} {found.group(1)} != versions.toml {key} {toolchain.get(key)}"
            )

    # Every runner a workflow names must be one tracked here, so a new image
    # can't slip in unpinned.
    runners = set(read_toml_section("runners").values())
    for workflow in sorted(WORKFLOWS.glob("*.yml")):
        for runner in re.findall(r"runs-on:\s*(\S+)", workflow.read_text(encoding="utf-8")):
            if runner not in runners:
                failures.append(f"{workflow}: runs-on '{runner}' is not a tracked runner")

    if failures:
        sys.stderr.write("version drift from versions.toml:\n")
        for line in failures:
            sys.stderr.write(f"  {line}\n")
        sys.exit(1)
    print(f"versions.toml consistent: product {product}, swift SDK {swift_pin}")


if __name__ == "__main__":
    main()
