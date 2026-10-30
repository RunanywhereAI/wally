---
name: runanywhere
description: Set up and use RunAnywhere Wally from the terminal — install wally, sign in, pick a coding harness, run a model, check spend. Use when the user wants to get started with RunAnywhere or Wally, run a harness like opencode against a hosted or on-device model, or asks what their usage is.
---

# RunAnywhere

`wally` is one CLI for two things: models running **on this machine**, and models
served from **RunAnywhere Cloud**. The same commands cover both — if a model is
on the machine it is served locally, otherwise the request goes to the console
the user is signed in to and is metered against their balance.

## Install `wally` if it is missing

If `wally` is not on PATH, install it first — one line, no configuration and no
key to copy:

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

macOS (Apple Silicon) and Linux x86-64. On Windows:
`irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex`.
The installer only puts `wally` on PATH and writes this skill; it signs nothing
in on its own.

## First check what is already true

```bash
wally account whoami     # signed in? which console?
wally backends   # which engines this build linked
```

`wally account whoami` failing with "not signed in" is the only thing that needs fixing
before anything else works against the cloud. On-device models need no account.

## Signing in

```bash
wally account login
```

Opens the console in a browser. The person signs in with Google or GitHub,
approves the terminal, and the CLI stores a key in `~/.config/wally`. There is no
password and no organization step. If a browser cannot open, `wally account login
--no-browser` prints the URL to visit.

## Coding harnesses

A harness is an existing coding tool that `wally` wires to a model. Local
harnesses use the certified Qwen3 4B Instruct 2507 artifact.

```bash
wally opencode -m qwen3-4b-instruct-2507
```

If opencode is not installed, `wally` says so and prints the install command
(`npm i -g opencode-ai`) rather than failing. Install it, then run the same
line again.

`wally openwork -m <model>` opens the OpenWork desktop app with every
RunAnywhere model in its model picker, on a private profile. On macOS it offers
`brew install --cask openwork` when the app is missing.

Offer to explain a harness before running it. Someone who has just signed up
does not yet know what opencode is, and "would you like me to explain how the
opencode harness works?" is a better second message than a launched TUI.

## Shannon and other tools

`wally shannon -- -u "$APP_URL" -r "$REPO_PATH"` runs Shannon, an open-source AI
pentester, on a model from the person's account. Only suggest it for a system
they own or are authorised to test. It needs Docker running, plus Node 18+ unless Shannon 3.4.0 is
installed on PATH, and wally sets up the model config and key itself. For a scan longer than a
sign-in lasts, the person exports a console Cloud key as `WALLY_CLOUD_KEY`.
Setup details, and how to point any other OpenAI-compatible tool at the
endpoint by hand, are in `docs/EDITORS.md`.

## Security tools

`wally deepsec` and `wally vvah` run hosted security scanners on a model from the
person's account. Only suggest them for a repo or system they own or are
authorised to test. DeepSec (2.3.10) needs Node 18+ for `npx` unless a `deepsec` on PATH reports exactly 2.3.10; VVAH (1.5.1) needs
Python 3.11–3.14. Wally sets up routing per tool (command-line flags for DeepSec, a
config file in the Wally profile for VVAH); keys go in the child environment
only, never on the command line, and competing variables are removed for that
child only. For a
run longer than a sign-in lasts, they export a console Cloud key as
`WALLY_CLOUD_KEY`. Details are in `docs/EDITORS.md`.

```bash
wally deepsec -- init . "$REPO_PATH"
wally vvah -- scan --repo "$REPO_PATH"
```

## Running a model directly

```bash
wally models pull qwen3-4b-instruct-2507  # download it
wally models list                         # what is downloaded
wally run qwen3-4b-instruct-2507          # talk to it
```

Models land in `~/.local/share/runanywhere`. Nothing is downloaded until asked.

## Spend

```bash
wally account usage               # credit left, then input/output/cache tokens and spend
wally account usage --json
wally account usage --requests      # one page of settled requests, last day; --follow walks up to 100 pages
```

Read-only, and scoped to the signed-in account.

## Decisions with Eve

`eve`, the default for `wally decisions`, scores named labels without
generating prose:

```bash
wally decisions --input "Checkout is blank" --ask "Is this a bug?"
wally decide --input-file ticket.txt --choice "Owner=frontend,payments,account"
```

`--image FILE` (PNG, JPEG or WebP, up to 8, hosted only; HEIC is converted to
JPEG on macOS) asks the questions about images too; a model that does not take
them refuses with 400.

Use `--json` when another tool needs the raw response. Do not send `eve` (or
its retired names `pplx-decider-v1` and `qwev`) to `wally run` or a coding
harness; those surfaces refuse it and point back here.

## When something is wrong

- **"not signed in"** — `wally account login`.
- **"not available on this account"** — that model may need approval. Check
  the model id first; if it is right and the model is gated, request access at
  https://runwally.com/security/request. Do not log in again.
- **DeepSec or VVAH prerequisites missing** — install Node 18+ (DeepSec) or Python 3.11–3.14 (VVAH); see `docs/EDITORS.md`.
- **"that key is not valid"** — the key was revoked or expired; `wally account login` again.
- **opencode not installed** — `npm i -g opencode-ai`.
- **a model is slow or unavailable** — `wally backends` shows which engines this
  build actually linked; a model needing an engine that is not there will not run.

## What not to do

Do not print, log or echo the contents of `~/.config/wally/credentials.json`.
It holds a key with the person's credit behind it.
