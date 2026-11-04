# PLS — hybrid router

A loopback router that exposes one model id and two lanes:

| Path | Lane |
|---|---|
| `/v1/...` | cloud parent |
| `/local/v1/...` | local MLX subagent; its answer is summarised or withheld |

`wally` starts it, points the harness at it, and stops it on exit. The local
model server is spawned by the router and killed with it.

## Running

```
wally deepseek --hybrid [--policy privacy|cost] -m <local-model> --cloud-model <cloud-model> [prompt]
```

## Policies

Each new user prompt goes to a local decision model (`mlx-clef-flash-9b-4bit`,
served by wally on the entitlement socket as `POST /decide`). Its answer
decides what the cloud parent is told, appended to that same user message:

| Policy | Decision says | The cloud sees | Subagent answer |
|---|---|---|---|
| `privacy` (default) | private | a placeholder and `pls-task: tN`, never the prompt | withheld (`local_only`) |
| `privacy` | not private | the prompt plus "launch a subagent, `pls-task: tN`" | summarised |
| `cost` | large task | the prompt plus "launch a subagent, `pls-task: tN`" | summarised |
| `cost` | small task | the prompt plus "do it yourself" | no subagent |

Under `privacy` the parent is offered only the `subagent` tool, and any other
tool call in its reply is dropped and asked again; after two tries the router
emits the subagent call itself. The `pls-task:` id is how the subagent side
finds its summary type and, for a private prompt, the real text.

If the decision model is unavailable, `privacy` treats every prompt as
private and `cost` lets the cloud do everything. Records live in
`tasks.json` (mode `0600`, no prompt text), so a resumed session stays masked.

`wally pls -m <model-directory>` runs the router alone, for development.

## Subscription gate

The router asks wally for the account's entitlement over a unix socket at
`~/.config/wally/pls/entitlement.sock` (mode `0600`). Without a subscription
every request goes to the cloud and the local model is never loaded. The
answer is cached five seconds. An unreachable socket keeps the last answer, so
a premium account keeps the local lanes after wally goes away; only a last
answer of non-premium fails closed to the cloud.

Premium accounts get the mode file and the two lanes.

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
{ "mode": "hybrid", "policy": "privacy", "max_turns": 20 }
```

- `cloud`: everything goes to the cloud exactly as the harness sent it, and
  the local model is never loaded. Switching a session here and back is safe.
- `hybrid`: the parent goes to the cloud under the policy, subagents go local.

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
cloud), no auth on the port, one global mode.
