#!/usr/bin/env python3
"""Fail when a C++ test case has no Rust counterpart of the same name.

tests/cpp_case_ledger.txt lists every case the C++ suites registered before the
Rust port; each must appear as `fn <case>(` under tests/ or src/, or be listed
in tests/cpp_case_exceptions.txt with a reason. Run from anywhere.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]


def entries(path):
    for line in (ROOT / path).read_text().splitlines():
        line = line.strip()
        if line and not line.startswith('#'):
            yield line


def main():
    ledger = [tuple(l.split()[:2]) for l in entries('tests/cpp_case_ledger.txt')]
    exceptions = {tuple(l.split(' — ')[0].split()[:2]) for l in entries('tests/cpp_case_exceptions.txt')}
    sources = ''.join(p.read_text(errors='replace') for d in ('tests', 'src') for p in (ROOT / d).rglob('*.rs'))
    defined = set(re.findall(r'\bfn\s+([A-Za-z0-9_]+)\s*\(', sources))
    missing = [(f, c) for f, c in ledger if c not in defined and (f, c) not in exceptions]
    ported = len(ledger) - len(missing) - len(exceptions)
    print(f'{ported}/{len(ledger)} C++ cases ported, {len(exceptions)} excepted, {len(missing)} missing')
    for f, c in missing:
        print(f'  missing: {f} {c}')
    return 1 if missing else 0


if __name__ == '__main__':
    sys.exit(main())
