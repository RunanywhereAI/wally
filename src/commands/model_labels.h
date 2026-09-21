#pragma once

#include "model_types.pb.h"

namespace wally::commands::model_labels {

namespace v1 = runanywhere::v1;

inline const char* category(v1::ModelCategory category) {
    switch (category) {
        case v1::MODEL_CATEGORY_LANGUAGE:
            return "llm";
        case v1::MODEL_CATEGORY_MULTIMODAL:
        case v1::MODEL_CATEGORY_VISION:
            return "vlm";
        case v1::MODEL_CATEGORY_SPEECH_RECOGNITION:
            return "stt";
        case v1::MODEL_CATEGORY_SPEECH_SYNTHESIS:
            return "tts";
        case v1::MODEL_CATEGORY_VOICE_ACTIVITY_DETECTION:
            return "vad";
        case v1::MODEL_CATEGORY_EMBEDDING:
            return "embedding";
        case v1::MODEL_CATEGORY_SPEAKER_DIARIZATION:
            return "diarize";
        case v1::MODEL_CATEGORY_SEMANTIC_SEGMENTATION:
            return "segment";
        case v1::MODEL_CATEGORY_IMAGE_GENERATION:
            return "diffusion";
        case v1::MODEL_CATEGORY_AUDIO:
            return "audio";
        default:
            return "?";
    }
}

inline const char* backend(v1::InferenceFramework framework) {
    switch (framework) {
        case v1::INFERENCE_FRAMEWORK_ONNX:
            return "ONNX Runtime";
        case v1::INFERENCE_FRAMEWORK_LLAMA_CPP:
            return "llama.cpp";
        case v1::INFERENCE_FRAMEWORK_FOUNDATION_MODELS:
            return "Apple Foundation";
        case v1::INFERENCE_FRAMEWORK_SYSTEM_TTS:
            return "System TTS";
        case v1::INFERENCE_FRAMEWORK_FLUID_AUDIO:
            return "Fluid Audio";
        case v1::INFERENCE_FRAMEWORK_COREML:
            return "Core ML";
        case v1::INFERENCE_FRAMEWORK_MLX:
            return "MLX";
        case v1::INFERENCE_FRAMEWORK_TFLITE:
            return "TensorFlow Lite";
        case v1::INFERENCE_FRAMEWORK_EXECUTORCH:
            return "ExecuTorch";
        case v1::INFERENCE_FRAMEWORK_MEDIAPIPE:
            return "MediaPipe";
        case v1::INFERENCE_FRAMEWORK_MLC:
            return "MLC";
        case v1::INFERENCE_FRAMEWORK_PICO_LLM:
            return "Pico LLM";
        case v1::INFERENCE_FRAMEWORK_PIPER_TTS:
            return "Piper TTS";
        case v1::INFERENCE_FRAMEWORK_SWIFT_TRANSFORMERS:
            return "Swift Transformers";
        case v1::INFERENCE_FRAMEWORK_BUILT_IN:
            return "Built-in";
        case v1::INFERENCE_FRAMEWORK_NONE:
            return "None";
        case v1::INFERENCE_FRAMEWORK_UNKNOWN:
            return "Unknown";
        case v1::INFERENCE_FRAMEWORK_SHERPA:
            return "Sherpa-ONNX";
        case v1::INFERENCE_FRAMEWORK_QHEXRT:
            return "QHexRT";
        case v1::INFERENCE_FRAMEWORK_UNSPECIFIED:
            return "Unspecified";
        default:
            return "?";
    }
}

// Compact backend tags for the merged `models list` BACKEND column, where one
// model's per-backend variants collapse to a single row (e.g. "mlx/llama.cpp").
inline const char* short_backend(v1::InferenceFramework framework) {
    switch (framework) {
        case v1::INFERENCE_FRAMEWORK_MLX:
            return "mlx";
        case v1::INFERENCE_FRAMEWORK_LLAMA_CPP:
            return "llama.cpp";
        case v1::INFERENCE_FRAMEWORK_COREML:
            return "ane";
        case v1::INFERENCE_FRAMEWORK_QHEXRT:
            return "npu";
        case v1::INFERENCE_FRAMEWORK_ONNX:
            return "onnx";
        default:
            return backend(framework);
    }
}

inline const char* format(v1::ModelFormat format) {
    switch (format) {
        case v1::MODEL_FORMAT_GGUF:
            return "GGUF";
        case v1::MODEL_FORMAT_GGML:
            return "GGML";
        case v1::MODEL_FORMAT_ONNX:
            return "ONNX";
        case v1::MODEL_FORMAT_ORT:
            return "ORT";
        case v1::MODEL_FORMAT_BIN:
            return "BIN";
        case v1::MODEL_FORMAT_COREML:
            return "Core ML";
        case v1::MODEL_FORMAT_MLMODEL:
            return "MLModel";
        case v1::MODEL_FORMAT_MLPACKAGE:
            return "MLPackage";
        case v1::MODEL_FORMAT_TFLITE:
            return "TFLite";
        case v1::MODEL_FORMAT_SAFETENSORS:
            return "SafeTensors";
        case v1::MODEL_FORMAT_QNN_CONTEXT:
            return "QNN Context";
        case v1::MODEL_FORMAT_ZIP:
            return "ZIP";
        case v1::MODEL_FORMAT_FOLDER:
            return "Folder";
        case v1::MODEL_FORMAT_PROPRIETARY:
            return "Proprietary";
        case v1::MODEL_FORMAT_UNKNOWN:
            return "Unknown";
        default:
            return "?";
    }
}

}  // namespace wally::commands::model_labels
