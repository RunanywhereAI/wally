# Golden CLI corpus

Byte-exact stdout, stderr and exit code for 365 invocations of `wally`,
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

`wally prime-agent` added a line to every command list, so the 31 cases that
print one were re-captured from the Rust binary: `help__ROOT`, `help__help`,
the four `alt_help__*`, `version__bare`, every `unknown__*` case, and
`parse__--bogus`, `parse__-U__--json`, `parse__-u__-v`, `parse__-z`. That line
is the only difference in each. `help__prime-agent` and
`help__help__prime-agent` are new cases, mirroring OpenClaw's.

Re-capture from a binary (only when behaviour changes on purpose):

    python3 scripts/test/golden-capture.py --binary build/wally --out tests/golden --cases tests/golden/cases.json

Run a subset: `WALLY_GOLDEN_FILTER=models cargo test --test cli_golden`.

`local__models__list__--all` and `local__models__list__--all__--json` were
re-captured from the Rust binary when `models list` started taking a merged
row's size from the build its id pulls (it had shown the MLX build's size while
`pull <id>` fetched the GGUF), and started showing `mlx-ternary-bonsai-27b-2bit`
for the one MLX-only row whose merge key names no registered model. Those sizes
and that id are the only differences from the C++ capture.

Four `update` cases were edited when `wally update` dropped `--nightly` and
gained `-c, --check-for-updates` (`help__update`, `help__help__update`,
`leaf__update__bogus_flag`, `leaf__update__extra_positional`). The options
list is the only thing that differs from the C++ capture.

Four `uninstall` cases were edited when `wally uninstall` started keeping
downloaded models by default and gained `--delete-models`
(`help__uninstall`, `help__help__uninstall`, `leaf__uninstall__bogus_flag`,
`leaf__uninstall__extra_positional`). The options list is the only difference.

Fourteen `models list` cases were re-captured from the Rust binary when
`models list` gained `--cloud`, `--all` started adding the account's cloud
models, and `--local` took over the old meaning of `--all`. The differences are
the `--all`/`--local`/`--cloud` help lines, the empty-list hint (now `--local`), the "cloud models not shown"
line on a signed-out `--all`, and a `cloud` field on each `--json` row.
