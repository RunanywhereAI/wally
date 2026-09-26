<img src="docs/assets/wally.gif" alt="Wally, the RunAnywhere mascot" width="120">

# Wally

[![Release](https://img.shields.io/github/v/release/RunanywhereAI/wally?label=release)](https://github.com/RunanywhereAI/wally/releases/latest)

**Run open models on your own machine, or hosted when the job outgrows it.**

One command to chat with a model, serve it as an API, or open a coding agent
on it. Hosted models bill against your RunAnywhere credit.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/demo-dark.gif">
  <img src="docs/assets/demo-light.gif" alt="wally run qwen3 answering a prompt in the terminal" width="100%">
</picture>

## Install

macOS on Apple Silicon (Intel Macs are not supported) and Linux on x86-64 with
glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+; no ARM build yet):

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

Windows (x64 and ARM64):

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex
```

## Run a model on your machine

You don't need an account or a key.

```bash
wally models pull qwen3     # download Qwen3 0.6B
wally run qwen3             # chat
wally run qwen3 "Hello"     # one answer and exit
wally serve qwen3           # OpenAI-compatible API on :8080 (macOS, Linux)
```

`wally models list --all` shows everything you can pull. Any Hugging Face GGUF
works too, by its full path:

```bash
wally models pull hf.co/Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf
```

Models live in `~/.local/share/runanywhere` (`--home` or `$RUNANYWHERE_HOME`
moves them). `wally models rm` frees the space.

## Use a local model in your coding agent

Qwen3 4B Instruct is the local model certified for coding agents: tested for
tool calls and long context. Download it once, then open any supported agent
on it. Pick the build for your machine:

```bash
# Apple Silicon (MLX, 2.2 GB)
wally models pull mlx-qwen3-4b-instruct-2507
wally opencode -m mlx-qwen3-4b-instruct-2507

# Windows and Linux (GGUF, 4 GB)
wally models pull qwen3-4b-instruct-2507
wally opencode -m qwen3-4b-instruct-2507
```

`claude-code`, `claude-desktop`, `hermes`, `openclaw` and `deepseek` take the
same `-m`. [EDITORS.md](docs/EDITORS.md) says how each one is wired. Other
local models are refused by the coding-agent commands.

## Use a hosted model

Sign in once, then pass a hosted model id. `glm-5.3-flash` is the default
coding model; the rest are listed in your RunAnywhere console.

```bash
wally account login
wally opencode --cloud -m glm-5.3-flash
wally claude-code -m glm-5.3-flash
wally account usage         # credit left, recent spend
```

Hosted models go through the coding-agent commands; `wally run` and
`wally serve` are local only. Hosted runs are billed per token against your
credit.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/harness-dark.gif">
  <img src="docs/assets/harness-light.gif" alt="OpenCode on the hosted GLM model fixing a bug through wally" width="100%">
</picture>

## Models at a glance

| Family | Pull |
|---|---|
| Qwen3, Qwen3.8 | `qwen3`, `qwen3-4b-instruct-2507` |
| Llama 3.2 | `llama-3.2-3b` |
| Gemma 4 | `gemma-4-e2b` |
| LFM2.5 | `lfm2.5-1.2b` |
| Granite 4 | `granite-4.1-3b` |
| SmolLM2 | `smollm2-135m` |

Prefix an id with `mlx-` for the Apple GPU build. [MODELS.md](docs/MODELS.md)
has the full catalog.

## Commands you'll use

| Chat | |
|---|---|
| `wally run <model> [prompt]` | chat, or one answer and exit |
| `wally serve <model>` | OpenAI-compatible API on :8080 |

| Models | |
|---|---|
| `wally models list [--all]` | models on this machine, or the whole catalog |
| `wally models pull` / `rm` | download or delete a model |
| `wally models show` | size, context window, files |
| `wally models default` | the model coding tools open with |

| Coding tools | |
|---|---|
| `wally opencode` / `claude-code` / `claude-desktop` | open the tool on a model with `-m`, hosted with `--cloud` |
| `wally hermes` / `openclaw` / `deepseek` | same |

| Account | |
|---|---|
| `wally account login` / `logout` / `whoami` | browser sign-in and session |
| `wally account usage` | credit left and recent spend |

| Wally | |
|---|---|
| `wally about` | versions, backends, paths |
| `wally update` | latest release |
| `wally uninstall` | remove wally, its models and its config |

`wally --help` and `wally <command> --help` cover the rest.

## Build from source

Wally is Rust, built through CMake against a prebuilt RunAnywhere C++ desktop
kit (not the SDK source tree). [CONTRIBUTING.md](CONTRIBUTING.md) has the steps.

## More

- [Releases](https://github.com/RunanywhereAI/wally/releases): every version, with checksums
- [Engines and platforms](docs/ENGINES.md): what runs where and how wally picks
- [Models](docs/MODELS.md): the full catalog
- [Editors and hosted models](docs/EDITORS.md): how each tool is wired, where your session lives
- [docs.runanywhere.ai](https://docs.runanywhere.ai)
- [Discord](https://discord.gg/N359FBbDVd)
- [Hugging Face](https://huggingface.co/runanywhere)

MIT. See [LICENSE](./LICENSE).
