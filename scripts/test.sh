#!/usr/bin/env bash
# go test ./... plus a gofmt-clean check and go vet ./...
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root_dir"

echo "gofmt -l ." >&2
unformatted="$(gofmt -l .)"
if [[ -n "$unformatted" ]]; then
  echo "gofmt found unformatted files:" >&2
  echo "$unformatted" >&2
  exit 1
fi

echo "go vet ./..." >&2
go vet ./...

echo "go test ./..." >&2
go test ./...
