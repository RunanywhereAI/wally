#!/usr/bin/env python3
"""Refresh the CLI contract pin from an InferenceInfra checkout.

    python3 contracts/sync_from_inferenceinfra.py --from /path/to/InferenceInfra
    python3 contracts/sync_from_inferenceinfra.py --check
    python3 contracts/sync_from_inferenceinfra.py --check --from /path/to/InferenceInfra

`--check` without `--from` is the hermetic CI gate: the generated
`console_contract.rs` matches the committed extract, and the extract records
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
# The two files a sync regenerates from scratch. Named once so the destination
# guard and the error message cannot drift apart.
GENERATED = ("contracts/wally-cli-v1.openapi.json", "src/account/console_contract.rs")


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


def _pointed_names(repo: Path, namespace: str) -> list[str]:
    """Short names of the refs under `namespace` that point at HEAD."""
    pointed = _git(
        repo,
        "for-each-ref",
        "--format=%(refname:short)",
        "--points-at",
        "HEAD",
        namespace,
    )
    return [line.strip() for line in pointed.splitlines() if line.strip()]


def _strip_remote(name: str) -> str:
    """`origin/feature/cli-sync` -> `feature/cli-sync`; `origin/HEAD` -> ``.

    Only the remote name comes off. The rest is the branch, slashes included.
    `origin/HEAD` is the remote's symbolic default and names no branch of its
    own, so it is dropped rather than reported as a branch called "HEAD".
    """
    _, separator, rest = name.partition("/")
    if not separator or rest in {"", "HEAD"}:
        return ""
    return rest


def _run(argv: list[str]) -> None:
    result = subprocess.run(argv, cwd=REPO, check=False)
    if result.returncode != 0:
        raise SystemExit(result.returncode)


def check_local(extract: Path = EXTRACT) -> None:
    if not extract.is_file():
        raise SystemExit(f"missing {extract}")
    document = json.loads(extract.read_text(encoding="utf-8"))
    source = document.get("x-runanywhere-source") or {}
    for key in ("repository", "commit", "branch", "artifact"):
        if not source.get(key):
            raise SystemExit(
                f"{extract.name} is missing x-runanywhere-source.{key}. "
                "Run: python3 contracts/sync_from_inferenceinfra.py "
                "--from /path/to/InferenceInfra"
            )
    stamp = f"{source['commit'][:8]}, {source['branch']}, {source['artifact']}"
    if extract.resolve() != EXTRACT.resolve():
        # An alternate extract proves nothing about what ships. generate_console
        # _binding.py reads the committed extract and has no path override, so
        # the header cannot be checked against some other file -- and the
        # committed pin was never looked at either. Say so in the words this
        # run actually earned: "Wally CLI lock OK" means the shipping lock is
        # good, and a run over another file must never be able to print it.
        # The caveat goes to stderr so stdout carries only the result.
        print(
            f"note: {extract} is not the committed extract; "
            "console_contract.rs and the committed pin were NOT checked",
            file=sys.stderr,
        )
        print(f"alternate extract provenance OK ({stamp})")
        return
    _run([sys.executable, str(GENERATOR), "--check"])
    print(f"Wally CLI lock OK ({stamp})")


def _dirty_generated() -> list[str]:
    """The generated files that differ from HEAD in this repo, if any.

    `diff --name-only HEAD` rather than `status --porcelain` on purpose: it
    emits bare paths, covering staged and unstaged edits alike, with no status
    columns to parse. (_git strips the output, which would eat porcelain's
    leading column and silently truncate the first path.) Both files are
    tracked, so nothing is missed by not reporting untracked entries.
    """
    changed = _git(REPO, "diff", "--name-only", "HEAD", "--", *GENERATED)
    return [line.strip() for line in changed.splitlines() if line.strip()]


def sync_from(from_repo: Path, force: bool = False) -> None:
    if not from_repo.is_dir():
        raise SystemExit(f"--from {from_repo} is not a directory")
    dirty = _git(from_repo, "status", "--porcelain")
    if dirty:
        raise SystemExit(
            f"--from {from_repo} has a dirty working tree. Commit or stash "
            "before syncing so the stamped commit is a real InferenceInfra revision."
        )
    # A sync rewrites both generated files from the source contract, so any
    # uncommitted edit sitting in them is destroyed with no way back. The
    # source tree is already refused when dirty for the same class of reason;
    # the destination gets the same courtesy rather than a silent overwrite.
    if not force:
        clobbered = _dirty_generated()
        if clobbered:
            raise SystemExit(
                "these generated files have uncommitted changes and a sync would "
                "overwrite them:\n  " + "\n  ".join(clobbered) + "\n"
                "Commit or stash them first, or pass --force to overwrite."
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
        # Local heads first: their short name IS the branch name, slashes and
        # all. A remote ref carries a remote prefix on top of that, so it needs
        # one -- and only one -- leading segment removed.
        heads = _pointed_names(from_repo, "refs/heads")
        remotes = [_strip_remote(name) for name in _pointed_names(from_repo, "refs/remotes")]
        names = heads + [name for name in remotes if name]
        for candidate in ("development", "main"):
            if candidate in names:
                branch = candidate
                break
        else:
            # Whatever ref actually points here, named in full. Splitting on
            # "/" here would report `feature/cli-sync` as `cli-sync`, so the
            # pin would name a branch that does not exist.
            branch = names[0] if names else "detached"
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
    parser.add_argument(
        "--force",
        action="store_true",
        help="overwrite generated files that have uncommitted changes",
    )
    parser.add_argument(
        "--extract",
        type=Path,
        default=EXTRACT,
        help=(
            "check provenance in this extract instead of the committed one "
            "(a testing aid: it verifies neither the committed pin nor "
            "console_contract.rs, and never reports the lock as OK)"
        ),
    )
    args = parser.parse_args()
    if args.from_repo is None and not args.check:
        parser.error("pass --from <InferenceInfra checkout> and/or --check")
    if args.from_repo is not None:
        sync_from(args.from_repo.resolve(), force=args.force)
    if args.check:
        check_local(args.extract)


if __name__ == "__main__":
    main()
