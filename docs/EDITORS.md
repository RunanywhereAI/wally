# Editors, coding agents and local or hosted models

How `wally` wires each tool, and where a signed-in session lives.

## Editors and coding agents

One command points a tool at a model and starts it. There is nothing to
configure by hand:

```bash
wally claude-code -m qwen3-4b-instruct-2507
wally codex -m qwen3-4b-instruct-2507
wally chatgpt -m qwen3-4b-instruct-2507
wally hermes -m qwen3-4b-instruct-2507
wally deepseek -m qwen3-4b-instruct-2507
wally openclaw -m qwen3-4b-instruct-2507
```

The model can be one on this machine or one the console serves. Without `-m` the
tool starts the way you already have it configured, and wally wires nothing.

| Tool | How it is wired |
| --- | --- |
| `claude-code` | `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` in the process |
| `codex` | `-c` overrides on its command line naming a `wally` provider at the translator, plus a model catalog file for the run; the loopback token is in `WALLY_CODEX_API_KEY`. It runs in a Codex home of wally's own, so its threads and settings stay apart from yours; nothing is copied in and `~/.codex` is never written |
| `chatgpt` | the ChatGPT app for macOS started as a second instance with its own app data and its own Codex home, whose `config.toml` holds the same settings `codex` passes as `-c` (one block wally rewrites per launch; the app's own entries are kept) and an API-key `auth.json` so no ChatGPT sign-in is asked for. Your own ChatGPT app and `~/.codex` are never touched. Quitting the app ends the run, and Ctrl-C quits the app |
| `opencode` | a temporary provider in `OPENCODE_CONFIG_CONTENT`, with the local or hosted endpoint and context limits |
| `claude-desktop` | a gateway profile in Claude Desktop's third party mode, covering the chat and Cowork tabs |
| `hermes` | `CUSTOM_BASE_URL`, `HERMES_INFERENCE_PROVIDER=custom`, the model in `HERMES_INFERENCE_MODEL`, and the key under the name its host gates on |
| `openclaw` | your own `openclaw.json` plus one provider, written for the run and named by `OPENCLAW_CONFIG_PATH` |
| `deepseek` | a `--patch` overlay on the argv that adds the provider to dsh's `llm-pi-ai` row; nothing enters `$DSH_HOME` |
| `prime-agent` | a temporary extension loaded with `-e` that registers the provider for the run, with the key in `RUNANYWHERE_API_KEY`; nothing enters `~/.prime/agent` |
| `command-code` | a `runanywhere-wally` provider merged into `~/.commandcode/providers.json` (it reads providers from nowhere else) and taken out when it exits: the original bytes go back if the file is unchanged, otherwise only that provider comes out. The file names `$WALLY_COMMAND_CODE_API_KEY` rather than holding the key. A local model, or a person with no Command Code sign-in, runs with `CMD_LOCAL_ONLY=1` (no Command Code gateway models, `/share`, `/usage` or server-side web tools); with no sign-in a placeholder `COMMAND_CODE_API_KEY` gets past Command Code's login screen. `--skip-onboarding --no-auto-update` are passed, `-t` is left to you. One run at a time (a lock in `~/.commandcode/`). Its own subcommands (`-- status`, `-- login`, …) run untouched; `-- acp` is wired, and there the editor picks the model |

Two flags go with `-m`. `--serve` holds the endpoint open and prints it instead
of launching anything, which is how a tool nobody has taught wally about gets
wired up. `--restore` puts Claude Desktop back the way it was and starts
nothing; a normal run already undoes its own configuration when the app quits,
so this is for the run that was interrupted before it could.

Claude Code and Claude Desktop speak Anthropic's Messages API, while the models
wally serves speak OpenAI's, so a translator sits between them. It carries tool
definitions out, tool calls back, and the results of those calls out again,
which is what lets an agent on the far side run the tools it was given rather
than describe them. Codex speaks only OpenAI's Responses API, so it gets a
translator too: Responses in, chat completions out, and the stream back as
Responses events, each text or argument fragment passed on as it arrives.
`opencode`, `hermes` and `openclaw` speak OpenAI's chat completions already, so
they talk to the endpoint directly.

The effort you pick in the tool reaches the model as `reasoning_effort`: Claude
Code's and Claude Desktop's effort setting, Claude Code's thinking switch
(off is `none`), Codex's reasoning level, whose picker lists `none` to
`max`, OpenCode's variants (`ctrl+t`, or `-m runanywhere/<model>#<level>`), and
DeepSeek Harness's Effort control. Which levels a model offers, and which one
it runs at when you pick none, come from the console's `/v1/models` `reasoning`
block, never from a list in wally: GLM 5.3 Flash low / high / max, DeepSeek
V4.1 Flash low / high / xhigh / max, Muse Spark minimal to xhigh, and MiMo only
on or off. OpenCode gets every level as a variant, a `none` variant where the
model can stop thinking, and the model's default as its starting level.
DeepSeek Harness shows the levels with "Provider default" first: its pi-ai
adapter takes one default for every model on a route and refuses a request
whose model lacks it, so wally leaves the default to the endpoint, which runs
the model's own. It offers no Off, because pi-ai sends the same value for Off
and for no choice at all. OpenClaw's `/think` (and `--thinking`) offers the
model's levels, `off` sends `none` where the model can stop thinking, and the
model's default is set for it unless your own config sets one
(`agents.defaults.models["runanywhere/<model>"].params.thinking`). Claude Code
starts each model at its default level (`--effort`, unless you pass your own),
and `/effort` changes it; Codex and the ChatGPT app list each model's levels
and start at its default.

Three tools keep their own default. Hermes takes any level with `--reasoning`
(`wally hermes -m <model> -- --reasoning high`) or `/reasoning`, and otherwise
runs at `agent.reasoning_effort` in `~/.hermes/config.yaml` (per model under
`agent.reasoning_overrides`), which wally does not write. Claude Desktop shows
each model's levels up to its highest and starts at the app's own default.
Prime Agent is given no reasoning levels yet: its models run at the
endpoint's default, and a level has to be named in Prime Agent itself.
Command Code is given each model's levels from `low` to `max` as
`reasoningEfforts` (`--effort`, or `/model`), and starts at its own default.
The endpoint attributes its traffic as `unknown` until the console learns a
`command_code` harness.
Wally ends when Command Code ends: Ctrl-C reaches both, and wally cleans up
after the tool exits. A signal sent to wally alone leaves it waiting for the
tool; signal the process group to stop both. A `kill -9` leaves the provider in
`providers.json` until the next `wally command-code` drops it, and a bare
`cmd -c` afterwards cannot find that provider for a resumed session.

Codex is told the model's context window through the catalog file, so its
context meter and auto-compaction work against the real limit. Its hosted web
search, WebSocket transport and remote compaction are switched off for the run,
because a chat-completions endpoint has none of them; Codex compacts locally
instead. Its `apply_patch` tool reaches the model as a function that takes the
patch text, and the call goes back to Codex in the form it expects. Reasoning
shows as Codex's reasoning summary and is not sent back on the next turn, the
same as the Claude path. The endpoint attributes Codex traffic as `unknown`
until the console learns a `codex` harness.

The model's reasoning shows up as a thinking block with an empty signature,
because these models can't sign it. Claude Code accepts that, and wally drops
the block again when Claude Code sends it back. Cache reads show up as
`cache_read_input_tokens`, taken out of `input_tokens` so nothing is counted
twice. Cache writes aren't reported, because the endpoint doesn't report them.
OpenCode, OpenClaw and Prime Agent get the catalog's cache-read price, so their
cost display charges a cached prompt at the cached rate.

Nothing wally writes for a tool outlives the run. The variables go in the child
process. DeepSeek Harness is handed a patch in a temp directory, deleted when it
exits; its own `$DSH_HOME` is never written to, and the API key never enters
the file, because the provider names
an environment variable and dsh resolves it per request.

OpenClaw, DeepSeek and Prime Agent also get told the model's real context window and max
output, read from the console catalog. Hermes does not: it takes a
context-window hint from exactly one place, `model.context_length` (or a
`custom_providers` entry) in `~/.hermes/config.yaml`, and there is no
path-override env var, no CLI flag, and no way to swap in a second config
file without swapping in a second Hermes. `HERMES_HOME` governs the whole
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
handshake. Against the hosted endpoint that handshake was measured at about
half a second, and an agent makes several requests per turn. A connection that
sat idle long enough for the far side to drop it (a long build, a walk away
from the desk) is tried once more on a fresh one, only when nothing had come
back yet; a request that has started answering is never repeated. So the first
request after a long pause pays one handshake, and the rest of the session does
not.

When the tool stops listening part-way through an answer (Esc in Claude Code,
the app quitting), the translator notices within a tenth of a second rather
than at the next chunk it fails to deliver, and asks the console to stop that
request by name, so the model stops generating an answer nobody will read and
the session stops paying for it. The name is the request id the endpoint sends
with its first token, so a request abandoned while the model is still reading
the prompt is stopped the moment that first token arrives, and nothing after it
is passed on. When the tool quits, `wally` sends any cancel still queued before
it returns. Each cancel is a line in `shim.log` under the state directory
(`~/.local/state/runanywhere/`, or `$XDG_STATE_HOME/runanywhere/`; on Windows
`%LOCALAPPDATA%\RunAnywhere\state\`), never the tool's terminal. A local
model needs none of this: the dropped connection is enough.

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

A model you have not downloaded can still answer through a coding tool, if the
console serves it. `wally run` and `wally serve` stay local.

```bash
wally account login
wally account whoami
wally opencode --cloud -m glm-5.3-flash
wally claude-code -m glm-5.3-flash
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

### Reasoning effort without wally

A tool pointed at `https://inference.runanywhere.ai/v1` with an API key, no
wally in between, sets a model's reasoning with the request field
`reasoning_effort`. `GET /v1/models` says what each model takes, in its
`reasoning` block:

```json
"reasoning": {
  "levels": ["low", "high", "xhigh", "max"],
  "default_level": "high",
  "thinking_off": true
}
```

`levels` are the values `reasoning_effort` accepts, `default_level` is what a
request that sends none runs at, and `thinking_off: true` means `none` turns
thinking off (a model with `false` always thinks, and `none` is refused).

OpenCode needs each level declared as a variant, or it offers no choice. In
`opencode.json`:

```json
{
  "provider": {
    "runanywhere": {
      "npm": "@ai-sdk/openai-compatible",
      "name": "RunAnywhere",
      "options": {
        "baseURL": "https://inference.runanywhere.ai/v1",
        "apiKey": "{env:RUNANYWHERE_API_KEY}"
      },
      "models": {
        "deepseek-v4.1-flash": {
          "reasoning": true,
          "options": { "reasoningEffort": "high" },
          "variants": {
            "low": { "reasoningEffort": "low" },
            "high": { "reasoningEffort": "high" },
            "xhigh": { "reasoningEffort": "xhigh" },
            "max": { "reasoningEffort": "max" },
            "none": { "reasoningEffort": "none" }
          }
        }
      }
    }
  }
}
```

`options.reasoningEffort` is the starting level and a variant overrides it:
cycle with `ctrl+t`, or name one with `opencode run -m
runanywhere/deepseek-v4.1-flash#max`. Repeat the block for each model with that
model's `levels`, and leave `none` out where `thinking_off` is false. (A
`#variant` in the config's own `model` key is ignored, which is why the
default goes in `options`.)

DeepSeek Harness takes the levels as `reasoningEfforts` on each model of its
`llm-pi-ai` provider; each key is a level it shows, and its value is what it
sends:

```yaml
- name: '@deepseek-ai/dsh-llm-pi-ai'
  config:
    providers:
      runanywhere:
        api: openai-completions
        baseURL: https://inference.runanywhere.ai/v1
        apiKeyEnv: RUNANYWHERE_API_KEY
        compat:
          supportsDeveloperRole: false
        models:
          - id: deepseek-v4.1-flash
            reasoningEfforts: { low: low, high: high, xhigh: xhigh, max: max }
```

Any other OpenAI-compatible client sends the field itself, for example
`"reasoning_effort": "max"` beside `model` and `messages`, or
`extra_body={"reasoning_effort": "max"}` in the OpenAI Python SDK.

