#!/usr/bin/env python3
"""Refresh the CLI contract pin from an InferenceInfra checkout.

    python3 contracts/sync_from_inferenceinfra.py --from /path/to/InferenceInfra
    python3 contracts/sync_from_inferenceinfra.py --check
    python3 contracts/sync_from_inferenceinfra.py --check --from /path/to/InferenceInfra

`--check` without `--from` is the hermetic CI gate: the generated
`console_contract.h` matches the committed extract, and the extract records
the InferenceInfra commit it was carved from. Freshness against InferenceInfra
HEAD is enforced on the InferenceInfra PR (consumer-impact); this public
consumer cannot read that private repository from CI.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
EXTRACTOR = ROOT / "extract-cli-contract.py"
GENERATOR = ROOT / "generate_console_binding.py"
EXTRACT = ROOT / "wally-cli-v1.openapi.json"
CONTROL_PLANE = "contracts/control-plane-v1.openapi.json"


def _git(cwd: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=cwd,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(f"git {' '.join(args)} failed in {cwd}: {result.stderr.strip()}")
    return result.stdout.strip()


def _run(argv: list[str]) -> None:
    result = subprocess.run(argv, cwd=REPO, check=False)
    if result.returncode != 0:
        raise SystemExit(result.returncode)


def check_local() -> None:
    if not EXTRACT.is_file():
        raise SystemExit(f"missing {EXTRACT}")
    document = json.loads(EXTRACT.read_text(encoding="utf-8"))
    source = document.get("x-runanywhere-source") or {}
    for key in ("repository", "commit", "branch", "artifact"):
        if not source.get(key):
            raise SystemExit(
                f"{EXTRACT.name} is missing x-runanywhere-source.{key}. "
                "Run: python3 contracts/sync_from_inferenceinfra.py "
                "--from /path/to/InferenceInfra"
            )
    _run([sys.executable, str(GENERATOR), "--check"])
    print(
        f"Wally CLI lock OK ({source['commit'][:8]}, {source['branch']}, "
        f"{source['artifact']})"
    )


def sync_from(from_repo: Path) -> None:
    if not from_repo.is_dir():
        raise SystemExit(f"--from {from_repo} is not a directory")
    dirty = _git(from_repo, "status", "--porcelain")
    if dirty:
        raise SystemExit(
            f"--from {from_repo} has a dirty working tree. Commit or stash "
            "before syncing so the stamped commit is a real InferenceInfra revision."
        )
    source = from_repo / CONTROL_PLANE
    if not source.is_file():
        raise SystemExit(f"missing {source}")
    commit = _git(from_repo, "rev-parse", "HEAD")
    branch = _git(from_repo, "rev-parse", "--abbrev-ref", "HEAD")
    if branch in {"HEAD", ""}:
        # Detached checkout (CI worktree, `git worktree add --detach`). Prefer
        # development/main when this commit is on them so the pin names a real
        # branch instead of "HEAD".
        pointed = _git(
            from_repo,
            "for-each-ref",
            "--format=%(refname:short)",
            "--points-at",
            "HEAD",
            "refs/heads",
            "refs/remotes",
        )
        names = [line.strip() for line in pointed.splitlines() if line.strip()]
        for candidate in ("development", "main"):
            if candidate in names or any(name.endswith("/" + candidate) for name in names):
                branch = candidate
                break
        else:
            branch = names[0].rsplit("/", 1)[-1] if names else "detached"
    _run(
        [
            sys.executable,
            str(EXTRACTOR),
            str(source),
            "--output",
            str(EXTRACT),
            "--source-commit",
            commit,
            "--source-branch",
            branch,
        ]
    )
    _run([sys.executable, str(GENERATOR)])
    print(f"synced InferenceInfra {commit[:8]} ({branch})")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--from", dest="from_repo", type=Path, help="InferenceInfra checkout")
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if the committed pin is stale (hermetic without --from)",
    )
    args = parser.parse_args()
    if args.from_repo is None and not args.check:
        parser.error("pass --from <InferenceInfra checkout> and/or --check")
    if args.from_repo is not None:
        sync_from(args.from_repo.resolve())
    if args.check:
        check_local()


if __name__ == "__main__":
    main()
