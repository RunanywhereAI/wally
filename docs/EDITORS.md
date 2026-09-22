# Editors, coding agents and local or hosted models

How `wally` wires each tool, and where a signed-in session lives.

## Editors and coding agents

One command points a tool at a model and starts it. There is nothing to
configure by hand:

```bash
wally claude-code -m qwen3-4b-instruct-2507
wally hermes -m qwen3-4b-instruct-2507
wally deepseek -m qwen3-4b-instruct-2507
wally openclaw -m qwen3-4b-instruct-2507
```

The model can be one on this machine or one the console serves. Without `-m` the
tool starts the way you already have it configured, and wally wires nothing.

| Tool | How it is wired |
| --- | --- |
| `claude-code` | `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` in the process |
| `opencode` | a temporary provider in `OPENCODE_CONFIG_CONTENT`, with the local or hosted endpoint and context limits |
| `claude-desktop` | a gateway profile in Claude Desktop's third party mode, covering the chat and Cowork tabs |
| `hermes` | `CUSTOM_BASE_URL`, `HERMES_INFERENCE_PROVIDER=custom`, the model in `HERMES_INFERENCE_MODEL`, and the key under the name its host gates on |
| `openclaw` | your own `openclaw.json` plus one provider, written for the run and named by `OPENCLAW_CONFIG_PATH` |
| `deepseek` | a `--patch` overlay on the argv, pointing dsh at a settings document wally wrote; nothing enters `$DSH_HOME` |

Two flags go with `-m`. `--serve` holds the endpoint open and prints it instead
of launching anything, which is how a tool nobody has taught wally about gets
wired up. `--restore` puts Claude Desktop back the way it was and starts
nothing; a normal run already undoes its own configuration when the app quits,
so this is for the run that was interrupted before it could.

Claude Code and Claude Desktop speak Anthropic's Messages API, while the models
wally serves speak OpenAI's, so a translator sits between them. It carries tool
definitions out, tool calls back, and the results of those calls out again,
which is what lets an agent on the far side run the tools it was given rather
than describe them. `opencode`, `hermes` and `openclaw` speak OpenAI already, so
they talk to the endpoint directly.

Nothing wally writes for a tool outlives the run. The variables go in the child
process. DeepSeek Harness is handed a settings document and a one-row patch in
a temp directory, both deleted when it exits; its own `$DSH_HOME` is never
written to, and the API key never enters either file, because the provider names
an environment variable and dsh resolves it per request.

OpenClaw and DeepSeek also get told the model's real context window and max
output, read from the console catalog. Hermes does not: it takes a
context-window hint from exactly one place, `model.context_length` (or a
`custom_providers` entry) in `~/.hermes/config.yaml`, and there is no
path-override env var, no CLI flag, and no way to swap in a second config
file without swapping in a second Hermes — `HERMES_HOME` governs the whole
tree, so pointing it elsewhere for the run would cost the person their
SOUL.md, sessions and skills to deliver one field. wally will not write to
`~/.hermes/config.yaml` either. So it prints the real number instead: `wally
hermes -m <model>` says how many tokens the model actually supports and names
the `model.context_length` line to add if you want Hermes to budget the
session against the full window rather than its own guess. `wally deepseek` opens
its web ui; `wally deepseek "fix the failing test"` runs its headless profile
instead. OpenClaw's config is a copy of your own `~/.openclaw/openclaw.json`
with one provider added, written to a temp file and deleted when the tool exits.
Your file is never touched. It is a copy rather than a fresh document because
`OPENCLAW_CONFIG_PATH` replaces the whole thing: a bare provider block would
drop your agents, your gateway token and the flag that says onboarding is done,
so OpenClaw would run its wizard on every launch. wally also pins
`OPENCLAW_STATE_DIR` to where your state already lives, because OpenClaw
otherwise takes the state directory from the config file's own folder.

The translator keeps its connections to the model endpoint open between
requests, one per stream in flight, so a turn does not start with a new TLS
handshake — against the hosted endpoint that handshake was measured at about
half a second, and an agent makes several requests per turn. A connection that
sat idle long enough for the far side to drop it (a long build, a walk away
from the desk) is tried once more on a fresh one, only when nothing had come
back yet; a request that has started answering is never repeated. So the first
request after a long pause pays one handshake, and the rest of the session does
not.

When the tool stops listening part-way through an answer — Esc in Claude Code,
the app quitting — the translator notices within a tenth of a second rather
than at the next chunk it fails to deliver, and asks the console to stop that
request by name, so the model stops generating an answer nobody will read and
the session stops paying for it. The name is the request id the endpoint sends
with its first token, so a request abandoned while the model is still reading
the prompt is stopped the moment that first token arrives, and nothing after it
is passed on. When the tool quits, `wally` sends any cancel still queued before
it returns. Each cancel is a line in `shim.log` under the state directory
(`~/.local/state/runanywhere/`, or `$XDG_STATE_HOME/runanywhere/`) — never the
tool's terminal. A local model needs none of this: the dropped connection is
enough.

## Local models

Download a model once, then pass its ID to a harness. No account login is needed:

```bash
wally models pull qwen3-4b-instruct-2507 --engine llamacpp
wally opencode -m qwen3-4b-instruct-2507
wally deepseek -m qwen3-4b-instruct-2507 "explain this project"

# Apple Silicon: the same certified model through MLX
wally models pull mlx-qwen3-4b-instruct-2507
wally opencode -m mlx-qwen3-4b-instruct-2507
```

Wally starts the SDK's OpenAI-compatible server on a free `127.0.0.1` port,
loads the selected local model, and hands that endpoint to the tool. The server
stops when the tool exits. Each session serves one model; its configured context
and bounded output budget are included in the tool configuration. Local coding
harnesses are intentionally gated to this certified Qwen3 4B artifact. Other
local models are rejected because their tool calls and long-context behavior
have not been certified.

When the certified model is missing, an interactive harness command asks
whether to download it and continues launching after the pull completes. The
default is No. Non-interactive commands never wait for input; they print the
equivalent `wally models pull <id>` command and exit.

If you use a separate model directory, pass the same `--home` for downloading
and launching, for example
`wally --home /path/to/storage opencode -m qwen3-4b-instruct-2507`.
An incomplete download reports how to finish the pull. To force a hosted model
in OpenCode even when a local copy exists, use `wally opencode --cloud -m <id>`.

`wally opencode --help` shows Wally's launch options. Use
`wally opencode -- --help` to ask OpenCode itself for help.

## Hosted models

A model you have not downloaded can still answer, if the console serves it:

```bash
wally account login
wally account whoami
wally run gemma-4-31b-it "why is the sky blue"
```

`wally account login` opens the console in a browser and waits for you to
approve the machine. `wally account logout` deletes the session.

Where the credential is kept depends on the platform, and `WALLY_PROFILE_DIR`
moves it anywhere:

| | Path |
|---|---|
| macOS, Linux | `$XDG_CONFIG_HOME/wally/credentials.json`, or `~/.config/wally` when unset |
| Windows | `%LOCALAPPDATA%\RunAnywhere\Wally\credentials.dat`, encrypted with DPAPI |

`WALLY_CONSOLE_URL` points the CLI at a console API other than the default, and
`WALLY_CONSOLE_WEB_URL` at the page that approves the sign-in. Those are two
different hosts; see [AGENTS.md](../AGENTS.md).

`wally auth login`, which signed a device in with an API key rather than a
browser, is not registered in this release; the browser flow above is the only
way in.

