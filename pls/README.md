# PLS — hybrid router

A loopback router with one endpoint, `/v1/chat/completions`, for the main
chat and every subagent. `/v1/models` lists two ids, `Hybrid-Wally-0.1` for
the parent and `Hybrid-Wally-0.1-local` for subagents; the second only tells
the harness to compact at the local model's window. Each entry carries
`context_length` and `max_completion_tokens` (Hermes reads them). Routing reads
the request:

| Kind | Rule | `hybrid` | `cloud` |
|---|---|---|---|
| subagent | model is `Hybrid-Wally-0.1-local`, or header `x-parent-session-id` (opencode), or first prompt starts `pls-task:` | local model, never decided | cloud, marker stripped |
| side | no `tools` (session title) | cloud; privacy masks private prompts | cloud as sent |
| main | everything else | decision, rewrite, PII redact, guard, cloud | cloud as sent |

Only the router writes `pls-task:`. Ids look like `a1b2c3-t1`: the prefix
names the session (its first prompt) and numbers start at t1 in each one.
Prompts with no reply between them are one task, and only the unanswered
prompt carries a task line. It stamps every spawn call in a main reply
(the harness's own tool, see Harnesses) with the pending task's id, or with a new ad hoc task (`local_only`
under privacy, `summary` under cost). Any other path, `/local/v1` included,
is 404.

`wally` starts it, points the harness at it, and stops it on exit. The local
model server is spawned by the router and killed with it.

## Running

```
wally <harness> --hybrid [--policy privacy|cost] -m <local-model> --cloud-model <cloud-model>
```

`<harness>` is `deepseek`, `opencode`, `openwork`, `hermes` or `openclaw`.
Each launch is temporary: the harness gets its config for one run (an
environment variable, a temp file or a temp directory) and none of the
person's own harness files change. Every request must carry the launch's
`x-pls-token`; each harness config sends it.

## Harnesses

`wally` writes the harness's profile into `config.json` as `harness`. It
names the spawn tool the router stamps, guards and denies to children, and
the text the harness adds around a child's prompt.

| Profile | Spawn tool and task field | Child loses | Prompt cleanup | Launch |
|---|---|---|---|---|
| `deepseek` | `subagent.prompt` | `subagent`, `subagent_fork`, `send_message` | dsh's "Your parent agent id…" note | `--patch` overlay |
| `opencode-v2` | `subagent.prompt` (+ `agent: general`) | `subagent`, `task` | "You are a subagent spawned by another session." line | `OPENCODE_CONFIG_CONTENT`, `--standalone` |
| `opencode-v1` (also OpenWork) | `task.prompt` (+ `subagent_type: general`) | `subagent`, `task` | none | `OPENCODE_CONFIG_CONTENT` |
| `hermes` | `delegate_task.tasks[].goal` | `delegate_task`, `send_message` | none | `HERMES_MANAGED_DIR` temp overlay |
| `openclaw` | `sessions_spawn.task` | `sessions_spawn`, `subagents` | none; its context block is kept | temp `openclaw.json` |

The synthesised call fills the fields each harness requires. An OpenClaw spawn
that names a `model` has it removed, so the parent cannot send a child to the
cloud. Hermes `delegate_task` with `action: list|steer|stop` starts nothing and
is left alone. Under privacy the parent keeps its spawn tool and the tools that
collect results (OpenClaw's `subagents`, `sessions_*`, `agents_wait`).

Harness-written `user` messages are skipped when routing: dsh's
`<system-reminder>` and runtime snapshot and its "Agent … sent a message"
reports, and OpenClaw's internal context blocks. The Hermes and OpenClaw
patterns come from source, not yet from a loopback capture.

## Policies

Each new user prompt goes to a local decision model (`mlx-clef-flash-9b-4bit`,
served by wally on its socket as `POST /decide`). Its answer
decides what the cloud parent is told, appended to that same user message:

| Policy | Decision says | The cloud sees | Subagent answer |
|---|---|---|---|
| `privacy` (default) | private | a placeholder and `pls-task: tN`, never the prompt | withheld (`local_only`) |
| `privacy` | not private | the prompt plus "launch a subagent, `pls-task: tN`" | summarised |
| `cost` | large task | the prompt plus "launch a subagent, `pls-task: tN`" | summarised |
| `cost` | small task | the prompt plus "do it yourself" | no subagent |

Under `privacy` the parent is offered only its spawn tool, and any other
tool call in its reply is dropped and asked again; after two tries the router
emits the subagent call itself. The `pls-task:` id is how the subagent side
finds its summary type and, for a private prompt, the real text.

If the decision model is unavailable, `privacy` treats every prompt as
private and `cost` lets the cloud do everything. Records live in
`tasks.json` (mode `0600`, no prompt text), so a resumed session stays masked.

`wally pls -m <model-directory>` runs the router alone, for development.

## Subagents

A subagent runs on the local model and cannot launch another: it loses its
profile's spawning and messaging tools under both policies, and its system
prompt says so once. A local reply that still calls one has the
call dropped (`child_spawn_blocked`). If the local model cannot start,
privacy withholds the subagent's answer and cost sends it to the cloud with
the marker stripped (`local_unavailable`).

## Socket

wally serves the router over a unix socket at `~/.config/wally/pls/pls.sock`
(mode `0600`, path in `PLS_SOCKET`): `POST /decide` for the decision model and
`GET /token` for the cloud token. Nothing is gated on a plan.

## Debug console (temporary)

Set `PLS_DEBUG=1` and run `wally pls`. Open `<router url>/debug` and watch every
request, its lane, its reason and every file's PII counts live. `/debug/state`
holds the backlog, `/debug/events` is the stream. Remove once the MVP ships.

## PII

Every request is scanned for emails, phone numbers, API keys, IPv4 addresses and
IBANs. Counts are attributed to the file path the text mentions and travel to
the cloud in `x-pls-pii`, plus `/stats`. So paths and counts leave the machine,
never the values: the cloud-bound messages are redacted to `[email]`, `[phone]`,
`[key]`, `[iban]` and `[ipv4]` before they are sent.

## Mode

`~/.config/wally/pls/config.json` is re-read on every request:

```json
{ "mode": "hybrid", "policy": "privacy", "max_turns": 20,
  "harness": "opencode-v2",
  "cloud_limits": { "context_length": 200000, "max_completion_tokens": 32000 } }
```

`harness` and `cloud_limits` are written by each launch; an unknown harness
reads as `deepseek`, and a missing limit as the local one.

- `cloud`: everything goes to the cloud exactly as the harness sent it, and
  the local model is never loaded. Switching a session here and back is safe.
- `hybrid`: main chat goes to the cloud under the policy, subagents go local.

Missing or unparseable falls back to `hybrid`, `privacy` and 20 turns.

## Files

```
~/.config/wally/pls/pls.py          override, wins over the copy beside the binary
~/.config/wally/pls/config.json     mode, policy and turn cap
~/.config/wally/pls/tasks.json      task records, no prompt text (0600)
~/.config/wally/pls/venv/           built once from requirements.lock
~/.config/wally/pls/pls.pid         swept on the next start
```

## Requirements

`uv` on `PATH` or beside the binary. The first launch builds the venv
(`mlx-vlm`, `fastapi`, `uvicorn`, `httpx`) — about 1–2 GB.

## Limits

Text only, one concurrent local request (queue depth 8, then overflow to the
cloud under `cost`; `privacy` waits instead), one global mode.
