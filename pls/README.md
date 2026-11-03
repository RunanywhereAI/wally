# PLS — hybrid router

A loopback router that exposes one model id and two lanes:

| Path | Lane |
|---|---|
| `/v1/...` | cloud |
| `/local/v1/...` | local MLX model, result summarised |
| `/local-only/v1/...` | local MLX model, result withheld |

`wally` starts it, points the harness at it, and stops it on exit. The local
model server is spawned by the router and killed with it.

## Running

```
wally deepseek --hybrid -m <local-model> --cloud-model <cloud-model> [prompt]
```

`wally pls -m <model-directory>` runs the router alone, for development.

## Subscription gate

The router asks wally for the account's entitlement over a unix socket at
`~/.config/wally/pls/entitlement.sock` (mode `0600`). Without a subscription
every request goes to the cloud and the local model is never loaded. The
answer is cached five seconds; an unreachable socket keeps the last answer, so
a lost wally fails closed to the cloud.

Premium accounts get the mode file and the two lanes.

## Debug console (temporary)

Set `PLS_DEBUG=1` and run `wally pls`. Open `<router url>/debug` and watch every
request, its lane, its reason and every file's PII counts live. `/debug/state`
holds the backlog, `/debug/events` is the stream. Remove once the MVP ships.

## PII

Every request is scanned for emails, phone numbers, API keys, IPv4 addresses and
IBANs. Counts are attributed to the file path the text mentions and travel to
the cloud in `x-pls-pii`, plus `/stats`. Only counts leave the machine, never
the values.

## Mode

`~/.config/wally/pls/config.json` is re-read on every request:

```json
{ "mode": "hybrid", "max_turns": 20 }
```

- `cloud` — everything goes to the cloud, the local model is never loaded.
- `hybrid` — the parent goes to the cloud, subagents go local.
- `local` — everything goes local.

Missing or unparseable falls back to `hybrid` and 20 turns.

## Files

```
~/.config/wally/pls/pls.py          override, wins over the copy beside the binary
~/.config/wally/pls/config.json     mode and turn cap
~/.config/wally/pls/venv/           built once from requirements.lock
~/.config/wally/pls/pls.pid         swept on the next start
```

## Requirements

`uv` on `PATH` or beside the binary. The first launch builds the venv
(`mlx-vlm`, `fastapi`, `uvicorn`, `httpx`) — about 1–2 GB.

## Limits

Text only, one concurrent local request (queue depth 8, then overflow to the
cloud), no auth on the port, one global mode.
