# Golden CLI corpus

Byte-exact stdout, stderr and exit code for 361 invocations of `wally`,
captured from the C++ build of `origin/main` at `d1e9c0b` (the last C++
release line) on macOS arm64 with kit 0.20.37. `tests/cli_golden.rs` replays
every case against the Rust binary in the same isolated environment and fails
on any difference — this is the proof that the port kept the CLI surface:
help for every command path, CLI11's parse errors and exit codes, `--json`
documents, and local read-only commands.

Only cases that fail to parse or only read local state are listed: nothing
here updates, uninstalls, serves, downloads or launches a tool.

- `cases.json` — the case list (`name`, `args`; `$HOME` means the isolated home)
- `expected/<name>.{stdout,stderr,code}`

Three cases were re-captured from the Rust binary when `wally account usage`
gained `--requests` (`help__account__usage`, `leaf__account__usage__bogus_flag`,
`leaf__account__usage__extra_positional`). Their help text is the only thing
that differs from the C++ capture; nothing else about them changed. The same
change reworded the command's one-line description, so every case that prints
the command list (the root and `account` help, `version__bare`, and the
`unknown__*` and `parse__*` errors) was re-captured too, and that line is the
only difference in each.

Re-capture from a binary (only when behaviour changes on purpose):

    python3 scripts/test/golden-capture.py --binary build/wally --out tests/golden --cases tests/golden/cases.json

Run a subset: `WALLY_GOLDEN_FILTER=models cargo test --test cli_golden`.
