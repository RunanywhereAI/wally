# Golden CLI corpus

Byte-exact stdout, stderr and exit code for 363 invocations of `wally`,
captured from the C++ build of `origin/main` at `d1e9c0b` (the last C++
release line) on macOS arm64 with kit 0.20.37. `tests/cli_golden.rs` replays
every case against the Rust binary in the same isolated environment and fails
on any difference — this is the proof that the port kept the CLI surface:
help for every command path, CLI11's parse errors and exit codes, `--json`
documents, and local commands that touch only the case's own throwaway home.

Only cases that fail to parse or touch nothing outside their own throwaway
home are listed: nothing here updates, uninstalls, serves, downloads or
launches a tool outside that home.

- `cases.json` — the case list (`name`, `args`; `$HOME` means the isolated home)
- `expected/<name>.{stdout,stderr,code}`

Three cases were re-captured from the Rust binary when `wally account usage`
gained `--requests` (`help__account__usage`, `leaf__account__usage__bogus_flag`,
`leaf__account__usage__extra_positional`). Their help text is the only thing
that differs from the C++ capture; nothing else about them changed. The same
change reworded the command's one-line description, so the 37 cases that print
a command list containing it were re-captured too, and that line is the only
difference in each: `help__ROOT`, `help__help`, the four `alt_help__*`,
`help__help__account__{login,logout,usage,whoami}`, `version__bare`, every
`unknown__*` case, and the `parse__*` errors that print the root or `account`
list (`parse__--bogus`, `parse__-U__--json`, `parse__-u__-v`, `parse__-z`,
`parse__account`, `parse__account__foo`). `parse__models` and
`parse__models__foo` print only the `models` help and did not change.

Three more cases were re-captured from the Rust binary when `wally bench`'s
`--vlm-image` help text was reworded to say it's required, since wally ships
no built-in VLM sample image (`help__bench`, `leaf__bench__bogus_flag`,
`leaf__bench__extra_positional`). Their help text is the only thing that
differs from the C++ capture; nothing else about them changed.

The same three cases were re-captured again when `wally bench`'s `model`
help text was reworded to say a model must already be downloaded, naming
`wally models pull <ref>`; their help text differs from the C++ capture in
the same way, nothing else changed.

`wally models default` gained `--json` support (the C++ original, and the
Rust port until now, silently accepted the flag and printed the plain-text
form): `local__models__default__--json` was re-captured from the Rust
binary, and `local__models__default__some-id__--json` /
`local__models__default__--clear__--json` are new cases covering the set and
clear paths.

Re-capture from a binary (only when behaviour changes on purpose):

    python3 scripts/test/golden-capture.py --binary build/wally --out tests/golden --cases tests/golden/cases.json

Run a subset: `WALLY_GOLDEN_FILTER=models cargo test --test cli_golden`.
