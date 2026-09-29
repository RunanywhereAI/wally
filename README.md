<p align="center">
  <img src="docs/assets/wally.gif" alt="Wally, the RunAnywhere mascot" width="120">
</p>

# Wally

[![Release](https://img.shields.io/github/v/release/RunanywhereAI/wally?label=release)](https://github.com/RunanywhereAI/wally/releases/latest)

***Wally*** is a command-line tool that connects your coding agent to the model of your choice. With a single command, it opens a coding agent, such as OpenCode, Claude Code, Hermes, OpenClaw, DeepSeek Harness or Prime Agent, already configured for that model, and it installs the agent first if it is missing. You can use a hosted model, such as GLM 5.3 Flash, billed against your RunAnywhere credit, or download an open model and run it on your own machine without an account. Wally can also chat with a model in the terminal and serve it through an OpenAI-compatible API. It runs on macOS, Linux and Windows.

**Get Started with >> [official docs](https://docs.runanywhere.ai/)**
<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/harness-dark.gif">
  <img src="docs/assets/harness-light.gif" alt="OpenCode on the hosted GLM model fixing a bug through wally" width="100%">
</picture>


## Install

macOS on Apple Silicon (Intel Macs are not supported), or Linux on x86-64 or
ARM64 with glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+):

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

Windows (x64 and ARM64):

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex
```

The installer adds wally to your PATH for new terminals. In the terminal you
installed from, run the line it prints (`. ~/.zshrc` or similar), or open a new
one, before your first `wally` command. On Windows, open a new terminal.

## Quick Start

Some Instructions to Quickly Get Started.

**Wally Login:**
```bash
wally account login
```

**Install & Call a Harness:**
```bash
wally opencode -m glm-5.3-flash
```

**Or Launch the Harness with Default Model:**
```bash
wally opencode
```

**Get Help:**
```bash
wally help
```

## Ask Eve

Wally runs the models; Eve makes the calls. `eve` is RunAnywhere's hosted
decision model: it scores explicit labels without generating text. Ask yes/no,
choice, or ordered score questions:

```bash
wally decisions --input "Checkout is blank after Pay" \
  --ask "Is this a software bug?" \
  --choice "Owner=frontend,payments,account"
```

Use `--json` for the raw API response, or `--request FILE` for the complete
typed request shape. Decision models are refused by `wally run` and the coding
tools, which point back here.

Eve is also runnable on this machine. Pull a local checkpoint and point
`-m` at it; `--local` (or just a local `-m`) scores in-process through the
SDK's decision component, `--cloud` forces the hosted path:

```bash
wally models pull clef-flash-9b
wally decisions --local -m clef-flash-9b \
  --input "Checkout is blank after Pay" --ask "Is this a software bug?"
```

Both transports share the request flags, the human rendering and the `--json`
document, so the same invocation works locally and hosted.

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
