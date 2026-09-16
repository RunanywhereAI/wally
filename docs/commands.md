# Commands

`wally help` is the live source of truth; groups and one-line summaries below
match the code at the time this was written.

## Run

- `wally run [model] [prompt]`: run a model once and print the answer, or drop
  into a chat if you leave off the prompt. With no model given, wally picks
  one for you.
- `wally chat`: open an interactive chat with a model.

Both stream through the local daemon (see [daemon.md](daemon.md)), so `wally
serve` does not need to be running separately; a client starts the daemon
itself if it is not already up.

## Serve

- `wally serve`: run the local server in the foreground and print the address
  it is listening on. This is what a coding tool or `curl` talks to directly.

## Models

- `wally models list`: models installed on this machine.
- `wally models show <id>`: details for one installed model.
- `wally models rm <id>`: delete an installed model (`-y` skips the confirm).
- `wally models pull <id>`: download a model into the on-device store. This
  needs a downloader wired in through `WALLY_PULL_SCRIPT` or
  `scripts/pull-model.sh`; neither ships yet, so a pull fails today with a
  message naming the gap instead of silently doing nothing.

## Coding agents

One subcommand per harness, each launching that tool wired against a model
through the daemon: `wally claude-code`, `wally claude-desktop`, `wally
opencode`, `wally hermes`, `wally openclaw`, `wally deepseek`. Add `--model` to
pick one explicitly. See [harnesses.md](harnesses.md) for what "wired" means.

## Manage

- `wally harness`: install, remove, and configure the coding harnesses wally
  can launch.
- `wally harness set-default-model <harness> <model>`: set a harness's default
  model without going through the interactive menu.

## Account

- `wally login`: sign in to Wally Cloud through a browser device flow.
- `wally logout`: sign out on this machine.
- `wally whoami`: show the signed-in account and this month's usage.

## About

- `wally version`: print the version this binary was built with.
- `wally info`: version, platform, Go runtime, and the console this build
  talks to.
- `wally uninstall`: an interactive picker for removing wally data from this
  machine (models, chats, configs, harness state, or your sign-in). Nothing is
  deleted until you confirm.

## Deferred

On-device inference is not linked into this build yet. `wally run` and `wally
chat` against a local model return an explicit "not enabled in this build
yet" error rather than a silent failure; cloud models are unaffected.
