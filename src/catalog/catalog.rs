//! Built-in model catalog (port of src/catalog/catalog.cpp) — the CLI's curated
//! equivalent of the example apps' ModelCatalog. Entries use the proto-generated
//! enums and register through the same single-call commons entry points the SDKs
//! use (rac_register_model_from_url_proto / rac_register_multi_file_model_proto).
//! Registration is idempotent per process.

use std::sync::OnceLock;

use crate::io::output::{describe_result, status_line};
use crate::io::proto::{serialize, v1, ProtoBuffer};
use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogFile {
    pub url: &'static str,
    pub filename: &'static str,
    pub required: bool,
    pub size_bytes: i64,
    pub checksum_sha256: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogEntry {
    pub id: &'static str,
    /// short name accepted by `pull/run/...` (None = none)
    pub alias: Option<&'static str>,
    pub name: &'static str,
    pub category: v1::ModelCategory,
    pub framework: v1::InferenceFramework,
    pub format: v1::ModelFormat,
    /// single-file / archive primary (None → multi-file)
    pub url: Option<&'static str>,
    /// multi-file artifacts (VLM pairs, embeddings)
    pub files: &'static [CatalogFile],
    /// approximate, for display/planning
    pub download_size_bytes: i64,
    /// 0 = unknown/not applicable
    pub context_length: i32,
    pub supports_thinking: bool,
    /// 0 = unknown/not applicable
    pub memory_required_bytes: i64,
    /// Computer-Use-Agent profile id ("" = none)
    pub cua_profile: &'static str,
    /// Shared base for the same model across backends (llama.cpp / MLX / ANE /
    /// NPU). None → the row stands alone. `models list` groups by this.
    pub merge_key: Option<&'static str>,
}

// AUTO-TRANSLITERATED DATA START (parse_catalog.py) — do not hand-edit the
// tables below; regenerate with the script instead so every id/url/size stays
// byte-exact with src/catalog/catalog.cpp on origin/main d1e9c0b.

const MB: i64 = 1024 * 1024;

const SMOL_VLM2_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/ggml-org/SmolVLM2-256M-Video-Instruct-GGUF/resolve/main/SmolVLM2-256M-Video-Instruct-Q8_0.gguf",
        filename: "SmolVLM2-256M-Video-Instruct-Q8_0.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/ggml-org/SmolVLM2-256M-Video-Instruct-GGUF/resolve/main/mmproj-SmolVLM2-256M-Video-Instruct-Q8_0.gguf",
        filename: "mmproj-SmolVLM2-256M-Video-Instruct-Q8_0.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const LFM2_VL_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/runanywhere/LFM2-VL-450M-GGUF/resolve/main/LFM2-VL-450M-Q8_0.gguf",
        filename: "LFM2-VL-450M-Q8_0.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/runanywhere/LFM2-VL-450M-GGUF/resolve/main/mmproj-LFM2-VL-450M-Q8_0.gguf",
        filename: "mmproj-LFM2-VL-450M-Q8_0.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const LFM2_5_VL3_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-GGUF/resolve/main/LFM2.5-VL-3B-Q4_K_M.gguf",
        filename: "LFM2.5-VL-3B-Q4_K_M.gguf",
        required: true,
        size_bytes: 1674454240,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-GGUF/resolve/main/mmproj-LFM2.5-VL-3B-Q8_0.gguf",
        filename: "mmproj-LFM2.5-VL-3B-Q8_0.gguf",
        required: true,
        size_bytes: 583109120,
        checksum_sha256: None,
    },
];

const QWEN2_VL_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/ggml-org/Qwen2-VL-2B-Instruct-GGUF/resolve/main/Qwen2-VL-2B-Instruct-Q4_K_M.gguf",
        filename: "Qwen2-VL-2B-Instruct-Q4_K_M.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/ggml-org/Qwen2-VL-2B-Instruct-GGUF/resolve/main/mmproj-Qwen2-VL-2B-Instruct-Q8_0.gguf",
        filename: "mmproj-Qwen2-VL-2B-Instruct-Q8_0.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const FARA15_GGUF_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/runanywhere/Fara1.5-4B-GGUF/resolve/main/Fara1.5-4B-Q4_K_M.gguf",
        filename: "Fara1.5-4B-Q4_K_M.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/runanywhere/Fara1.5-4B-GGUF/resolve/main/mmproj-Fara1.5-4B-f16.gguf",
        filename: "mmproj-Fara1.5-4B-f16.gguf",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MINI_LM_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model.onnx",
        filename: "model.onnx",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/vocab.txt",
        filename: "vocab.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const SHERPA_PARAKEET_TDT_V2_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/1ab9323565ddb038682214b292f588070a538ce2/encoder.int8.onnx",
        filename: "encoder.int8.onnx",
        required: true,
        size_bytes: 652184296,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/1ab9323565ddb038682214b292f588070a538ce2/decoder.int8.onnx",
        filename: "decoder.int8.onnx",
        required: true,
        size_bytes: 7257753,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/1ab9323565ddb038682214b292f588070a538ce2/joiner.int8.onnx",
        filename: "joiner.int8.onnx",
        required: true,
        size_bytes: 1739080,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8/resolve/1ab9323565ddb038682214b292f588070a538ce2/tokens.txt",
        filename: "tokens.txt",
        required: true,
        size_bytes: 9384,
        checksum_sha256: None,
    },
];

const SHERPA_PARAKEET_TDT_V3_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/encoder.int8.onnx",
        filename: "encoder.int8.onnx",
        required: true,
        size_bytes: 652184281,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/decoder.int8.onnx",
        filename: "decoder.int8.onnx",
        required: true,
        size_bytes: 11845275,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/joiner.int8.onnx",
        filename: "joiner.int8.onnx",
        required: true,
        size_bytes: 6355277,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/resolve/2bda32ec70b097a55adaa07d9a7173915b43cc78/tokens.txt",
        filename: "tokens.txt",
        required: true,
        size_bytes: 93939,
        checksum_sha256: None,
    },
];

const SHERPA_CANARY180_MFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/resolve/9077164e0d3dd1d5353743e89ceaa1d3a770838c/encoder.int8.onnx",
        filename: "encoder.int8.onnx",
        required: true,
        size_bytes: 132678643,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/resolve/9077164e0d3dd1d5353743e89ceaa1d3a770838c/decoder.int8.onnx",
        filename: "decoder.int8.onnx",
        required: true,
        size_bytes: 74437848,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/resolve/9077164e0d3dd1d5353743e89ceaa1d3a770838c/tokens.txt",
        filename: "tokens.txt",
        required: true,
        size_bytes: 53555,
        checksum_sha256: None,
    },
];

const SHERPA_NEMOTRON_STREAMING_ASR_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-320ms-int8-2026-06-11/resolve/424ce58898995b713f84341f2e1492f9207a26aa/encoder.int8.onnx",
        filename: "encoder.int8.onnx",
        required: true,
        size_bytes: 657601518,
        checksum_sha256: Some("f79c3fcc149f268b54b7d5754bdc2ba5c47c16b1fc70d15728a56f6efbf60ca5"),
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-320ms-int8-2026-06-11/resolve/424ce58898995b713f84341f2e1492f9207a26aa/decoder.int8.onnx",
        filename: "decoder.int8.onnx",
        required: true,
        size_bytes: 14978075,
        checksum_sha256: Some("19f9c98fc6d0a2c33a65a43b36fdb2e914c26c0aa9764be3aebc502a1e982fb0"),
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-320ms-int8-2026-06-11/resolve/424ce58898995b713f84341f2e1492f9207a26aa/joiner.int8.onnx",
        filename: "joiner.int8.onnx",
        required: true,
        size_bytes: 9504438,
        checksum_sha256: Some("4101c7c679a0bc30483794b27a059e34e79232aa2068d78d51231a22c8b0d7ce"),
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-nemotron-3.5-asr-streaming-0.6b-320ms-int8-2026-06-11/resolve/424ce58898995b713f84341f2e1492f9207a26aa/tokens.txt",
        filename: "tokens.txt",
        required: true,
        size_bytes: 131440,
        checksum_sha256: Some("729cc103155bafa785f9cd45746cd41cabe97eab7182fc04d594129587958f8a"),
    },
];

const SHERPA_PARAKEET_CTC_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/runanywhere/sherpa-onnx-nemo-parakeet-ctc-1.1b-int8/resolve/48a549f552774db3cd09dd1548f3d1a2b37bc7c5/model.int8.onnx",
        filename: "model.int8.onnx",
        required: true,
        size_bytes: 1110014145,
        checksum_sha256: Some("62f73c17a5301c048c7273cf24ef1cd0c3621d3625c5415fbafe5633d7bf2f98"),
    },
    CatalogFile {
        url: "https://huggingface.co/runanywhere/sherpa-onnx-nemo-parakeet-ctc-1.1b-int8/resolve/48a549f552774db3cd09dd1548f3d1a2b37bc7c5/tokens.txt",
        filename: "tokens.txt",
        required: true,
        size_bytes: 10374,
        checksum_sha256: Some("ed16e1a4e3a3aa379138c0b1888e5d49f993c9d512b2be4d46e90a87afd54921"),
    },
];

const MLX_QWEN3_06_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/added_tokens.json",
        filename: "added_tokens.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-0.6B-4bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_MAPLE_PREVIEW_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/added_tokens.json",
        filename: "added_tokens.json",
        required: true,
        size_bytes: 707,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 3292,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 2710,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 1671853,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/model-00001-of-00003.safetensors",
        filename: "model-00001-of-00003.safetensors",
        required: true,
        size_bytes: 2162084350,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/model-00002-of-00003.safetensors",
        filename: "model-00002-of-00003.safetensors",
        required: true,
        size_bytes: 2187444586,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/model-00003-of-00003.safetensors",
        filename: "model-00003-of-00003.safetensors",
        required: true,
        size_bytes: 958711742,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/model-flashhead.safetensors",
        filename: "model-flashhead.safetensors",
        required: true,
        size_bytes: 6087456,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 40054,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 613,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 11422654,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 5432,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/deepgrove/maple-preview-2bit-mlx/resolve/d0a7314d6bf14c880201b599d7a701cfbc8717e6/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 2776833,
        checksum_sha256: None,
    },
];

const MLX_NEMOTRON_NANO8_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/bourn23/nvidia-llama-3.1-nemotron-nano-8b-v1-mlx-4bit/resolve/00378e66048eadf358aad0f66c09e5c3750f8243/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_NEMOTRON_MINI4_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Nemotron-Mini-4B-Instruct-4bit-mlx/resolve/b5784198153d2d71afcc97d4cc38c049abced8cd/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_LLAMA32_1_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Llama-3.2-1B-Instruct-4bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_QWEN2_VL2_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/added_tokens.json",
        filename: "added_tokens.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/chat_template.json",
        filename: "chat_template.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/preprocessor_config.json",
        filename: "preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen2-VL-2B-Instruct-4bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_FAST_VLM05_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/added_tokens.json",
        filename: "added_tokens.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/llava_qwen.py",
        filename: "llava_qwen.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/preprocessor_config.json",
        filename: "preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/processing_fastvlm.py",
        filename: "processing_fastvlm.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/FastVLM-0.5B-bf16/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_LFM2_5_VL3_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/LiquidAI/LFM2.5-VL-3B-MLX-4bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_QWEN3_EMBEDDING06_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/added_tokens.json",
        filename: "added_tokens.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_QWEN3_ASR06_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/chat_template.json",
        filename: "chat_template.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/preprocessor_config.json",
        filename: "preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-ASR-0.6B-8bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GLM_ASR_NANO2512_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/configuration_glmasr.py",
        filename: "configuration_glmasr.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/inference.py",
        filename: "inference.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/modeling_audio.py",
        filename: "modeling_audio.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/modeling_glmasr.py",
        filename: "modeling_glmasr.py",
        required: false,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/GLM-ASR-Nano-2512-4bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_PARAKEET_CTC11_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-ctc-1.1b/resolve/295d0c0557aef0c445db79b3d09c9a94a69ffeaf/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-ctc-1.1b/resolve/295d0c0557aef0c445db79b3d09c9a94a69ffeaf/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_PARAKEET_TDT_V2_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2/resolve/8ae155301e23d820d82aa60d24817c900e69e487/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2/resolve/8ae155301e23d820d82aa60d24817c900e69e487/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_PARAKEET_TDT_V3_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v3/resolve/ed2b7e8c15f9aaa0b5772e2efb986255eaef7e15/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v3/resolve/ed2b7e8c15f9aaa0b5772e2efb986255eaef7e15/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_PARAKEET_RNNT11_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-rnnt-1.1b/resolve/7f399a0d3442123deae9194e71f5c984b2879efa/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/parakeet-rnnt-1.1b/resolve/7f399a0d3442123deae9194e71f5c984b2879efa/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_NEMOTRON_STREAMING_ASR_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/nemotron-3.5-asr-streaming-0.6b-8bit/resolve/7279359e4481b5e9e185a318bd618e429c6d86cd/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/nemotron-3.5-asr-streaming-0.6b-8bit/resolve/7279359e4481b5e9e185a318bd618e429c6d86cd/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_QWEN3_TTS06_BBASE_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/preprocessor_config.json",
        filename: "preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/speech_tokenizer/config.json",
        filename: "speech_tokenizer/config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/speech_tokenizer/configuration.json",
        filename: "speech_tokenizer/configuration.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/speech_tokenizer/model.safetensors",
        filename: "speech_tokenizer/model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/speech_tokenizer/preprocessor_config.json",
        filename: "speech_tokenizer/preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_SOPRANO1180_M5_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/special_tokens_map.json",
        filename: "special_tokens_map.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Soprano-1.1-80M-5bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_BONSAI27_B1_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-27B-mlx-1bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_BONSAI1_7_B1_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-1.7B-mlx-1bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_BONSAI4_B1_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-4B-mlx-1bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_BONSAI8_B1_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Bonsai-8B-mlx-1bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_TERNARY_BONSAI1_7_B2_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-mlx-2bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_TERNARY_BONSAI4_B2_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-4B-mlx-2bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_TERNARY_BONSAI8_B2_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-8B-mlx-2bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_TERNARY_BONSAI27_B2_BIT_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/merges.txt",
        filename: "merges.txt",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/prism-ml/Ternary-Bonsai-27B-mlx-2bit/resolve/main/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MUSE_GLIMMER30_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/unsloth/Muse-Glimmer-30B-GGUF/resolve/faa5b025c584459c13febfa5c59883516710ae39/Muse-Glimmer-30B-UD-Q4_K_XL.gguf",
        filename: "Muse-Glimmer-30B-UD-Q4_K_XL.gguf",
        required: true,
        size_bytes: 15878222368,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/unsloth/Muse-Glimmer-30B-GGUF/resolve/faa5b025c584459c13febfa5c59883516710ae39/mmproj-Muse-Glimmer-30B-Q8_0.gguf",
        filename: "mmproj-Muse-Glimmer-30B-Q8_0.gguf",
        required: true,
        size_bytes: 2051685088,
        checksum_sha256: None,
    },
];

const NEMOTRON_OMNI_REASONING_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/unsloth/NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-GGUF/resolve/571758804835f56154718683f5c0e388b7d0fef9/NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-UD-Q4_K_M.gguf",
        filename: "NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-UD-Q4_K_M.gguf",
        required: true,
        size_bytes: 23887023552,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/unsloth/NVIDIA-Nemotron-3-Nano-Omni-30B-A3B-Reasoning-GGUF/resolve/571758804835f56154718683f5c0e388b7d0fef9/mmproj-F16.gguf",
        filename: "mmproj-F16.gguf",
        required: true,
        size_bytes: 1587540224,
        checksum_sha256: None,
    },
];

const MLX_GEMMA4_E2_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 3550670554,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-e2b-it-4bit/resolve/238767527555cb75a05732a84dff5d6ba0dd6809/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GEMMA4_E4_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/model-00001-of-00002.safetensors",
        filename: "model-00001-of-00002.safetensors",
        required: true,
        size_bytes: 4249502053,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/model-00002-of-00002.safetensors",
        filename: "model-00002-of-00002.safetensors",
        required: true,
        size_bytes: 2548805689,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-E4B-it-qat-4bit/resolve/0f35c6f6d386f7f74e628bd7c6526ce531212300/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GEMMA4_12_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/model-00001-of-00003.safetensors",
        filename: "model-00001-of-00003.safetensors",
        required: true,
        size_bytes: 5343482357,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/model-00002-of-00003.safetensors",
        filename: "model-00002-of-00003.safetensors",
        required: true,
        size_bytes: 5315166254,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/model-00003-of-00003.safetensors",
        filename: "model-00003-of-00003.safetensors",
        required: true,
        size_bytes: 329123819,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-12B-it-qat-4bit/resolve/e70c6b3ba0979b3357dcd2f223ad8bde7787a6b6/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GEMMA4_26_BA4_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/model-00001-of-00003.safetensors",
        filename: "model-00001-of-00003.safetensors",
        required: true,
        size_bytes: 5320218487,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/model-00002-of-00003.safetensors",
        filename: "model-00002-of-00003.safetensors",
        required: true,
        size_bytes: 5363328422,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/model-00003-of-00003.safetensors",
        filename: "model-00003-of-00003.safetensors",
        required: true,
        size_bytes: 4657658867,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-26b-a4b-it-4bit/resolve/0d77464eeb233a2da68ebf9d7dc4edaac7db956d/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GEMMA4_31_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/model-00001-of-00004.safetensors",
        filename: "model-00001-of-00004.safetensors",
        required: true,
        size_bytes: 5366617512,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/model-00002-of-00004.safetensors",
        filename: "model-00002-of-00004.safetensors",
        required: true,
        size_bytes: 5361642573,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/model-00003-of-00004.safetensors",
        filename: "model-00003-of-00004.safetensors",
        required: true,
        size_bytes: 5367276094,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/model-00004-of-00004.safetensors",
        filename: "model-00004-of-00004.safetensors",
        required: true,
        size_bytes: 2316480497,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/gemma-4-31b-it-4bit/resolve/696d436c404745a59f30e4939a658162b0a9e57f/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_QWEN3_8_27_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/model-00001-of-00003.safetensors",
        filename: "model-00001-of-00003.safetensors",
        required: true,
        size_bytes: 5343268662,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/model-00002-of-00003.safetensors",
        filename: "model-00002-of-00003.safetensors",
        required: true,
        size_bytes: 5354185130,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/model-00003-of-00003.safetensors",
        filename: "model-00003-of-00003.safetensors",
        required: true,
        size_bytes: 5357087557,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/preprocessor_config.json",
        filename: "preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/processor_config.json",
        filename: "processor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/video_preprocessor_config.json",
        filename: "video_preprocessor_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/Qwen3.8-27B-4bit/resolve/3e6447f082e89cc7f0bc6e5441afd38dfce760ff/vocab.json",
        filename: "vocab.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GRANITE4_1_3_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 2127162429,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-3b-4bit/resolve/b1b476b5a17c46b7d6cd663b4a8ed44b66720aef/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GRANITE4_1_8_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/model.safetensors",
        filename: "model.safetensors",
        required: true,
        size_bytes: 5238406779,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-8b-4bit/resolve/08fb1e272f7bd49fa83ce279bbdc496c980380ac/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const MLX_GRANITE4_1_30_BFILES_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/chat_template.jinja",
        filename: "chat_template.jinja",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/config.json",
        filename: "config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/generation_config.json",
        filename: "generation_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/model-00001-of-00004.safetensors",
        filename: "model-00001-of-00004.safetensors",
        required: true,
        size_bytes: 5360664833,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/model-00002-of-00004.safetensors",
        filename: "model-00002-of-00004.safetensors",
        required: true,
        size_bytes: 5363828231,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/model-00003-of-00004.safetensors",
        filename: "model-00003-of-00004.safetensors",
        required: true,
        size_bytes: 5363828281,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/model-00004-of-00004.safetensors",
        filename: "model-00004-of-00004.safetensors",
        required: true,
        size_bytes: 1953655228,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/model.safetensors.index.json",
        filename: "model.safetensors.index.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/tokenizer.json",
        filename: "tokenizer.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/mlx-community/granite-4.1-30b-4bit/resolve/03e8065d3219e525aa27fc4aaa9b375fe2cd6cb8/tokenizer_config.json",
        filename: "tokenizer_config.json",
        required: true,
        size_bytes: 0,
        checksum_sha256: None,
    },
];

const SHERPA_SUPERTONIC_V3_FILES: &[CatalogFile] = &[
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/duration_predictor.int8.onnx",
        filename: "duration_predictor.int8.onnx",
        required: true,
        size_bytes: 3700147,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/text_encoder.int8.onnx",
        filename: "text_encoder.int8.onnx",
        required: true,
        size_bytes: 36416150,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/tts.json",
        filename: "tts.json",
        required: true,
        size_bytes: 8253,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/unicode_indexer.bin",
        filename: "unicode_indexer.bin",
        required: true,
        size_bytes: 262144,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/vector_estimator.int8.onnx",
        filename: "vector_estimator.int8.onnx",
        required: true,
        size_bytes: 78400833,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/vocoder.int8.onnx",
        filename: "vocoder.int8.onnx",
        required: true,
        size_bytes: 25991073,
        checksum_sha256: None,
    },
    CatalogFile {
        url: "https://huggingface.co/csukuangfj2/sherpa-onnx-supertonic-3-tts-int8-2026-05-11/resolve/cca5a0e6c96e1d2c720986bf7e75fcc81dee3ae4/voice.bin",
        filename: "voice.bin",
        required: true,
        size_bytes: 517168,
        checksum_sha256: None,
    },
];

const CATALOG: &[CatalogEntry] = &[
    // --- LLM (LlamaCpp / GGUF) ---
    // Name carries Q8_0 on purpose: this is the one GGUF artifact in the
    // catalog above 4-bit (verbatim from the consumer apps and matches the
    // Linux test rig's layout -- see the file comment above kCatalog), so the
    // display name must not claim the same "just the size" naming the <=4-bit
    // entries use. Swap the URL for a verified <=4-bit artifact instead of
    // relabeling if this is ever tightened to match the rest of the catalog.
    CatalogEntry {
        id: "qwen3-0.6b",
        alias: Some("qwen3"),
        name: "Qwen3 0.6B Q8_0",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q8_0.gguf"),
        files: &[],
        download_size_bytes: 639 * MB,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("qwen3-0.6b"),
    },
    // RunAnywhere's canonical-based llama.cpp fork supports PrismML's Q1_0
    // Bonsai artifacts. Ternary-Bonsai uses the explicitly canonical
    // Q2_0_g64 artifacts below; legacy 128-value Q2_0 remains unsupported.
    // Exact artifact byte sizes.
    CatalogEntry {
        id: "bonsai-1.7b",
        alias: Some("bonsai-1.7b"),
        name: "Bonsai 1.7B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Bonsai-1.7B-gguf/resolve/main/Bonsai-1.7B-Q1_0.gguf"),
        files: &[],
        download_size_bytes: 248302272,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-1.7b"),
    },
    CatalogEntry {
        id: "bonsai-4b",
        alias: Some("bonsai-4b"),
        name: "Bonsai 4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Bonsai-4B-gguf/resolve/main/Bonsai-4B-Q1_0.gguf"),
        files: &[],
        download_size_bytes: 572270624,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-4b"),
    },
    CatalogEntry {
        id: "bonsai-8b",
        alias: Some("bonsai-8b"),
        name: "Bonsai 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Bonsai-8B-gguf/resolve/main/Bonsai-8B-Q1_0.gguf"),
        files: &[],
        download_size_bytes: 1158654496,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-8b"),
    },
    CatalogEntry {
        id: "bonsai-27b",
        alias: Some("bonsai-27b"),
        name: "Bonsai 27B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Bonsai-27B-gguf/resolve/main/Bonsai-27B-Q1_0.gguf"),
        files: &[],
        download_size_bytes: 3803452480,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-27b"),
    },
    CatalogEntry {
        id: "ternary-bonsai-1.7b",
        alias: Some("ternary-bonsai-1.7b"),
        name: "Ternary-Bonsai 1.7B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Ternary-Bonsai-1.7B-gguf/resolve/983b5dec2ff16aab79990711ba0f828a499a7e6a/Ternary-Bonsai-1.7B-Q2_0_g64.gguf"),
        files: &[],
        download_size_bytes: 490163968,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-1.7b"),
    },
    CatalogEntry {
        id: "ternary-bonsai-4b",
        alias: Some("ternary-bonsai-4b"),
        name: "Ternary-Bonsai 4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Ternary-Bonsai-4B-gguf/resolve/a3eb42bafe873f9686bc97486c43b72ef7d75ec8/Ternary-Bonsai-4B-Q2_0_g64.gguf"),
        files: &[],
        download_size_bytes: 1137806656,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-4b"),
    },
    CatalogEntry {
        id: "ternary-bonsai-8b",
        alias: Some("ternary-bonsai-8b"),
        name: "Ternary-Bonsai 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/prism-ml/Ternary-Bonsai-8B-gguf/resolve/c2aefbeb4b24469cd11579c3384b990404c17a30/Ternary-Bonsai-8B-Q2_0_g64.gguf"),
        files: &[],
        download_size_bytes: 2310125920,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-8b"),
    },
    CatalogEntry {
        id: "maple-preview",
        alias: Some("maple-preview"),
        name: "DeepGrove Maple Preview",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/deepgrove/maple-preview-GGUF/resolve/f5466f918e0c50cdb9d4d47a6f35813509a42a30/maple-preview-TQ1_0-head-Q4_K.gguf"),
        files: &[],
        download_size_bytes: 4984016416,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("maple-preview"),
    },
    CatalogEntry {
        id: "llama-3.2-3b",
        alias: Some("llama3.2"),
        name: "Llama 3.2 3B Instruct",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/bartowski/Llama-3.2-3B-Instruct-GGUF/resolve/main/Llama-3.2-3B-Instruct-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 2020 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // LiquidAI LFM2.5 family (official GGUF, Apache 2.0). Replaces the older
    // LFM2 Q8 entry: newer version, ≤4-bit, pinned revisions. 230M/350M also ship
    // as ANE (Core ML) and NPU (Hexagon) bundles below, merged into one list row.
    CatalogEntry {
        id: "lfm2.5-230m",
        alias: Some("lfm2.5-230m"),
        name: "LiquidAI LFM2.5 230M",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/LiquidAI/LFM2.5-230M-GGUF/resolve/cdf97bd8205908758f44aec508d68ac1aef98f5c/LFM2.5-230M-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 153406304,
        context_length: 32768,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-230m"),
    },
    CatalogEntry {
        id: "lfm2.5-350m",
        alias: Some("lfm2.5-350m"),
        name: "LiquidAI LFM2.5 350M",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/LiquidAI/LFM2.5-350M-GGUF/resolve/9969000761ce34de907bf20017cbfc3d52d6eaf9/LFM2.5-350M-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 219 * MB,
        context_length: 32768,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-350m"),
    },
    CatalogEntry {
        id: "lfm2.5-1.2b",
        alias: Some("lfm2.5"),
        name: "LiquidAI LFM2.5 1.2B Instruct",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/LiquidAI/LFM2.5-1.2B-Instruct-GGUF/resolve/6767265158422fb8a19c62ceb45f16f05363615b/LFM2.5-1.2B-Instruct-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 697 * MB,
        context_length: 32768,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-1.2b"),
    },
    CatalogEntry {
        id: "lfm2.5-2.6b",
        alias: Some("lfm2.5-2.6b"),
        name: "LiquidAI LFM2.5 2.6B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/LiquidAI/LFM2.5-2.6B-GGUF/resolve/84022ce711b28455e8c4fc364ce68c00cf995875/LFM2.5-2.6B-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 1597 * MB,
        context_length: 32768,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // SmolLM2 135M from the llama.cpp org's own GGUF (official), ≤4-bit.
    CatalogEntry {
        id: "smollm2-135m",
        alias: Some("smollm2"),
        name: "SmolLM2 135M",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/ggml-org/SmolLM2-135M-GGUF/resolve/44686446221a479a9227d7a895cf92930f86de8a/SmolLM2-135M-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 96 * MB,
        context_length: 8192,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // Google Gemma 4 family (GGUF). Licensed under Apache 2.0; preserve the
    // upstream license and attribution notices when redistributing.
    CatalogEntry {
        id: "gemma-4-e2b",
        alias: Some("gemma4-e2b"),
        name: "Gemma 4 E2B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/gemma-4-E2B-it-GGUF/resolve/0314792d7f1f7e229411f620751375812bb9faf2/gemma-4-E2B-it-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 3106738272,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-e2b"),
    },
    CatalogEntry {
        id: "gemma-4-e4b",
        alias: Some("gemma4-e4b"),
        name: "Gemma 4 E4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/gemma-4-E4B-it-GGUF/resolve/bfc15c382204943c3a8fff0c750b94ae2364d7a3/gemma-4-E4B-it-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 4977171584,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-e4b"),
    },
    CatalogEntry {
        id: "gemma-4-12b",
        alias: Some("gemma4-12b"),
        name: "Gemma 4 12B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/gemma-4-12b-it-GGUF/resolve/fc034cfff751157913579611efad8462ac1be606/gemma-4-12b-it-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 7121861440,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-12b"),
    },
    CatalogEntry {
        id: "gemma-4-26b-a4b",
        alias: Some("gemma4-26b-a4b"),
        name: "Gemma 4 26B-A4B (MoE)",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/gemma-4-26B-A4B-it-GGUF/resolve/c099eb48e663fd284577b04978a94ffccb261841/gemma-4-26B-A4B-it-UD-Q4_K_XL.gguf"),
        files: &[],
        download_size_bytes: 17010980576,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-26b-a4b"),
    },
    CatalogEntry {
        id: "gemma-4-31b",
        alias: Some("gemma4-31b"),
        name: "Gemma 4 31B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/gemma-4-31B-it-GGUF/resolve/c1ac76e99d5513b141e8adde7288b85c3f9c32ec/gemma-4-31B-it-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 18323733440,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-31b"),
    },
    // Qwen3.8-27B (dense, newest Qwen, Apache 2.0).
    CatalogEntry {
        id: "qwen3.8-27b",
        alias: Some("qwen3.8-27b"),
        name: "Qwen3.8 27B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/Qwen3.8-27B-GGUF/resolve/f1bfb127c64f7072bdd2cad55f258b9c8b2910fe/Qwen3.8-27B-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 17106775008,
        context_length: 262144,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("qwen3.8-27b"),
    },
    // IBM Granite 4.1 family (Apache 2.0).
    CatalogEntry {
        id: "granite-4.1-3b",
        alias: Some("granite4.1-3b"),
        name: "IBM Granite 4.1 3B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/granite-4.1-3b-GGUF/resolve/5b88826e4b80789548180f8faab39c5cf68772c9/granite-4.1-3b-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 2099502400,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-3b"),
    },
    CatalogEntry {
        id: "granite-4.1-8b",
        alias: Some("granite4.1-8b"),
        name: "IBM Granite 4.1 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/granite-4.1-8b-GGUF/resolve/6f9671f73eb03273bc09319194b8a4e810e03a8f/granite-4.1-8b-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 5347915136,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-8b"),
    },
    CatalogEntry {
        id: "granite-4.1-30b",
        alias: Some("granite4.1-30b"),
        name: "IBM Granite 4.1 30B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/unsloth/granite-4.1-30b-GGUF/resolve/6cb34f31b11ca4c1433de1af7391dac46de4e666/granite-4.1-30b-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 17490241472,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-30b"),
    },
    // IBM Granite 4.2 family (bartowski GGUF, Apache 2.0) — newest Granite.
    CatalogEntry {
        id: "granite-4.2-8b",
        alias: Some("granite4.2-8b"),
        name: "IBM Granite 4.2 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/bartowski/granite-4.2-8b-GGUF/resolve/a592100df8fe4931c7cffbac7b28e8176a1d52da/granite-4.2-8b-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 5283 * MB,
        context_length: 131072,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "granite-4.2-30b",
        alias: Some("granite4.2-30b"),
        name: "IBM Granite 4.2 30B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/bartowski/granite-4.2-30b-GGUF/resolve/1847d3b70241af9d656f382a4cf29d5c6573e584/granite-4.2-30b-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 17192 * MB,
        context_length: 131072,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- VLM (gguf + mmproj pairs) ---
    CatalogEntry {
        id: "smolvlm2-256m-video-instruct-q8_0",
        alias: Some("smolvlm2"),
        name: "SmolVLM2 256M Video Instruct Q8_0",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: SMOL_VLM2_FILES,
        download_size_bytes: 420 * MB,
        context_length: 2048,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "lfm2-vl-450m-q8_0",
        alias: Some("lfm2-vl"),
        name: "LFM2-VL 450M Q8_0",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: LFM2_VL_FILES,
        download_size_bytes: 600 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // Native window is 128k (lfm2.context_length in the GGUF); 4096 is the
    // on-device working context, matching the other multi-GB VLM row.
    CatalogEntry {
        id: "lfm2.5-vl-3b-q4_k_m",
        alias: Some("lfm2.5-vl"),
        name: "LFM2.5-VL 3B Q4_K_M",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: LFM2_5_VL3_BFILES_FILES,
        download_size_bytes: 2257563360,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "qwen2-vl-2b-instruct-q4_k_m",
        alias: Some("qwen2-vl"),
        name: "Qwen2-VL 2B Instruct Q4_K_M",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: QWEN2_VL_FILES,
        download_size_bytes: 1800 * MB,
        context_length: 2048,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "fara1.5-4b-q4_k_m",
        alias: Some("fara"),
        name: "Fara1.5 4B Computer-Use Agent Q4_K_M",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: FARA15_GGUF_FILES,
        download_size_bytes: 3300 * MB,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "fara",
        merge_key: None,
    },
    // Meta Muse Glimmer 30B (Apache 2.0). llama.cpp's mmproj is image-only —
    // vision-capable, not the checkpoint's marketed audio/video "omni" surface.
    CatalogEntry {
        id: "muse-glimmer-30b-q4_k_xl",
        alias: Some("muse-glimmer"),
        name: "Muse Glimmer 30B UD-Q4_K_XL",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: MUSE_GLIMMER30_BFILES_FILES,
        download_size_bytes: 17929907456,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // NVIDIA Nemotron-3-Nano-Omni-30B-A3B-Reasoning (MoE, NVIDIA Open Model
    // License). Same image-only mmproj caveat as Muse Glimmer above.
    CatalogEntry {
        id: "nemotron-3-nano-omni-30b-a3b-reasoning-q4_k_m",
        alias: Some("nemotron-omni"),
        name: "NVIDIA Nemotron-3-Nano-Omni 30B-A3B Reasoning UD-Q4_K_M (vision, MoE)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: None,
        files: NEMOTRON_OMNI_REASONING_FILES,
        download_size_bytes: 25474563776,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Speech (Sherpa-ONNX archives; orchestrator extracts in-core) ---
    CatalogEntry {
        id: "sherpa-onnx-whisper-tiny.en",
        alias: Some("whisper-tiny"),
        name: "Whisper Tiny English (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: Some("https://github.com/RunanywhereAI/sherpa-onnx/releases/download/runanywhere-models-v1/sherpa-onnx-whisper-tiny.en.tar.gz"),
        files: &[],
        download_size_bytes: 75 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "sherpa-nemo-parakeet-tdt-0.6b-v2-int8",
        alias: Some("parakeet-tdt-v2"),
        name: "NVIDIA Parakeet TDT 0.6B v2 INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_PARAKEET_TDT_V2_FILES,
        download_size_bytes: 661190513,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "sherpa-nemo-parakeet-tdt-0.6b-v3-int8",
        alias: Some("parakeet-tdt-v3"),
        name: "NVIDIA Parakeet TDT 0.6B v3 INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_PARAKEET_TDT_V3_FILES,
        download_size_bytes: 670478772,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "sherpa-nemo-parakeet-ctc-1.1b-int8",
        alias: Some("parakeet-ctc"),
        name: "NVIDIA Parakeet CTC 1.1B INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_PARAKEET_CTC_FILES,
        download_size_bytes: 1110024519,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 2147483648,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "sherpa-nemo-canary-180m-flash-int8",
        alias: Some("canary-180m"),
        name: "NVIDIA Canary 180M Flash INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_CANARY180_MFILES_FILES,
        download_size_bytes: 207170046,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "sherpa-nemotron-3.5-asr-streaming-0.6b-320ms-int8",
        alias: Some("nemotron-asr-streaming"),
        name: "NVIDIA Nemotron 3.5 Streaming ASR 0.6B 320ms INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_NEMOTRON_STREAMING_ASR_FILES,
        download_size_bytes: 682215471,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "vits-piper-en_US-lessac-medium",
        alias: Some("piper"),
        name: "Piper TTS US English (Lessac Medium)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: Some("https://github.com/RunanywhereAI/sherpa-onnx/releases/download/runanywhere-models-v1/vits-piper-en_US-lessac-medium.tar.gz"),
        files: &[],
        download_size_bytes: 65 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // Supertone Supertonic v3 (MIT). Not the raw Supertone/supertonic-3 repo —
    // see kSherpaSupertonicV3Files for why. Needs sherpa-onnx >= 1.13.2.
    CatalogEntry {
        id: "sherpa-supertonic-3-tts-int8",
        alias: Some("supertonic"),
        name: "Supertone Supertonic v3 TTS INT8 (Sherpa-ONNX)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Sherpa,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: SHERPA_SUPERTONIC_V3_FILES,
        download_size_bytes: 145295768,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- VAD ---
    // Exact artifact size (matches iOS ModelCatalogBootstrap.swift): the
    // post-finalize size guard treats download_size_bytes as authoritative,
    // and an over-stated 3 MB estimate tripped it on the valid ~2.3 MB file.
    CatalogEntry {
        id: "silero-vad",
        alias: Some("silero"),
        name: "Silero VAD",
        category: v1::ModelCategory::VoiceActivityDetection,
        framework: v1::InferenceFramework::Onnx,
        format: v1::ModelFormat::Onnx,
        url: Some("https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx"),
        files: &[],
        download_size_bytes: 2327524,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Speaker diarization (ONNX Runtime) ---
    CatalogEntry {
        id: "diar-streaming-sortformer-4spk-v2.1",
        alias: Some("sortformer"),
        name: "NVIDIA Streaming Sortformer 4-Speaker v2.1",
        category: v1::ModelCategory::SpeakerDiarization,
        framework: v1::InferenceFramework::Onnx,
        format: v1::ModelFormat::Onnx,
        url: Some("https://huggingface.co/cgus/diar_streaming_sortformer_4spk-v2.1-onnx/resolve/main/diar_streaming_sortformer_4spk-v2.1.onnx"),
        files: &[],
        download_size_bytes: 492242946,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Semantic segmentation (ONNX Runtime) ---
    CatalogEntry {
        id: "segformer-b0-ade-512",
        alias: Some("segformer"),
        name: "SegFormer B0 ADE20K 512 (Semantic Segmentation)",
        category: v1::ModelCategory::SemanticSegmentation,
        framework: v1::InferenceFramework::Onnx,
        format: v1::ModelFormat::Onnx,
        url: Some("https://huggingface.co/Xenova/segformer-b0-finetuned-ade-512-512/resolve/main/onnx/model.onnx"),
        files: &[],
        download_size_bytes: 15335446,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Embeddings ---
    CatalogEntry {
        id: "nemotron-3-embed-1b-q4_k_m",
        alias: Some("nemotron-3-embed"),
        name: "NVIDIA Nemotron 3 Embed 1B Q4_K_M",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/zenmagnets/Nemotron-3-Embed-1B-Q4_K_M-GGUF/resolve/06df1fde6f7009c91f6cc3cd520081921929a678/nemotron-3-embed-1b-q4_k_m.gguf"),
        files: &[],
        download_size_bytes: 749352096,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "llama-nemotron-embed-1b-v2-q4_k_m",
        alias: Some("llama-nemotron-embed"),
        name: "NVIDIA Llama Nemotron Embed 1B v2 Q4_K_M",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/mykor/llama-nemotron-embed-1b-v2-GGUF/resolve/bf7c9832b1d76f86777379e58b7b74805ee58006/llama-nemotron-embed-1B-v2-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 807690624,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // NVIDIA Llama Embed Nemotron 8B — portable GGUF previously HNPU-only.
    CatalogEntry {
        id: "llama-embed-nemotron-8b-q4_k_m",
        alias: Some("llama-embed-nemotron"),
        name: "NVIDIA Llama Embed Nemotron 8B Q4_K_M",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/mradermacher/llama-embed-nemotron-8b-GGUF/resolve/e7ae3cbae4f7693bbd75ec959bf293f39e1f2e25/llama-embed-nemotron-8b.Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 4625233184,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "all-minilm-l6-v2",
        alias: Some("minilm"),
        name: "All-MiniLM-L6-v2 (Embeddings)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::Onnx,
        format: v1::ModelFormat::Onnx,
        url: None,
        files: MINI_LM_FILES,
        download_size_bytes: 90 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Reranking (llama.cpp cross-encoder; `wally rerank -m <id>`) ---
    CatalogEntry {
        id: "bge-reranker-v2-m3-q4_k_m",
        alias: Some("bge-reranker"),
        name: "BGE Reranker v2-m3 Q4_K_M (Reranking)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::LlamaCpp,
        format: v1::ModelFormat::Gguf,
        url: Some("https://huggingface.co/gpustack/bge-reranker-v2-m3-GGUF/resolve/main/bge-reranker-v2-m3-Q4_K_M.gguf"),
        files: &[],
        download_size_bytes: 438376864,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- Image generation (CoreML diffusion; Apple only) ---
    // Apple-optimized Stable Diffusion 1.5. Id matches the built-in diffusion
    // model registry (diffusion_model_registry.cpp) and the Swift facade's
    // canonical `.imageGeneration` model, so `wally image generate` resolves
    // and auto-pulls it through that SDK-side registry regardless of this
    // catalog. IMAGE_GENERATION is not is_llm(), so the LLM-only cut means
    // this entry is never registered by register_all() and never appears in
    // `wally models list` (with or without --all), nor does it resolve
    // through `wally models pull <id>` -- it stays here only as the
    // documented source of its metadata for `wally image generate`'s default.
    // The palettized CoreML bundle is a directory of
    // compiled .mlmodelc sub-models served by the `coreml` engine; a
    // pre-fetched bundle can also be passed to `--model` as a local path.
    // The Hugging Face *repo page* is HTML (~160 KB) and is not a model.
    // Point at the compiled split-einsum zip (~1.5 GB).
    CatalogEntry {
        id: "stable-diffusion-v1-5-coreml",
        alias: Some("sd15"),
        name: "Stable Diffusion 1.5 (CoreML)",
        category: v1::ModelCategory::ImageGeneration,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/apple/coreml-stable-diffusion-v1-5-palettized/resolve/main/coreml-stable-diffusion-v1-5-palettized_split_einsum_v2_compiled.zip"),
        files: &[],
        download_size_bytes: 1500 * MB,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // NeuRT advertises LLM + STT + EMBED + RERANK + VLM + EMBED_IMAGE + DIFFUSION; folder refs (same ModelInfo
    // path as sd15). Pass a local compiled tree to `--model` — `wally models pull` of a
    // Hugging Face repo page is HTML, not a bundle.
    // TEMP(ane-cut): the two ANE LLM rows are out of the release. Both URLs
    // are Hugging Face repo *pages* (the repos hold fp16/ and int8/ trees, no
    // archive), so `models pull` cannot fetch them, and the public kit has no
    // NeuRT engine to run them. Uncomment this block, the `ane-` prefix in
    // find() below, and the test row in tests/test_wally_unit.cpp together
    // once real artifacts exist; nothing else has to change.
    // {"lfm2_5_230m_ane", "lfm2-230m-ane", "LiquidAI LFM2.5 230M",
    //  v1::MODEL_CATEGORY_LANGUAGE, v1::INFERENCE_FRAMEWORK_COREML,
    //  v1::MODEL_FORMAT_MLPACKAGE,
    //  "https://huggingface.co/runanywhere/LFM2.5-230M_ANE", nullptr, 0, 0, 0,
    //  false, 0, "", "lfm2.5-230m"},
    // {"lfm2_5_350m_ane", "lfm2-350m-ane", "LiquidAI LFM2.5 350M",
    //  v1::MODEL_CATEGORY_LANGUAGE, v1::INFERENCE_FRAMEWORK_COREML,
    //  v1::MODEL_FORMAT_MLPACKAGE,
    //  "https://huggingface.co/runanywhere/LFM2.5-350M_ANE", nullptr, 0, 0, 0,
    //  false, 0, "", "lfm2.5-350m"},
    // The first ANE EMBEDDING row. docs/BUNDLE_CONTRACT.md listed this exact bundle as the one
    // that "loads, undrivable" — its manifest parsed and its encoder graph bound, but the SDK's
    // neurt engine filled no embedding_ops, so nothing could drive it. Gate B on an M4 Max:
    // cosine vs the fp32 gold min 0.9588 / mean 0.9890, relevant ranked above irrelevant in 4/4
    // gold records. `wally embed --engine ane --model <local tree>`.
    //
    // ASYMMETRIC: the bundle declares "query: " / "passage: " prefixes and returns a materially
    // different vector per role. Measured here, prefixing correctly moves query-vs-relevant
    // separation from ~0.001 to 0.53 — so a caller that ignores input_type does not get a slightly
    // worse vector, it gets a useless one.
    CatalogEntry {
        id: "nemotron3_embed_1b_ane",
        alias: Some("nemotron3-embed-ane"),
        name: "Nemotron-3-Embed-1B (Apple Neural Engine)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/Nemotron-3-Embed-1B-BF16_ANE"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // The first ANE RERANK row. Its `score` graph role was outside NeuRT's manifest vocabulary, so
    // the published bundle was rejected before a graph was touched. Gate on an M4 Max: positive
    // beats negative on 5/5 gold triples, matching the reference. `wally rerank --engine ane`.
    //
    // Category note: MODEL_CATEGORY_RERANK (value 12) exists and is the right one. WALLY's other
    // reranker (bge-reranker-v2-m3, above) uses MODEL_CATEGORY_EMBEDDING — a pre-existing
    // inconsistency left alone here rather than changed as a drive-by.
    CatalogEntry {
        id: "nv_rerankqa_1b_v2_ane",
        alias: Some("nv-rerank-ane"),
        name: "NV-RerankQA-1B-v2 (Apple Neural Engine)",
        category: v1::ModelCategory::Rerank,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/llama-3.2-nv-rerankqa-1b-v2_ANE"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // The first ANE VLM row. Image + prompt -> text: the runtime runs the vision tower, splices its
    // 256 visual tokens over the prompt's <IMG_CONTEXT> positions, then drives the ordinary chunked
    // text decode. Gate on an M4 Max: reproduced all 3 gold generations EXACTLY, word for word,
    // including prompt-token counts (274/273/274). `wally vlm generate --engine ane`.
    CatalogEntry {
        id: "internvl3_5_1b_ane",
        alias: Some("internvl-1b-ane"),
        name: "InternVL3.5 1B (Apple Neural Engine)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/InternVL3_5-1B_ANE"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // The first ANE IMAGE-EMBEDDING row (pixels -> vector, for retrieval/similarity). Serves the
    // RAC_PRIMITIVE_EMBED_IMAGE slot promoted from reserved_slot_3 in ABI v10. Gate on an M4 Max:
    // cosine vs the fp32 gold min 0.9919 / mean 0.9979, and each augmented image ranks its original
    // first (2/2), matching the gold's reference_ranks.
    CatalogEntry {
        id: "siglip2_base_256_ane",
        alias: Some("siglip2-ane"),
        name: "SigLIP2 base-256 (Apple Neural Engine)",
        category: v1::ModelCategory::Vision,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/siglip2-base-patch16-256_ANE"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // The first ANE TEXT-TO-SPEECH row, and NeuRT's last null primitive filled (SDK 0.20.33).
    // Kokoro-82M across three Core ML graphs (duration -> decode -> gen) plus two host seams that
    // are not expressible as ANE ops: the duration->alignment expansion and the harmonic source.
    // Ships its own G2P lexicon, so no runtime phonemizer is needed.
    //
    // Measured on an M4 Max against the PUBLISHED bundle downloaded fresh: 129 ms of synthesis for
    // 4225 ms of audio (32.8x realtime), 24 kHz, no NaNs. Gate A mel distance 0.2832-0.3159, where
    // upstream Kokoro's own real-op CustomSTFT scores 0.289 against its complex path and two
    // DIFFERENT utterances score 2.63-2.71 -- i.e. at the vocoder's floor, not conversion loss.
    // The .zip, NOT the repo root. A bare huggingface.co/<org>/<repo> URL makes
    // `wally models pull` fetch the repo's HTML PAGE -- 120 KB of markup written to disk
    // under the model id, with a cheerful "done 100%". Every other ANE row here
    // still has that shape and is therefore listable but not pullable.
    CatalogEntry {
        id: "kokoro_82m_ane",
        alias: Some("kokoro-ane"),
        name: "Kokoro 82M (Apple Neural Engine)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/Kokoro-82M_ANE/resolve/main/kokoro-82m_ANE.zip"),
        files: &[],
        download_size_bytes: 153037101,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "parakeet_tdt_0_6b_v2_ane",
        alias: Some("parakeet-tdt-v2-ane"),
        name: "Parakeet TDT 0.6B v2 (Apple Neural Engine)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Coreml,
        format: v1::ModelFormat::Mlpackage,
        url: Some("https://huggingface.co/runanywhere/parakeet-tdt-0.6b-v2_ANE"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // --- MLX (Apple Silicon / Apple GPU via mlx-swift-lm) ---
    CatalogEntry {
        id: "mlx-qwen3-0.6b-4bit",
        alias: Some("mlx-qwen3"),
        name: "Qwen3 0.6B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN3_06_BFILES_FILES,
        download_size_bytes: 351383618,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("qwen3-0.6b"),
    },
    CatalogEntry {
        id: "mlx-maple-preview-2bit",
        alias: Some("mlx-maple-preview"),
        name: "DeepGrove Maple Preview",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_MAPLE_PREVIEW_FILES,
        download_size_bytes: 5330252282,
        context_length: 128000,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("maple-preview"),
    },
    CatalogEntry {
        id: "mlx-llama-3.1-nemotron-nano-8b-v1-4bit",
        alias: Some("mlx-nemotron-nano"),
        name: "NVIDIA Llama 3.1 Nemotron Nano 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_NEMOTRON_NANO8_BFILES_FILES,
        download_size_bytes: 4534806075,
        context_length: 131072,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("mlx-nemotron-nano"),
    },
    CatalogEntry {
        id: "mlx-nemotron-mini-4b-instruct-4bit",
        alias: Some("mlx-nemotron-mini"),
        name: "NVIDIA Nemotron Mini 4B Instruct",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_NEMOTRON_MINI4_BFILES_FILES,
        download_size_bytes: 2392679103,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("mlx-nemotron-mini"),
    },
    // PrismML Bonsai family 1-bit MLX. Needs the narrow Prism kernels carried
    // by the canonical-first RunAnywhere MLX/mlx-swift forks pinned in the
    // Swift manifests and resolved files.
    CatalogEntry {
        id: "mlx-bonsai-1.7b-1bit",
        alias: Some("mlx-bonsai-1.7b"),
        name: "Bonsai 1.7B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_BONSAI1_7_B1_BIT_FILES,
        download_size_bytes: 269060904,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-1.7b"),
    },
    CatalogEntry {
        id: "mlx-bonsai-4b-1bit",
        alias: Some("mlx-bonsai-4b"),
        name: "Bonsai 4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_BONSAI4_B1_BIT_FILES,
        download_size_bytes: 628865840,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-4b"),
    },
    CatalogEntry {
        id: "mlx-bonsai-8b-1bit",
        alias: Some("mlx-bonsai-8b"),
        name: "Bonsai 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_BONSAI8_B1_BIT_FILES,
        download_size_bytes: 1280131424,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-8b"),
    },
    // PrismML Bonsai-27B 1-bit MLX (~5.1 GB safetensors). Experimental —
    // requires mlx-swift-lm support for qwen3_5 / 1-bit Bonsai.
    CatalogEntry {
        id: "mlx-bonsai-27b-1bit",
        alias: Some("mlx-bonsai"),
        name: "Bonsai 27B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_BONSAI27_B1_BIT_FILES,
        download_size_bytes: 5129115752,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("bonsai-27b"),
    },
    // PrismML Ternary-Bonsai family at ternary/2-bit MLX. bits=2 was already
    // supported by upstream MLX 0.31.6 before the Prism 1-bit patch, so this
    // needs no additional fork support beyond what Bonsai (above) needs.
    // Verified this session: loaded + generated correctly via the app's
    // Add-from-URL flow (Ternary-Bonsai-1.7B, 64 tok/s, no crash).
    CatalogEntry {
        id: "mlx-ternary-bonsai-1.7b-2bit",
        alias: Some("mlx-ternary-bonsai-1.7b"),
        name: "Ternary-Bonsai 1.7B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_TERNARY_BONSAI1_7_B2_BIT_FILES,
        download_size_bytes: 484049216,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-1.7b"),
    },
    CatalogEntry {
        id: "mlx-ternary-bonsai-4b-2bit",
        alias: Some("mlx-ternary-bonsai-4b"),
        name: "Ternary-Bonsai 4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_TERNARY_BONSAI4_B2_BIT_FILES,
        download_size_bytes: 1131565944,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-4b"),
    },
    CatalogEntry {
        id: "mlx-ternary-bonsai-8b-2bit",
        alias: Some("mlx-ternary-bonsai-8b"),
        name: "Ternary-Bonsai 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_TERNARY_BONSAI8_B2_BIT_FILES,
        download_size_bytes: 2303661704,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-8b"),
    },
    // merge_key matches the bare id, same as every other Ternary-Bonsai size
    // above (1.7b/4b/8b) -- not the "mlx-" prefixed alias -- so a future GGUF
    // Ternary-Bonsai-27B row merges into this one row instead of listing twice.
    CatalogEntry {
        id: "mlx-ternary-bonsai-27b-2bit",
        alias: Some("mlx-ternary-bonsai-27b"),
        name: "Ternary-Bonsai 27B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_TERNARY_BONSAI27_B2_BIT_FILES,
        download_size_bytes: 8490785104,
        context_length: 4096,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("ternary-bonsai-27b"),
    },
    CatalogEntry {
        id: "mlx-llama-3.2-1b-instruct-4bit",
        alias: Some("mlx-llama3.2"),
        name: "Llama 3.2 1B Instruct",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_LLAMA32_1_BFILES_FILES,
        download_size_bytes: 712575975,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("mlx-llama3.2"),
    },
    CatalogEntry {
        id: "mlx-qwen2-vl-2b-instruct-4bit",
        alias: Some("mlx-qwen2-vl"),
        name: "Qwen2-VL 2B Instruct 4-bit (MLX)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN2_VL2_BFILES_FILES,
        download_size_bytes: 1261853827,
        context_length: 2048,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-fastvlm-0.5b-bf16",
        alias: Some("mlx-fastvlm"),
        name: "FastVLM 0.5B bf16 (MLX)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_FAST_VLM05_BFILES_FILES,
        download_size_bytes: 1256926974,
        context_length: 2048,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-lfm2.5-vl-3b-4bit",
        alias: Some("mlx-lfm2.5-vl"),
        name: "LFM2.5-VL 3B 4-bit (MLX)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_LFM2_5_VL3_BFILES_FILES,
        download_size_bytes: 2388258432,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-qwen3-embedding-0.6b-4bit-dwq",
        alias: Some("mlx-qwen3-embed"),
        name: "Qwen3 Embedding 0.6B 4-bit DWQ (MLX)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN3_EMBEDDING06_BFILES_FILES,
        download_size_bytes: 351230811,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-qwen3-asr-0.6b-8bit",
        alias: Some("mlx-qwen3-asr"),
        name: "Qwen3-ASR 0.6B 8-bit (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN3_ASR06_BFILES_FILES,
        download_size_bytes: 1010773761,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-glm-asr-nano-2512-4bit",
        alias: Some("mlx-glm-asr"),
        name: "GLM-ASR Nano 2512 4-bit (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GLM_ASR_NANO2512_FILES,
        download_size_bytes: 1288437789,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-parakeet-ctc-1.1b",
        alias: Some("mlx-parakeet-ctc"),
        name: "NVIDIA Parakeet CTC 1.1B (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_PARAKEET_CTC11_BFILES_FILES,
        download_size_bytes: 4250718357,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-parakeet-tdt-0.6b-v2",
        alias: Some("mlx-parakeet-tdt-v2"),
        name: "NVIDIA Parakeet TDT 0.6B v2 (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_PARAKEET_TDT_V2_FILES,
        download_size_bytes: 2471596080,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-parakeet-tdt-0.6b-v3",
        alias: Some("mlx-parakeet-tdt-v3"),
        name: "NVIDIA Parakeet TDT 0.6B v3 (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_PARAKEET_TDT_V3_FILES,
        download_size_bytes: 2508532829,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-parakeet-rnnt-1.1b",
        alias: Some("mlx-parakeet-rnnt"),
        name: "NVIDIA Parakeet RNNT 1.1B (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_PARAKEET_RNNT11_BFILES_FILES,
        download_size_bytes: 4282283914,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-nemotron-3.5-asr-streaming-0.6b-8bit",
        alias: Some("mlx-nemotron-asr"),
        name: "NVIDIA Nemotron 3.5 Streaming ASR 0.6B 8-bit (MLX)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_NEMOTRON_STREAMING_ASR_FILES,
        download_size_bytes: 755758528,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-qwen3-tts-12hz-0.6b-base-8bit",
        alias: Some("mlx-qwen3-tts"),
        name: "Qwen3-TTS 12Hz 0.6B Base 8-bit (MLX)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN3_TTS06_BBASE_FILES,
        download_size_bytes: 1991299138,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "mlx-soprano-1.1-80m-5bit",
        alias: Some("mlx-soprano"),
        name: "Soprano 1.1 80M 5-bit (MLX)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_SOPRANO1180_M5_BIT_FILES,
        download_size_bytes: 82220814,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    // Google Gemma 4 family (MLX). config.json model_type "gemma4" /
    // "gemma4_unified" (12B), both registered in the pinned mlx-swift-lm
    // 3.31.5 LLMTypeRegistry/VLMTypeRegistry — verified by reading the
    // checked-out package source this session (not assumed). Licensed under
    // Apache 2.0; preserve the upstream license and attribution notices.
    CatalogEntry {
        id: "mlx-gemma-4-e2b-it-4bit",
        alias: Some("mlx-gemma4-e2b"),
        name: "Gemma 4 E2B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GEMMA4_E2_BFILES_FILES,
        download_size_bytes: 3550670554,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-e2b"),
    },
    CatalogEntry {
        id: "mlx-gemma-4-e4b-it-qat-4bit",
        alias: Some("mlx-gemma4-e4b"),
        name: "Gemma 4 E4B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GEMMA4_E4_BFILES_FILES,
        download_size_bytes: 6798307742,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-e4b"),
    },
    CatalogEntry {
        id: "mlx-gemma-4-12b-it-qat-4bit",
        alias: Some("mlx-gemma4-12b"),
        name: "Gemma 4 12B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GEMMA4_12_BFILES_FILES,
        download_size_bytes: 10987772430,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-12b"),
    },
    CatalogEntry {
        id: "mlx-gemma-4-26b-a4b-it-4bit",
        alias: Some("mlx-gemma4-26b-a4b"),
        name: "Gemma 4 26B-A4B (MoE)",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GEMMA4_26_BA4_BFILES_FILES,
        download_size_bytes: 15341205776,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-26b-a4b"),
    },
    // The plain 4bit variant, NOT "-qat-4bit" — that name does not resolve to
    // a clean repo (verified this session); this is the largest dense Gemma 4.
    CatalogEntry {
        id: "mlx-gemma-4-31b-it-4bit",
        alias: Some("mlx-gemma4-31b"),
        name: "Gemma 4 31B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GEMMA4_31_BFILES_FILES,
        download_size_bytes: 18412016676,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("gemma-4-31b"),
    },
    // Qwen3.8-27B (dense) — config.json model_type "qwen3_5", registered.
    CatalogEntry {
        id: "mlx-qwen3.8-27b-4bit",
        alias: Some("mlx-qwen3.8-27b"),
        name: "Qwen3.8 27B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_QWEN3_8_27_BFILES_FILES,
        download_size_bytes: 16054541349,
        context_length: 262144,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("qwen3.8-27b"),
    },
    // IBM Granite 4.1 family (MLX). config.json model_type "granite",
    // registered in mlx-swift-lm 3.31.5's LLMTypeRegistry.
    CatalogEntry {
        id: "mlx-granite-4.1-3b-4bit",
        alias: Some("mlx-granite4.1-3b"),
        name: "IBM Granite 4.1 3B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GRANITE4_1_3_BFILES_FILES,
        download_size_bytes: 2127162429,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-3b"),
    },
    // A real, official mlx-community 8B 4-bit quant does exist (Apache-2.0,
    // model_type "granite") — verified via HF API this session, despite the
    // original assumption that none did; added for parity with 3B/30B.
    CatalogEntry {
        id: "mlx-granite-4.1-8b-4bit",
        alias: Some("mlx-granite4.1-8b"),
        name: "IBM Granite 4.1 8B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GRANITE4_1_8_BFILES_FILES,
        download_size_bytes: 5238406779,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-8b"),
    },
    CatalogEntry {
        id: "mlx-granite-4.1-30b-4bit",
        alias: Some("mlx-granite4.1-30b"),
        name: "IBM Granite 4.1 30B",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Mlx,
        format: v1::ModelFormat::Safetensors,
        url: None,
        files: MLX_GRANITE4_1_30_BFILES_FILES,
        download_size_bytes: 18041976573,
        context_length: 4096,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("granite-4.1-30b"),
    },
    // --- QHexRT (Snapdragon Hexagon NPU; Windows ARM64 overlay) ---
    // Ids match engines/qhexrt/qhexrt_model_catalog.cpp so pull/lifecycle
    // resolve the same native catalog the Android/Flutter apps use. Folder
    // URLs are registered as ModelInfo (same path as CoreML diffusion) —
    // the QNN context tree is fetched by the QHexRT bundle policy or passed
    // as a local `*_HNPU` directory to `wally run`.
    CatalogEntry {
        id: "lfm2_5_230m",
        alias: Some("lfm2-230m-npu"),
        name: "LiquidAI LFM2.5 230M",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/lfm2_5_230m_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-230m"),
    },
    CatalogEntry {
        id: "lfm2_5_350m",
        alias: Some("lfm2-350m-npu"),
        name: "LiquidAI LFM2.5 350M",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/lfm2_5_350m_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-350m"),
    },
    CatalogEntry {
        id: "lfm2_5_1_2b_thinking",
        alias: Some("lfm2-1.2b-npu"),
        name: "LiquidAI LFM2.5 1.2B Thinking",
        category: v1::ModelCategory::Language,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/lfm2_5_1_2b_thinking_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: true,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: Some("lfm2.5-1.2b"),
    },
    // Non-LLM Hexagon primitives. Ids match engines/qhexrt/qhexrt_model_catalog.cpp.
    // Same folder-URL registration as the LLM rows — pass a local `*_HNPU`
    // directory; do not expect `wally models pull` to fetch the HF repo HTML.
    CatalogEntry {
        id: "whisper_base",
        alias: Some("whisper-base-npu"),
        name: "Whisper Base (Hexagon NPU)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/whisper_base_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "moonshine_tiny",
        alias: Some("moonshine-tiny-npu"),
        name: "Moonshine Tiny (Hexagon NPU)",
        category: v1::ModelCategory::SpeechRecognition,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/moonshine_tiny_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "kitten_micro_0_8",
        alias: Some("kitten-micro-npu"),
        name: "Kitten TTS Micro 0.8 (Hexagon NPU)",
        category: v1::ModelCategory::SpeechSynthesis,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/kitten_micro_0_8_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "embeddinggemma_300m",
        alias: Some("embeddinggemma-npu"),
        name: "EmbeddingGemma 300M (Hexagon NPU)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/embeddinggemma_300m_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "internvl3_5_1b",
        alias: Some("internvl-1b-npu"),
        name: "InternVL3.5 1B (Hexagon NPU)",
        category: v1::ModelCategory::Multimodal,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/internvl3_5_1b_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "cosmos3_edge_diffusion",
        alias: Some("cosmos3-diffusion-npu"),
        name: "Cosmos3-Edge Diffusion (Hexagon NPU)",
        category: v1::ModelCategory::ImageGeneration,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/cosmos3_edge_image_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
    CatalogEntry {
        id: "nv_rerankqa_1b",
        alias: Some("nv-rerank-npu"),
        name: "NV-RerankQA 1B (Hexagon NPU)",
        category: v1::ModelCategory::Embedding,
        framework: v1::InferenceFramework::Qhexrt,
        format: v1::ModelFormat::QnnContext,
        url: Some("https://huggingface.co/runanywhere/nv_rerankqa_1b_HNPU"),
        files: &[],
        download_size_bytes: 0,
        context_length: 0,
        supports_thinking: false,
        memory_required_bytes: 0,
        cua_profile: "",
        merge_key: None,
    },
];

// AUTO-TRANSLITERATED DATA END
// LLM-only cut: the catalog surfaces language models only. Every other
// modality's entries still live in CATALOG above, but are filtered out here,
// so `models list`, lookups, suggestions and SDK registration all see LLMs
// only. Delete is_llm and its four uses below to restore the full catalog.
fn is_llm(entry: &CatalogEntry) -> bool {
    let _ = entry;
    true // TEMP(full-surface test): every modality listed
}

// MLX is an Apple-only backend. On any other platform its entries are hidden
// and never registered, so a Windows or Linux user cannot list, resolve, or
// download a model they could never run.
// llama.cpp, the Apple Neural Engine (Core ML via NeuRT) and QHexRT are gated
// by the linked kit's own capability macros rather than by host OS/arch:
// WALLY_HAS_LLAMACPP / WALLY_HAS_NEURT / WALLY_HAS_QHEXRT come from
// wally_define_engine_macros() (cmake/RunAnywhereSDK.cmake), set from the
// consumed kit's RunAnywhere_HAS_* config. The public windows-arm64 kit ships
// no llama.cpp backend (docs/ENGINES.md); NeuRT and QHexRT are private overlay
// packs (AGENTS.md), so the public Apple kit has no engine that can load a
// Core ML LLM even though the host is a Mac. Gating ANE on __APPLE__ used to
// list `ane-lfm2.5-350m` on that kit: `models pull` saved the Hugging Face repo
// page as the model and `run` then handed the folder to MLX, which failed on a
// missing config.json. Reading the linked kit's own macros tracks the real
// per-build matrix instead of guessing it from __APPLE__/_WIN32.
fn platform_supports(framework: v1::InferenceFramework) -> bool {
    match framework {
        v1::InferenceFramework::Mlx => cfg!(target_os = "macos"),
        v1::InferenceFramework::Coreml => cfg!(wally_has_neurt),
        v1::InferenceFramework::LlamaCpp => cfg!(wally_has_llamacpp),
        v1::InferenceFramework::Qhexrt => cfg!(wally_has_qhexrt),
        _ => true,
    }
}

// The one predicate every surface filters on: an LLM this platform can run.
fn listed(entry: &CatalogEntry) -> bool {
    is_llm(entry) && platform_supports(entry.framework)
}

/// All built-in entries.
pub fn all() -> &'static [CatalogEntry] {
    // A contiguous, LLM-only view built once; callers get the same stable
    // slice every call, matching the C++ `static const std::vector` cache.
    static LLM_ONLY: OnceLock<Vec<CatalogEntry>> = OnceLock::new();
    LLM_ONLY.get_or_init(|| CATALOG.iter().copied().filter(listed).collect())
}

/// Exact id or alias lookup.
pub fn find(id_or_alias: &str) -> Option<&'static CatalogEntry> {
    for entry in CATALOG {
        if !listed(entry) {
            continue;
        }
        if entry.id == id_or_alias || entry.alias == Some(id_or_alias) {
            return Some(entry);
        }
    }

    // Predictable per-backend names for a merged row. `models list` shows one id
    // per model (the shared merge_key); each backend's build is that id with a
    // backend prefix — `mlx-<id>`, `ane-<id>`, `npu-<id>` — so a reader never has
    // to guess the old alias. Only reached when the exact match above missed.
    const BACKEND_PREFIXES: &[(&str, v1::InferenceFramework)] = &[
        ("mlx-", v1::InferenceFramework::Mlx),
        // TEMP(ane-cut): no ANE rows are listed, so `ane-<id>` resolves to
        // nothing. Restore with the rows above.
        // ("ane-", v1::InferenceFramework::Coreml),
        ("npu-", v1::InferenceFramework::Qhexrt),
    ];
    for (prefix, framework) in BACKEND_PREFIXES {
        let Some(base) = id_or_alias.strip_prefix(prefix) else {
            continue;
        };
        for entry in CATALOG {
            if !listed(entry) || entry.framework != *framework {
                continue;
            }
            let key = entry.merge_key.unwrap_or(entry.id);
            if base == key {
                return Some(entry);
            }
        }
    }
    None
}

/// Closest-match candidates for error messages (substring match, ≤ max).
pub fn suggestions(input: &str, max: usize) -> Vec<String> {
    let mut matches = Vec::new();
    for entry in CATALOG {
        if matches.len() >= max {
            break;
        }
        if !listed(entry) {
            continue;
        }
        if entry.id.contains(input) || entry.alias.is_some_and(|alias| alias.contains(input)) {
            matches.push(entry.id.to_string());
        }
    }
    matches
}

/// The merge base for a registry id: the entry's merge_key when set, else the
/// id itself.
pub fn merge_key_for(id: &str) -> String {
    for entry in CATALOG {
        if id == entry.id {
            return entry.merge_key.unwrap_or(entry.id).to_string();
        }
    }
    id.to_string()
}

// CoreML bundles (a directory of compiled .mlmodelc sub-models) don't fit the
// URL / multi-file download-factory grammar, which rejects a bare repo ref.
// Register the ModelInfo directly so the id resolves in the general registry
// (and `wally models list --all` shows it, since it is catalog-only until
// downloaded); the bundle itself is fetched by the diffusion pipeline or
// supplied to `wally image --model <local path>`.
fn register_entry(entry: &CatalogEntry) -> sys::rac_result_t {
    if entry.framework == v1::InferenceFramework::Coreml
        || entry.framework == v1::InferenceFramework::Qhexrt
    {
        // CoreML bundles and QHexRT HNPU folders don't fit the single-file
        // download-factory grammar. Register ModelInfo so `wally models list --all`
        // / `wally run` resolve the id; the tree is fetched by the engine or passed
        // as a local path.
        let model = v1::ModelInfo {
            id: entry.id.to_string(),
            name: entry.name.to_string(),
            category: entry.category as i32,
            framework: entry.framework as i32,
            format: entry.format as i32,
            download_url: entry.url.unwrap_or_default().to_string(),
            download_size_bytes: entry.download_size_bytes,
            source: v1::ModelSource::Remote as i32,
            ..Default::default()
        };
        let bytes = serialize(&model);
        // SAFETY: `bytes` is alive for the duration of the call; the registry
        // handle is the process-global singleton the kit owns.
        return unsafe {
            sys::rac_model_registry_register_proto(
                sys::rac_get_model_registry(),
                bytes.as_ptr(),
                bytes.len(),
            )
        };
    }

    let mut out = ProtoBuffer::new();
    let rc = if !entry.files.is_empty() {
        let mut request = v1::RegisterMultiFileModelRequest {
            id: entry.id.to_string(),
            name: entry.name.to_string(),
            framework: entry.framework as i32,
            category: Some(entry.category as i32),
            format: Some(entry.format as i32),
            download_size_bytes: Some(entry.download_size_bytes),
            ..Default::default()
        };
        if entry.memory_required_bytes > 0 {
            request.memory_required_bytes = Some(entry.memory_required_bytes);
        }
        if entry.context_length > 0 {
            request.context_length = Some(entry.context_length);
        }
        if entry.supports_thinking {
            request.supports_thinking = Some(true);
        }
        if !entry.cua_profile.is_empty() {
            request.cua_profile = Some(entry.cua_profile.to_string());
        }
        request.files = entry
            .files
            .iter()
            .map(|file| v1::ModelFileDescriptor {
                url: file.url.to_string(),
                filename: file.filename.to_string(),
                is_optional: !file.required,
                size_bytes: (file.size_bytes > 0).then_some(file.size_bytes),
                checksum_sha256: file.checksum_sha256.map(str::to_string),
                ..Default::default()
            })
            .collect();
        let bytes = serialize(&request);
        // SAFETY: `bytes` is alive for the call; `out` is a freshly-initialised buffer.
        unsafe {
            sys::rac_register_multi_file_model_proto(bytes.as_ptr(), bytes.len(), out.as_mut_ptr())
        }
    } else {
        let mut request = v1::RegisterModelFromUrlRequest {
            url: entry.url.unwrap_or_default().to_string(),
            name: entry.name.to_string(),
            id: Some(entry.id.to_string()),
            framework: Some(entry.framework as i32),
            category: Some(entry.category as i32),
            download_size_bytes: Some(entry.download_size_bytes),
            ..Default::default()
        };
        if entry.context_length > 0 {
            request.context_length = Some(entry.context_length);
        }
        if entry.supports_thinking {
            request.supports_thinking = Some(true);
        }
        let bytes = serialize(&request);
        // SAFETY: `bytes` is alive for the call; `out` is a freshly-initialised buffer.
        unsafe {
            sys::rac_register_model_from_url_proto(bytes.as_ptr(), bytes.len(), out.as_mut_ptr())
        }
    };

    // The saved ModelInfo bytes are not needed here — only the status envelope.
    // `out`'s Drop frees the buffer (replaces the C++ rac_proto_buffer_free call).
    if rc == sys::SUCCESS {
        out.status()
    } else {
        rc
    }
}

/// Register every entry with the global model registry. Logs (does not fail
/// on) individual rejections so one bad entry can't take the CLI down.
pub fn register_all() -> sys::rac_result_t {
    let mut first_error = sys::SUCCESS;
    for entry in CATALOG {
        if !listed(entry) {
            continue;
        }
        let rc = register_entry(entry);
        if rc != sys::SUCCESS {
            status_line(&format!(
                "warning: catalog registration failed for {}: {}",
                entry.id,
                describe_result(rc)
            ));
            if first_error == sys::SUCCESS {
                first_error = rc;
            }
        }
    }
    first_error
}
