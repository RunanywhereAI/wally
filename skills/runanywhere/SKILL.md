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

Offer to explain a harness before running it. Someone who has just signed up
does not yet know what opencode is, and "would you like me to explain how the
opencode harness works?" is a better second message than a launched TUI.

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
wally account usage --requests      # one page of settled requests, last day; --follow reads every page
```

Read-only, and scoped to the signed-in account.

## When something is wrong

- **"not signed in"** — `wally account login`.
- **"that key is not valid"** — the key was revoked or expired; `wally account login` again.
- **opencode not installed** — `npm i -g opencode-ai`.
- **a model is slow or unavailable** — `wally backends` shows which engines this
  build actually linked; a model needing an engine that is not there will not run.

## What not to do

Do not print, log or echo the contents of `~/.config/wally/credentials.json`.
It holds a key with the person's credit behind it.
