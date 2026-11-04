#!/usr/bin/env bash
#
# check-agents-sync.sh — keep AGENTS.md/CLAUDE.md and .claude/.agents skills identical.
#
# CLAUDE.md is the single line `@AGENTS.md`, which Claude Code expands into
# AGENTS.md. A plain file, unlike a symlink, survives Windows checkouts without
# core.symlinks. .claude/skills/ is canonical; .agents/skills/ is a generated copy
# (not a directory symlink) so Windows checkouts without core.symlinks still
# work. CI fails if either pair drifts.
#
# Usage:
#   scripts/ci/check-agents-sync.sh          check (CI); exit 1 on drift
#   scripts/ci/check-agents-sync.sh --fix    recreate the symlink and mirror skills
#   scripts/ci/check-agents-sync.sh --help
#
# Exit codes: 0 ok/fixed | 1 drift | 2 tooling error
set -euo pipefail

usage() {
  cat <<'EOF'
check-agents-sync.sh — AGENTS.md ↔ CLAUDE.md and .claude/skills ↔ .agents/skills.

  scripts/ci/check-agents-sync.sh          check (CI / pre-commit)
  scripts/ci/check-agents-sync.sh --fix    rewrite CLAUDE.md + regenerate the skills mirror
  scripts/ci/check-agents-sync.sh --help

Exit codes: 0 ok/fixed | 1 drift | 2 tooling error
EOF
}

MODE=check
case "${1:-}" in
  ""|--check) MODE=check ;;
  --fix)      MODE=fix ;;
  -h|--help)  usage; exit 0 ;;
  *) printf 'error: unknown argument: %s\n' "$1" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

command -v git >/dev/null 2>&1 || { printf 'error: git not found on PATH\n' >&2; exit 2; }
git rev-parse --is-inside-work-tree >/dev/null 2>&1 \
  || { printf 'error: %s is not a git work tree\n' "${ROOT}" >&2; exit 2; }

status=0

is_tracked() { git ls-files --error-unmatch -- "$1" >/dev/null 2>&1; }

# --- AGENTS.md / CLAUDE.md ---------------------------------------------------
if [ ! -f AGENTS.md ]; then
  printf 'error: AGENTS.md is missing\n' >&2
  exit 1
fi

CLAUDE_IMPORT='@AGENTS.md'

claude_ok=0
if [ -f CLAUDE.md ] && [ ! -L CLAUDE.md ] && [ "$(cat CLAUDE.md)" = "${CLAUDE_IMPORT}" ]; then
  claude_ok=1
fi

if [ "${MODE}" = fix ]; then
  rm -f CLAUDE.md
  printf '%s\n' "${CLAUDE_IMPORT}" > CLAUDE.md
  printf 'wrote CLAUDE.md as %s\n' "${CLAUDE_IMPORT}"
else
  if [ "${claude_ok}" -ne 1 ]; then
    if [ ! -e CLAUDE.md ] && [ ! -L CLAUDE.md ]; then
      printf 'error: missing CLAUDE.md (should be the line %s; run --fix)\n' "${CLAUDE_IMPORT}" >&2
    elif [ -L CLAUDE.md ]; then
      printf 'error: CLAUDE.md is a symlink; it should be a file holding %s (run --fix)\n' "${CLAUDE_IMPORT}" >&2
    else
      printf 'error: CLAUDE.md must contain only %s; put instructions in AGENTS.md (run --fix)\n' "${CLAUDE_IMPORT}" >&2
    fi
    status=1
  elif ! is_tracked CLAUDE.md; then
    printf 'error: CLAUDE.md is correct but untracked — git add CLAUDE.md\n' >&2
    status=1
  fi
fi

# --- skills trees ------------------------------------------------------------
SRC=".claude/skills"
DST=".agents/skills"

if [ "${MODE}" = fix ]; then
  if [ ! -d "${SRC}" ]; then
    printf 'error: %s is missing; create canonical skills before --fix\n' "${SRC}" >&2
    exit 1
  fi
  rm -rf "${DST}"
  mkdir -p "$(dirname "${DST}")"
  cp -R "${SRC}" "${DST}"
  printf 'mirrored %s -> %s\n' "${SRC}" "${DST}"
  exit "${status}"
fi

if [ ! -d "${SRC}" ]; then
  printf 'error: missing canonical skills tree %s\n' "${SRC}" >&2
  exit 1
fi
if [ ! -d "${DST}" ]; then
  printf 'error: missing skills mirror %s (run --fix)\n' "${DST}" >&2
  exit 1
fi
if ! is_tracked "${SRC}/wally-architecture/SKILL.md" || ! is_tracked "${DST}/wally-architecture/SKILL.md"; then
  printf 'error: skill trees must be git-tracked (got ignored? check .gitignore)\n' >&2
  status=1
fi

if ! diff -rq "${SRC}" "${DST}" >/dev/null 2>&1; then
  printf 'error: %s and %s differ. Edit .claude/skills, then run --fix.\n' "${SRC}" "${DST}" >&2
  diff -rq "${SRC}" "${DST}" >&2 || true
  status=1
fi

if [ "${status}" -ne 0 ]; then
  printf '\nFix locally with:\n  bash scripts/ci/check-agents-sync.sh --fix\n  git add AGENTS.md CLAUDE.md .claude/skills .agents/skills\n' >&2
  exit 1
fi

printf 'CLAUDE.md imports AGENTS.md; .claude/skills and .agents/skills match.\n'
