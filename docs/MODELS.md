# Models

`wally models list --all` is the live list.

This table groups models by publisher; the id you pull depends on the engine
you want:

- `wally models pull <id>` — the llama.cpp / GGUF build (macOS, Windows x64, Linux).
- `wally models pull mlx-<id>` — the MLX build (Apple Silicon only).
- `ane-<id>` — the Apple Neural Engine build, where one exists. It is listed,
  not pullable: the catalog entry is a Hugging Face repo page, so `wally models
  pull ane-<id>` would only save that page's HTML. Download the compiled Core
  ML bundle yourself and point `--model` at the local tree.

MLX and ANE builds exist on Apple Silicon only; on any other platform they are
hidden from the catalog. The short aliases (`qwen3`, `llama3.2`, `smollm2`, …)
still resolve.

### Language

| Org | Families | Pull |
|---|---|---|
| [Alibaba Qwen](https://huggingface.co/Qwen) | Qwen3, Qwen3.8 | `qwen3-0.6b`, `mlx-qwen3-0.6b` |
| [Meta](https://huggingface.co/meta-llama) | Llama 3.2 | `llama-3.2-3b`, `mlx-llama3.2` |
| [Google](https://huggingface.co/google) | Gemma 4 | `gemma-4-e2b`, `mlx-gemma-4-e2b` |
| [Hugging Face](https://huggingface.co/HuggingFaceTB) | SmolLM2 | `smollm2-135m` |
| [Liquid AI](https://huggingface.co/LiquidAI) | LFM2.5 | `lfm2.5-350m`, `ane-lfm2.5-350m` (listed only, see above) |
| [IBM](https://huggingface.co/ibm-granite) | Granite 4.1, Granite 4.2 | `granite-4.1-3b`, `granite-4.2-8b` |
| [NVIDIA](https://huggingface.co/nvidia) | Nemotron | `mlx-nemotron-nano` |
| [PrismML](https://huggingface.co/prism-ml) | Bonsai, Ternary-Bonsai | `bonsai-1.7b`, `mlx-bonsai-1.7b` |
| [DeepGrove](https://huggingface.co/deepgrove) | Maple Preview | `maple-preview`, `mlx-maple-preview` |
