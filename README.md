<img src="docs/assets/wally.gif" alt="Wally, the RunAnywhere mascot" width="160" align="right">

# Wally

**Run open models on your own machine, or hosted when the job outgrows it.**

Chat with a language model from one terminal command. Local models never leave
your device. Hosted ones run on RunAnywhere Cloud and are billed against your
own credit.

<br clear="right">

## Install

macOS (Apple Silicon) and Linux (x86-64):

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

Windows:

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex
```

## Run a model on your machine

You don't need an account or a key, and nothing leaves the machine.

```bash
wally models pull qwen3     # download
wally run qwen3             # chat
wally run qwen3 "Hello"     # one answer and exit
wally serve qwen3           # OpenAI-compatible API on :8080 (macOS, Linux)
```

`wally models list --all` shows everything you can pull. Any Hugging Face GGUF
works too:

```bash
wally models pull hf.co/Qwen/Qwen3-0.6B-GGUF/Qwen3-0.6B-Q8_0.gguf
```

## Use a hosted model in your coding agent

Sign in once. The terminal shows a code and you approve it in the browser, so
you never paste a key.

```bash
wally account login
wally opencode --cloud -m glm-5.3-flash
wally claude-code -m glm-5.3-flash
wally account usage         # credit left and recent spend
```

Hosted models today are `glm-5.3-flash`, `qwen3.8-27b` and `gemma-4`. The same
commands work with a model on your machine, and `claude-desktop`, `hermes` and
`openclaw` are wired the same way.

## Commands you'll use

| | |
|---|---|
| `wally run` | chat, or one answer with a prompt |
| `wally models pull` / `wally models rm` | download or delete a model |
| `wally models list` | models on this machine |
| `wally serve` | OpenAI-compatible API |
| `wally account login` / `wally account usage` | sign in, check credit |
| `wally opencode` / `wally claude-code` | start a coding agent on a model |
| `wally update` | update wally to the latest release |

`wally --help` and `wally <command> --help` cover the rest.

## Build from source

Needs a built C++ desktop kit, not the SDK source. [CONTRIBUTING.md](CONTRIBUTING.md)
has the steps.

## More

- [Engines and platforms](docs/ENGINES.md): what runs where and how wally picks
- [Models](docs/MODELS.md): the full catalog
- [Editors and hosted models](docs/EDITORS.md): how each tool is wired, where your session lives
- [docs.runanywhere.ai](https://docs.runanywhere.ai) · [Discord](https://discord.gg/N359FBbDVd) · [Hugging Face](https://huggingface.co/runanywhere)

MIT. See [LICENSE](./LICENSE).
