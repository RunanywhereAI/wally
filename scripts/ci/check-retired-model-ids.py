#!/usr/bin/env python3
"""Fail if a retired hosted model id appears anywhere a person or an agent is
told what to type.

A retired id is refused by the gateway with 403 model_not_entitled, so an
installer hint or a skill that names one sends every new user straight into an
error. install.sh printed `wally opencode --cloud -m glm-5.3` in its "Next:"
block, and the RunAnywhere skill the installer copies into agents' skill
folders used the same id, long after `glm-5.3-flash` replaced it.

RETIRED mirrors `launch_models.retired` in InferenceInfra's
contracts/public/status_semantics.json, which is the machine-readable list of
what RunAnywhere serves. When the service retires an id, add it here.

    python3 scripts/ci/check-retired-model-ids.py

Scans user-facing text only: the installers, READMEs, docs, skills and the CLI
source (help text). tests/ is excluded on purpose -- fixtures there use
arbitrary ids, including retired ones a real usage history still carries.
Exits non-zero and prints file:line for every hit.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent

RETIRED = ("glm-5.3", "glm-5.2", "gemini-2.5-flash")

# Files and trees a user or an agent reads instructions from.
SCANNED = (
    "install.sh",
    "install.ps1",
    "README.md",
    "CONTRIBUTING.md",
    "AGENTS.md",
    "docs",
    "skills",
    ".claude/skills",
    ".agents/skills",
    "src",
)
TEXT_SUFFIXES = {".sh", ".ps1", ".md", ".rs", ".cpp", ".h", ".hpp", ".swift", ".txt", ""}


def pattern_for(model_id: str) -> re.Pattern[str]:
    """Match `model_id` as a whole id: `glm-5.3` but not `glm-5.3-flash`,
    `glm-5.30` or `xglm-5.3`. A sentence-ending period still counts as a hit."""
    return re.compile(
        r"(?<![A-Za-z0-9_.-])" + re.escape(model_id) + r"(?![A-Za-z0-9_-]|\.[A-Za-z0-9])"
    )


PATTERNS = tuple((model_id, pattern_for(model_id)) for model_id in RETIRED)


def find_retired(text: str) -> list[tuple[int, str]]:
    """(line number, retired id) for every retired id in `text`."""
    hits = []
    for number, line in enumerate(text.splitlines(), start=1):
        for model_id, pattern in PATTERNS:
            if pattern.search(line):
                hits.append((number, model_id))
    return hits


def scanned_files(root: Path = ROOT) -> list[Path]:
    files = []
    for entry in SCANNED:
        path = root / entry
        if path.is_file():
            files.append(path)
        elif path.is_dir():
            files.extend(
                candidate
                for candidate in sorted(path.rglob("*"))
                if candidate.is_file() and candidate.suffix in TEXT_SUFFIXES
            )
    return files


def scan(root: Path = ROOT) -> list[str]:
    findings = []
    for path in scanned_files(root):
        try:
            text = path.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        for number, model_id in find_retired(text):
            findings.append(f"{path.relative_to(root)}:{number}: retired model id '{model_id}'")
    return findings


def main() -> int:
    findings = scan()
    if findings:
        print("\n".join(findings), file=sys.stderr)
        print(
            f"{len(findings)} retired model id(s) in user-facing text; the gateway "
            "refuses these with 403 model_not_entitled.",
            file=sys.stderr,
        )
        return 1
    print(f"no retired model ids in {len(scanned_files())} user-facing files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
