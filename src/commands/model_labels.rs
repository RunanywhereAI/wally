//! Display labels for model categories, backends and formats (port of
//! src/commands/model_labels.h).

use crate::io::proto::v1;
use v1::{InferenceFramework as F, ModelCategory as C, ModelFormat as M};

pub fn category(category: C) -> &'static str {
    match category {
        C::Language => "llm",
        C::Multimodal | C::Vision => "vlm",
        C::SpeechRecognition => "stt",
        C::SpeechSynthesis => "tts",
        C::VoiceActivityDetection => "vad",
        C::Embedding => "embedding",
        C::SpeakerDiarization => "diarize",
        C::SemanticSegmentation => "segment",
        C::ImageGeneration => "diffusion",
        C::Audio => "audio",
        _ => "?",
    }
}

pub fn backend(framework: F) -> &'static str {
    match framework {
        F::Onnx => "ONNX Runtime",
        F::LlamaCpp => "llama.cpp",
        F::FoundationModels => "Apple Foundation",
        F::SystemTts => "System TTS",
        F::FluidAudio => "Fluid Audio",
        F::Coreml => "Core ML",
        F::Mlx => "MLX",
        F::Tflite => "TensorFlow Lite",
        F::Executorch => "ExecuTorch",
        F::Mediapipe => "MediaPipe",
        F::Mlc => "MLC",
        F::PicoLlm => "Pico LLM",
        F::PiperTts => "Piper TTS",
        F::SwiftTransformers => "Swift Transformers",
        F::BuiltIn => "Built-in",
        F::None => "None",
        F::Unknown => "Unknown",
        F::Sherpa => "Sherpa-ONNX",
        F::Qhexrt => "QHexRT",
        F::Unspecified => "Unspecified",
        #[allow(unreachable_patterns)]
        _ => "?",
    }
}

pub fn short_backend(framework: F) -> &'static str {
    match framework {
        F::Mlx => "mlx",
        F::LlamaCpp => "llama.cpp",
        F::Coreml => "ane",
        F::Qhexrt => "npu",
        F::Onnx => "onnx",
        _ => backend(framework),
    }
}

pub fn format(format: M) -> &'static str {
    match format {
        M::Gguf => "GGUF",
        M::Ggml => "GGML",
        M::Onnx => "ONNX",
        M::Ort => "ORT",
        M::Bin => "BIN",
        M::Coreml => "Core ML",
        M::Mlmodel => "MLModel",
        M::Mlpackage => "MLPackage",
        M::Tflite => "TFLite",
        M::Safetensors => "SafeTensors",
        M::QnnContext => "QNN Context",
        M::Zip => "ZIP",
        M::Folder => "Folder",
        M::Proprietary => "Proprietary",
        M::Unknown => "Unknown",
        _ => "?",
    }
}

/// The same three labels from a raw proto enum value (prost stores enums as
/// i32 in messages); an unknown value is "?" as in the C++ `default:` arm.
pub fn category_i32(value: i32) -> &'static str {
    C::try_from(value).map(category).unwrap_or("?")
}

pub fn backend_i32(value: i32) -> &'static str {
    F::try_from(value).map(backend).unwrap_or("?")
}

pub fn short_backend_i32(value: i32) -> &'static str {
    F::try_from(value).map(short_backend).unwrap_or("?")
}

pub fn format_i32(value: i32) -> &'static str {
    M::try_from(value).map(format).unwrap_or("?")
}
