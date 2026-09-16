# The daemon

`wally serve` runs one local HTTP server. It is the only thing that talks to a
model, local or cloud; every first-party command and every coding harness is a
client of it.

## Address

Binds to `127.0.0.1:11500` by default. `WALLY_HOST` overrides it as
`host:port`. The base URL first-party clients use is `http://<addr>/v1`.

## Routes

| Route | Speaks | Behavior |
|---|---|---|
| `GET /healthz` | plain text | `200 ok` once the server is up |
| `POST /v1/chat/completions` | OpenAI | resolves the model, streams the upstream response back |
| `POST /v1/messages` | Anthropic | translated to chat/completions, same resolution and streaming |

`wally serve` prints its listening address to stdout and shuts down cleanly on
`SIGINT`/`SIGTERM`.

## How a request resolves

A request names a model. The daemon looks it up:

- **Installed on-device model:** started through the local engine and
  streamed from there. The engine link is not built into this binary yet
  (milestone 5 of the rewrite), so this path currently returns `501` with a
  message saying so, instead of pretending to run something.
- **Anything else:** treated as a cloud model. No stored session means `401`
  with a message pointing at `wally login`; a valid session gets proxied to
  Wally Cloud with the request's model name intact.

## Starting it without a foreground process

A client (`wally run`, `wally chat`, a harness launch) checks `GET /healthz`
first and starts the daemon itself if nothing answers, so you do not have to
run `wally serve` by hand before every command. Running it directly is for
watching its output, or for pointing an external tool at it deliberately.
