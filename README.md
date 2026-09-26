<img src="docs/assets/wally.gif" alt="Wally, the RunAnywhere mascot" width="120">

# Wally

**Run open models on your own machine, or hosted when the job outgrows it.**

One terminal command to chat with a model. Local models stay on your device;
hosted ones bill against your RunAnywhere credit.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/demo-dark.gif">
  <img src="docs/assets/demo-light.gif" alt="wally run qwen3 answering a prompt in the terminal" width="100%">
</picture>

## Install

macOS (Apple Silicon) and Linux (x86-64 with glibc 2.35 or newer: Ubuntu 22.04+,
Debian 12+):

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

Windows (x64 and ARM64):

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex
```

## Run a model on your machine

You don't need an account or a key, and nothing leaves the machine.

```bash
wally models pull qwen3     # download Qwen3 0.6B
wally run qwen3             # chat
wally run qwen3 "Hello"     # one answer and exit
wally serve qwen3           # OpenAI-compatible API on :8080 (macOS, Linux)
```

`wally models list --all` shows everything you can pull. Any Hugging Face GGUF
works too:

```bash
wally models pull hf.co/Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf
```

## Use a local model in your coding agent

Qwen3 4B is the one local model certified for coding agents, meaning it's
tested for tool calls and long context. Download it once, then launch any
supported agent against it:

```bash
wally models pull qwen3-4b-instruct-2507
wally opencode -m qwen3-4b-instruct-2507
wally claude-code -m qwen3-4b-instruct-2507
```

`claude-desktop`, `deepseek`, `hermes` and `openclaw` are wired the same way.
Other local models are rejected.

## Use a hosted model

Sign in once, then pass a model id from your account:

```bash
wally account login
wally opencode --cloud -m glm-5.3-flash
wally account usage         # remaining credit
```

<img src="docs/assets/harness-dark.gif" alt="OpenCode on the hosted GLM model fixing a bug through wally" width="100%">

## Commands you'll use

| Command | What it does |
|---|---|
| `wally run` | chat, or one answer with a prompt |
| `wally models pull` / `wally models rm` | download or delete a model |
| `wally models list` | models on this machine |
| `wally serve` | OpenAI-compatible API |
| `wally account login` / `wally account usage` | sign in, check credit |
| `wally account usage --requests` | settled requests in a window (`--follow` walks up to 100 pages) |
| `wally opencode` / `wally claude-code` | start a coding agent on a model |
| `wally update` | update wally to the latest release |

`wally --help` and `wally <command> --help` cover the rest.

## Build from source

Wally is Rust, built through CMake against a prebuilt RunAnywhere C++ desktop
kit (not the SDK source tree). [CONTRIBUTING.md](CONTRIBUTING.md) has the steps.

## More

- [Engines and platforms](docs/ENGINES.md): what runs where and how wally picks
- [Models](docs/MODELS.md): the full catalog
- [Editors and hosted models](docs/EDITORS.md): how each tool is wired, where your session lives
- [docs.runanywhere.ai](https://docs.runanywhere.ai)
- [Discord](https://discord.gg/N359FBbDVd)
- [Hugging Face](https://huggingface.co/runanywhere)

MIT. See [LICENSE](./LICENSE).
