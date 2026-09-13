# Editors, coding agents and hosted models

How `wally` wires each tool, and where a signed-in session lives.

## Editors and coding agents

One command points a tool at a model and starts it. There is nothing to
configure by hand:

```bash
wally claude-code -m qwen3-0.6b
wally clion -m models/gemma-4-31b-it
wally claude-desktop -m models/gemma-4-31b-it
```

The model can be one on this machine or one the console serves. Without `-m` the
tool starts the way you already have it configured, and wally wires nothing.

| Tool | How it is wired |
| --- | --- |
| `claude-code`, `opencode` | `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN` in the process |
| `claude-desktop` | a gateway profile in Claude Desktop's third party mode, covering the chat and Cowork tabs |
| `clion`, `rustrover` | AI Assistant's OpenAI-compatible provider, which works without a JetBrains AI subscription |

Two flags go with `-m`. `--serve` holds the endpoint open and prints it instead
of launching anything, which is how a tool nobody has taught wally about gets
wired up. `--restore` puts Claude Desktop or a JetBrains IDE back the way it was
and starts nothing; a normal run already undoes its own configuration when the
app quits, so this is for the run that was interrupted before it could.

The first `wally clion` on a machine takes a while, because it installs the AI
Assistant plugin headlessly before starting the IDE. Later runs are quick. That
endpoint sits on a fixed port rather than whatever happened to be free, because
the IDE reads the address once at startup out of a file wally writes beforehand,
and a port that moved would leave that file naming something dead.

Claude Code and Claude Desktop speak Anthropic's Messages API, while the models
wally serves speak OpenAI's, so a translator sits between them. It carries tool
definitions out, tool calls back, and the results of those calls out again,
which is what lets an agent on the far side run the tools it was given rather
than describe them. The JetBrains IDEs need no translator, because AI Assistant
speaks OpenAI already.

Both the translator and the JetBrains proxy keep their connections to the
model endpoint open between requests, one per stream in flight, so a turn does
not start with a new TLS handshake — against the hosted endpoint that handshake
was measured at about half a second, and an agent makes several requests per
turn. A connection that sat idle long enough for the far side to drop it (a
long build, a walk away from the desk) is tried once more on a fresh one, only
when nothing had come back yet; a request that has started answering is never
repeated. So the first request after a long pause pays one handshake, and the
rest of the session does not.

When the tool stops listening part-way through an answer — Esc in Claude Code,
the app quitting — both notice within a tenth of a second rather than at the
next chunk they fail to deliver, and ask the console to stop that request by
name, so the model stops generating an answer nobody will read and the session
stops paying for it. The name is the request id the endpoint sends with its
first token, so a request abandoned while the model is still reading the prompt
is stopped the moment that first token arrives, and nothing after it is passed
on. When the tool quits, `wally` sends any cancel still queued before it
returns (`--serve` is ended by Ctrl-C, which drops everything with it). Each
cancel is a line in `shim.log` under the state directory
(`~/.local/state/runanywhere/`, or `$XDG_STATE_HOME/runanywhere/`) for the
translator, and in the profile directory's `proxy-trace.log` for the JetBrains
proxy when it was started with `--verbose` -- never the tool's terminal. A
local model needs none of this: the dropped connection is enough.

## Hosted models

A model you have not downloaded can still answer, if the console serves it:

```bash
wally login
wally whoami
wally run models/gemma-4-31b-it "why is the sky blue"
```

`wally login` opens the console in a browser and waits for you to approve the
machine. `wally logout` deletes the session.

Where the credential is kept depends on the platform, and `WALLY_PROFILE_DIR`
moves it anywhere:

| | Path |
|---|---|
| macOS, Linux | `$XDG_CONFIG_HOME/wally/credentials.json`, or `~/.config/wally` when unset |
| Windows | `%LOCALAPPDATA%\RunAnywhere\Wally\credentials.dat`, encrypted with DPAPI |

`WALLY_CONSOLE_URL` points the CLI at a console API other than the default, and
`WALLY_CONSOLE_WEB_URL` at the page that approves the sign-in. Those are two
different hosts; see [AGENTS.md](../AGENTS.md).

This is separate from `wally auth login`, which signs a device in with an API
key rather than a browser. Most people want `wally login`.

