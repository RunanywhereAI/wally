<p align="center">
  <img src="docs/assets/wally.gif" alt="Wally, the RunAnywhere mascot" width="120">
</p>

# Wally

[![Release](https://img.shields.io/github/v/release/RunanywhereAI/wally?label=release)](https://github.com/RunanywhereAI/wally/releases/latest)

***Wally*** is a command-line tool that connects your coding agent to the model of your choice. With a single command, it opens a coding agent, such as OpenCode, Claude Code, Hermes, OpenClaw, DeepSeek Harness or Prime Agent, already configured for that model, and it installs the agent first if it is missing. You can use a hosted model, such as GLM 5.3 Flash, billed against your RunAnywhere credit, or download an open model and run it on your own machine without an account. Wally can also chat with a model in the terminal and serve it through an OpenAI-compatible API. It runs on macOS, Linux and Windows.

**Get started with the [official documentation](https://docs.runanywhere.ai/).**

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/assets/harness-dark.gif">
  <img src="docs/assets/harness-light.gif" alt="OpenCode on the hosted GLM model fixing a bug through wally" width="100%">
</picture>

## Install

On macOS with Apple Silicon (Intel Macs are not supported) and on Linux with
x86-64 or ARM64 and glibc 2.39 or newer (Ubuntu 24.04+, Debian 13+):

```bash
curl -fsSL https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh | sh
```

On Windows (x64 and ARM64):

```powershell
irm https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1 | iex
```

The installer adds wally to your PATH for new terminals. In the terminal you
installed from, run the line it prints (`. ~/.zshrc` or similar), or open a new
one, before your first `wally` command. On Windows, open a new terminal.

## Quick start

Get up and running with a few commands.

**Log in:**

```bash
wally account login
```

**Open a coding agent on a hosted model:**

```bash
wally opencode -m glm-5.3-flash
```

**Or open it on the default model:**

```bash
wally opencode
```

The default model is `glm-5.3-flash`. To change it, run `wally models default <model>`.

**Test an app you are authorised to test with an AI pentester:**

```bash
wally shannon -- -u https://staging.example.com -r ./my-repo
```

Needs Docker. Node 18+ is required unless Shannon 3.4.0 is installed on PATH. See [Shannon](docs/EDITORS.md#shannon).

**Scan repos you are authorised to test:**

```bash
wally deepsec -- init . ./my-repo
wally vvah -- scan --repo ./my-repo
```

DeepSec 2.3.10 needs Node 18+ for `npx` unless it is on PATH. VVAH 1.5.1 needs Python 3.11–3.14 (it installs with pip into a venv). Models come from your signed-in account or `WALLY_CLOUD_KEY`. See [Security tools](docs/EDITORS.md#security-tools).

**Get help:**

```bash
wally help
```

Learn more about open models in our **[official documentation](https://docs.runanywhere.ai/)**.

## Ask Eve

Wally runs the models; Eve makes the calls. `eve` is RunAnywhere's hosted
decision model: it scores explicit labels without generating text. Ask yes/no,
choice, or ordered score questions:

```bash
wally decisions --input "Checkout is blank after Pay" \
  --ask "Is this a software bug?" \
  --choice "Owner=frontend,payments,account"
```

Use `--json` for response fields plus `request_id` and `http_status` (or an
`error` document on failure), or `--request FILE` for the complete
typed request shape. `--image FILE` (PNG, JPEG or WebP, up to 8, cloud only;
on macOS a HEIC photo is sent as a full-size JPEG, upright and without its
EXIF or GPS) asks every question about the images too; each image is billed as prompt
tokens once per question, and a model that does not take images refuses the
request before anything is billed. Images are sent at full size: the service
shrinks each to what the model sees, so a larger photo scores and costs the
same. It takes a request of up to 24 MiB with its images (1 MiB outside them),
and an image of up to 64 MP as JPEG or 24 MP as PNG or WebP; past those it
refuses with HTTP 400 and says which limit. Decision models are refused by `wally run` and the coding
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

Wally is written in Rust and built with CMake against a prebuilt RunAnywhere C++ desktop
kit (not the SDK source tree). [CONTRIBUTING.md](CONTRIBUTING.md) has the steps.

## More

- [Releases](https://github.com/RunanywhereAI/wally/releases): every version, with checksums
- [Engines and platforms](docs/ENGINES.md): what runs where and how wally picks
- [Models](docs/MODELS.md): the full catalog
- [Editors and hosted models](docs/EDITORS.md): how each coding agent is wired and where your session lives
- [docs.runanywhere.ai](https://docs.runanywhere.ai)
- [Discord](https://discord.gg/N359FBbDVd)
- [Hugging Face](https://huggingface.co/runanywhere)

Licensed under the MIT license. See [LICENSE](./LICENSE).
