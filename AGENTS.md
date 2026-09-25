# AGENTS.md — Wally

> **`AGENTS.md` is the real file.** `CLAUDE.md` is a committed symlink to it.
> Editing either name edits the same bytes. `bash scripts/ci/check-agents-sync.sh --fix`
> recreates the link and mirrors `.claude/skills/` → `.agents/skills/`. CI fails
> if the pair drifts.

This repository is the official `wally` product CLI. The application is a Rust
crate (`Cargo.toml`); it consumes a **packaged C++ desktop kit** via CMake's
`find_package(RunAnywhere)`. CMake stays the build entry (`cmake -B build && cmake
--build build && ctest --test-dir build`) and the owner of the kit — discovery,
pins, link closure, runtime staging, the Swift MLX host — and runs `cargo` for
the application code. It does not `add_subdirectory` or FetchContent the SDK,
and it does not compile llama.cpp / Sherpa / ONNX / MLX from source.

Pin: `cmake/sdk-pin.cmake`. Prefix: `-DCMAKE_PREFIX_PATH=` or `-DWALLY_SDK_KIT=`.
Pointing `WALLY_SDK_DIR` at SDK **source** is a configure error.

`BEST_PRACTISES.md` / `BEST_PRACTICES.md` (if present) is a local playbook and
must stay gitignored. The rules below are the subset that applies to this CLI.

## Ownership model

```text
argv / flags / env
  -> src/commands/cmd_*.rs      thin: parse → bootstrap() → one rac_* → render
  -> C++ desktop kit            catalog, download, lifecycle, generate, serve
  -> engines (in the kit)       llama.cpp, Sherpa, ONNX, MLX (Apple host)
```

The kit owns truth: models, backends, proto contracts, download, inference.
The CLI renders and interacts. If a command is composing a multi-step bootstrap,
hardcoding an engine name, or post-processing model output, that is a bug in the
SDK — fix it there, then consume a new kit.

## Layering

- Command modules stay thin. Business rules do not live in command callbacks
  (`src/cli/mod.rs`'s CLI11-compatible builder), Swift, or the REPL.
- **Proto is the SOT.** `crate::io::proto::v1::*` (prost) is generated at build
  time straight from the kit's own `.proto` files (`build.rs`, via `protox`),
  after checking the kit's `SCHEMA_LOCK` hash against the pin in
  `versions.toml`. Wally never runs `protoc`, and nothing generated is
  committed. Parse `rac_*` byte buffers with `io::proto` (`ProtoBuffer`,
  `parse_proto_buffer`) into `v1::*`.
- Typed contracts at every boundary: the CLI parser → proto messages → `rac_*`
  → stdout. No parallel hand-written enums for values that exist in
  `idl/*.proto`.
- Structured errors. Machine-readable codes from the ABI; human text on stderr.
  Results on stdout. `--json` prints exactly one document on stdout.
- Never log API keys, tokens, or Authorization headers. Status/progress go to
  stderr.

### P0 contract-first network rule

Protobuf remains the source of truth for the packaged SDK/ABI boundary. For a
direct HTTP API owned by RunAnywhere—especially Wally auth, account, billing,
catalog, usage, and inference—the source of truth is the service's pinned
OpenAPI 3.1 artifact. Add the operation to that contract first, then consume a
generated binding (or a mechanically verified thin adapter) from the exact
contract hash.

- Every operation has a stable `operationId` and named request, response,
  error, parameter, and streaming-event schemas.
- No command may invent JSON with string-built paths, ad-hoc objects,
  untyped arrays, duplicated enums, or unchecked response parsing. Closed
  values are generated enums; variants are discriminated unions; owned objects
  are closed; IDs and scalar domains carry their contract constraints.
- The transport may move opaque bytes for the SDK, but business code must never
  treat opaque JSON as a typed result. SSE/OpenAI streaming adapters use
  generated event/chunk types plus conformance tests.
- Pin the OpenAPI artifact hash beside `SCHEMA_LOCK`. Contract, binding, CLI
  adapter, fixtures, and drift/conformance tests land atomically; CI fails when
  any one is stale.

Do not wrap protobuf in OpenAPI merely to change protocol names. When Wally only
transports SDK-owned bytes, protobuf generation and `SCHEMA_LOCK` satisfy this
rule. When Wally directly owns an HTTP call, the OpenAPI requirement applies.

The console's ten CLI calls (`/auth/cli/{start,poll,refresh,revoke}`, `/v1/me`,
`/v1/cli/usage`, `/v1/cli/usage/requests`, `/v1/models`, `/v1/models/catalog`,
`/v1/requests/{request_id}/cancel`) follow this.
`contracts/wally-cli-v1.openapi.json` is the pinned artifact, extracted from
InferenceInfra's `control-plane-v1.openapi.json` by
`contracts/extract-cli-contract.py`. `contracts/generate_console_binding.py`
turns it into `src/account/console_contract.rs` (typed requests and responses,
DO NOT EDIT), which `console.rs` uses instead of hand-built JSON. Requests
serialize strictly; responses read tolerantly (a missing field defaults, a wrong
type or unknown enum value still fails) so the CLI survives a server that lags
the contract. One field is exempt: the usage export's `provider` is a label the
CLI only reports, so `console.rs` lifts a value the binding does not know out
of the body before parsing and carries it as text instead of failing the page.
`test_wally_contract` and `python3 contracts/sync_from_inferenceinfra.py
--check` fail the build if the header, the pin, and the artifact drift.

To re-vendor from an InferenceInfra checkout (records the source commit on the
extract):

```bash
python3 contracts/sync_from_inferenceinfra.py --from /path/to/InferenceInfra
```

`--check` without `--from` is hermetic. Freshness against InferenceInfra HEAD
is enforced on the InferenceInfra PR (`consumer-impact`); this repo cannot
read that private source from CI.

A sync rewrites both generated files, so it refuses to start when either one
has uncommitted changes rather than destroying them; commit or stash first, or
pass `--force`. The same all-or-nothing rule applies to provenance: the
extractor takes `--source-commit` and `--source-branch` together or not at all,
because half a stamp writes a pin that cannot pass `--check`.

Consistency with the SDK is `idl/SCHEMA_LOCK`, copied into the kit as
`share/runanywhere/SCHEMA_LOCK` and pinned here as `WALLY_PINNED_IDL_SCHEMA_SHA256`.
Configure fails if the kit's lock does not match (`cmake/RunAnywhereSDK.cmake`).
When the schema changes, consume a new kit and bump the pin — never regenerate
headers locally.

`RunAnywhere::commons` applies `google=runanywhere_internal` when the kit was
built with namespace isolation. Never `find_package(Protobuf)` against Homebrew.

## Command surface

Dual grammar: spec namespaces (`llm generate`, `models download`) plus the
terminal alias `run`. Model verbs (`list`/`pull`/`rm`/`show`/`default`) live
under `models` only — no top-level shortcut. One `configure_*` wires both. See
`src/commands/mod.rs`.

Do not reintroduce FetchContent of the SDK, a second inference backend tree,
or a retired MetalRT / hardcoded catalog.

## Signing in to a console

`wally login` is a device flow, the same shape `gh auth login` uses. The terminal
asks for a code, a browser the person already trusts approves it, and the
terminal collects a key. No password ever reaches the CLI.

The four endpoints it calls are **not ours to rename**: an installed binary
talks to whatever the console deploys, so a field or path change breaks every
copy in the wild. They are `POST /auth/cli/start`, `/auth/cli/poll`,
`/auth/cli/refresh`, and `GET /v1/me`. The console side has a test that reads
`src/account/console.rs` directly and fails if the two drift.

Two secrets do different jobs. `request_code` is public and names the attempt;
`poll_secret` proves the process collecting the grant is the one that started
it. The console stores only a hash of the second.

Two hosts, and they are not the same deployment. The API is the control plane
and serves all four endpoints above plus `/v1/cli/*`; the web console is only
the page a person approves the sign-in on.

| | Default | Override |
|---|---|---|
| API | `https://inference.runanywhere.ai` | `WALLY_CONSOLE_URL` |
| Approval page | `https://console.runanywhere.ai` | `WALLY_CONSOLE_WEB_URL` |

Both defaults live together in `src/account/credentials.rs` so they cannot
drift apart, and `TrustedBrowserOrigins()` is what pairs them: with no override
it trusts the deployed console, and for any other API origin it trusts only
that origin. Pointing at a local dev console needs `WALLY_CONSOLE_URL` set
explicitly, e.g. `WALLY_CONSOLE_URL=http://localhost:8080`.

The API URL may carry a path, because the deployed development console is one:
`https://inference.runanywhere.ai/api-dev`, where the load balancer strips the
prefix and forwards to the dev control plane. Every endpoint is appended to
whatever is configured, so the prefix follows the whole flow. Its approval page
is on Railway rather than that host, so dev also needs
`WALLY_CONSOLE_WEB_URL=https://runanywhere-frontend-development.up.railway.app`.

Do not reach for the dev backend's own Cloud Run hostname instead. It answers,
but it is behind the load balancer, so `/v1/chat/completions` lands on the
control plane rather than the gateway and returns 502 — a route no installed
binary can produce, and a day lost to debugging it.

`WALLY_PROFILE_DIR` moves the credential file, which is what lets several
accounts share one machine.

The credential is a normal API key with the customer's credit behind it. Treat
it as one: it goes in the profile file at `0600` and nowhere else, and it is
never logged.

## Building against the SDK

`WALLY_SDK_KIT` points at a built kit, not at SDK source. `cmake/sdk-pin.cmake`
pins the IDL version and its hash; a mismatch is a hard error and the fix is to
consume a matching kit or bump the pin, **never to run protoc**. CMake hands
`cargo` the kit's link line through the file API reply of a configured-but-
never-built probe target (`cmake/WallyRust.cmake`), so every binary `cargo`
links carries the same kit closure a CMake executable had.

Two binaries come out of a build. `wally-cxx` is the plain `cargo`-built CLI.
`wally` is the same crate's static library (`src/lib.rs`) plus the MLX backend
linked by the Swift host, and it only builds when `WALLY_SDK_SWIFT_PATH` names
an SDK checkout with the Swift tree. Ship `wally`.

MLX resolves its Metal shaders from `mlx-swift_Cmlx.bundle` beside the
executable. Copy the binary somewhere on its own and MLX silently fails to
register, so an install puts both together and points a wrapper at them.

## Configuration and secrets

- Read environment in one place (`GlobalOptions` / `bootstrap()`).
- Safe local default: `--environment development` (keyless). Production requires
  an API key + https. Tests must not need real keys or network.
- Secrets stay in GitHub Actions secrets / the user's env, never in source,
  formula files, or skill docs.

## Tests

Hermetic unit tests (`tests/test_wally_unit.rs`, one sub-file per owner under
`tests/test_wally_unit/`, plus `tests/cli_golden.rs` replaying `tests/golden/`
byte-exact): no models, no network, no real keys. `ctest --test-dir build` runs
`cargo test` and is the default CI bar. `scripts/test/test-ledger.py` checks
that every C++ test case named in `tests/cpp_case_ledger.txt` exists as a
same-named Rust `#[test] fn`.

Smoke / e2e (`scripts/test/smoke.sh`, `scripts/test/e2e.sh`) prove the product promise
against a **pinned kit**, not SDK source. `scripts/test/e2e-modalities.sh` (called
from `e2e.sh`) runs optional round-trips **by modality** — LLM, STT, TTS, VLM,
embed, image, VAD, rerank, segment — and never requires `--engine`. Discover
models via `WALLY_E2E_<MOD>` (path or catalog id), `WALLY_E2E_MODEL_ROOTS`, or
`WALLY_E2E_AUTO=1`. Legacy `WALLY_E2E_MODEL` / `WALLY_E2E_MLX_MODEL` /
`WALLY_E2E_NEURT_MODEL` / `WALLY_E2E_QHEXRT_MODEL` still map onto those
primitives. Public CI leaves every knob unset (skip). There is one CLI named
`wally`. On Apple, `cmake --build` links the Swift MLX host as `build/wally`
(llama.cpp + ONNX + Sherpa + MLX). Windows is `build/wally.exe` (no MLX).
`wally-cxx` is an Apple compile artifact, not the product. Full MLX model
smoke: `scripts/test/smoke-mlx.sh`.

## CI

Minimum: install the pinned Rust toolchain (`rust-toolchain.toml`, held to
`versions.toml [toolchain] rust` by `scripts/ci/check-versions.py`) → fetch the
pinned kit (`scripts/build/fetch-kit.sh`) → configure → build → ctest → smoke.
Fail on pin / schema mismatch. `cargo fmt --check`, `cargo clippy --all-targets
-- -D warnings`, `python3 scripts/test/test-ledger.py` and (macOS)
`scripts/build/gen-sys-bindings.sh --check` gate every PR.
`scripts/ci/check-agents-sync.sh` fails the PR when `CLAUDE.md` is not a
symlink to `AGENTS.md`, or when `.claude/skills` and `.agents/skills` differ.

Linux bottles are not a v1 merge blocker. Windows x64 and macOS arm64 are.

## Build

```bash
cmake -B build -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_PREFIX_PATH=/path/to/cpp-desktop-<os>-<arch>
cmake --build build -j "$(sysctl -n hw.logicalcpu)"
ctest --test-dir build --output-on-failure
# Apple: ./build/wally   Windows: ./build/wally.exe
bash scripts/test/e2e.sh ./build/wally
```

CMake resolves the kit and runs `cargo build --release` for the crate
(`cmake/WallyRust.cmake`); `ctest` runs `cargo test`. `rustup` (reading
`rust-toolchain.toml`) must already have the pinned toolchain installed.

On Apple Silicon, `cmake --build` produces `build/wally` (Swift host wrapping
`wally_run_main`). Users never run `wally-cxx`; that name exists only so CMake
cannot overwrite the product binary. Independent clones set
`WALLY_SDK_SWIFT_PATH` to a runanywhere-sdks checkout (CI does this). Nested
`EXTERNAL/Wally` finds `../../Package.swift` automatically. Disable the host
with `-DWALLY_APPLE_MLX_HOST=OFF` only for a fast loop that skips the Swift
build (`WALLY_APPLE_CXX_BINARY=ON` then gets the plain `cargo` binary at
`build/wally-cxx`).

NeuRT and QHexRT are optional private packs (`NEURUN_TOKEN`), never required to
configure or link the public bottle. QHexRT is not shipped for Windows x64.

## Skills

Canonical tree: `.claude/skills/` (Claude Code). Mirror: `.agents/skills/`
(Cursor / Codex). Edit the canonical tree, then
`bash scripts/ci/check-agents-sync.sh --fix`. Never hand-edit the mirror.

| Skill | When |
|---|---|
| `wally-architecture` | New commands, layering, proto, "where does this logic go?" |
| `wally-kit-pin` | Bump `cmake/sdk-pin.cmake`, consume a new kit |
| `wally-e2e` | macOS / Windows verification against a kit |
| `wally-release` | Cut a product release (independent of SDK version) |

## Definition of done

- Command is parse → `bootstrap()` → one `rac_*` → render.
- Proto types from the kit; pin matches `SCHEMA_LOCK`.
- Errors structured; no secrets on the log line.
- Unit tests hermetic; smoke/e2e green on the platforms you touched.
- `check-agents-sync.sh` passes.
- Docs name any mock, scaffold, or deferred work honestly (Linux bottle, NeuRT).
