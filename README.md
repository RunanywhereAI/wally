# wally

Wally runs language models, local or in the cloud, and points your coding
tools at them through one local server.

![wally mascot](docs/assets/wally-mascot.gif)

## Quickstart

```sh
wally login              # sign in to Wally Cloud in your browser
wally serve               # start the local server other tools talk to
wally run llama-3.1-8b    # run a model once, or open a chat if you leave off the prompt
wally opencode             # point OpenCode at that model and launch it
```

`wally login` is optional if you only want an on-device model, but on-device
inference itself is not wired into this build yet. See [Status](#status).

## Install

Build from source (see below), or use the installers once a release exists:

```sh
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh   # macOS/Linux
```

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex        # Windows
```

`install.sh` and `install.ps1` fetch a released binary from GitHub Releases.

## Build from source

```sh
git clone https://github.com/RunanywhereAI/wally.git
cd wally
./scripts/build.sh
./build/wally version
```

`scripts/build.sh` builds the single `wally` binary for your machine's OS and
architecture. `scripts/dev.sh` does the same but points the binary at the dev
console instead of production, useful when testing against a non-production
backend. `scripts/test.sh` runs the test suite plus formatting and vet checks.
See [docs/commands.md](docs/commands.md) and [docs/daemon.md](docs/daemon.md)
for what each piece does.

## Commands

Grouped the way `wally help` groups them:

| Group | Commands |
|---|---|
| Run | `wally run [model] [prompt]`, `wally chat` |
| Serve | `wally serve` |
| Models | `wally models list \| show \| rm \| pull` |
| Coding agents | `wally claude-code`, `wally claude-desktop`, `wally opencode`, `wally hermes`, `wally openclaw`, `wally deepseek` |
| Manage | `wally harness`, `wally harness set-default-model` |
| Account | `wally login`, `wally logout`, `wally whoami` |
| About | `wally version`, `wally info`, `wally uninstall` |

Run `wally help` or `wally <command> --help` for the full picture; the table
above is the shape, not the whole reference. `docs/commands.md` covers each
group in more detail.

## Status

This is the Go rewrite of the wally CLI. The command surface, the local
daemon, cloud login, and the coding-agent launchers are live. Two pieces are
still deferred:

- **On-device inference.** `wally run` and `wally chat` work against Wally
  Cloud today. The on-device engine link is not in this build, so a local
  model returns a "not enabled in this build yet" error instead of running.
- **Model download.** `wally models pull` needs a downloader wired in via
  `WALLY_PULL_SCRIPT` or `scripts/pull-model.sh`. Neither ships yet, so a pull
  fails with a clear message pointing at that gap.

See `docs/` for what each area actually does today, deferred pieces called out
where they apply.

## Documentation

- [docs/commands.md](docs/commands.md): the full command surface, grouped and explained
- [docs/daemon.md](docs/daemon.md): the local server every client talks to
- [docs/harnesses.md](docs/harnesses.md): what a coding-agent launcher does and how one is wired
- [docs/login.md](docs/login.md): the device-flow login and where credentials live
