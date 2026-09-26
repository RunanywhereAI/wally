//! `--engine` hint parsing (port of src/commands/engine_options.cpp). Owner:
//! the run/llm/tool/serve port.

use crate::catalog::model_ref::ResolveOptions;
use crate::io::proto::v1;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineHintResolution {
    pub framework: v1::InferenceFramework,
    pub resolve_options: ResolveOptions,
}

impl Default for EngineHintResolution {
    fn default() -> Self {
        EngineHintResolution {
            framework: v1::InferenceFramework::Unspecified,
            resolve_options: ResolveOptions::default(),
        }
    }
}

pub fn parse_engine_hint(engine: &str) -> Result<v1::InferenceFramework, String> {
    let normalized = engine.to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(v1::InferenceFramework::Unspecified);
    }
    if normalized == "mlx" {
        return Ok(v1::InferenceFramework::Mlx);
    }
    // The Apple engine. Its identity is `neurt` (the runtime that implements it);
    // the FRAMEWORK it maps onto is still COREML, because that is what the model
    // files are and there is no NEURT value in InferenceFramework. `coreml` is
    // accepted as an alias: it is the engine's former name and remains the honest
    // name of the framework, so a user typing either means the same thing.
    // Only a kit that linked NeuRT can honour it (bootstrap.cpp registers the
    // plugin under the same macro); anywhere else the name is refused up front
    // rather than letting the load fall through to MLX and fail on a Core ML
    // tree with a confusing "config.json not found".
    if normalized == "neurt"
        || normalized == "coreml"
        || normalized == "core-ml"
        || normalized == "ane"
    {
        #[cfg(wally_has_neurt)]
        {
            return Ok(v1::InferenceFramework::Coreml);
        }
        #[cfg(not(wally_has_neurt))]
        {
            return Err(format!(
                "engine '{engine}' (Apple Neural Engine) is not in this build"
            ));
        }
    }
    if normalized == "llamacpp"
        || normalized == "llama.cpp"
        || normalized == "llama_cpp"
        || normalized == "llama-cpp"
    {
        return Ok(v1::InferenceFramework::LlamaCpp);
    }
    if normalized == "onnx" {
        return Ok(v1::InferenceFramework::Onnx);
    }
    if normalized == "sherpa" {
        return Ok(v1::InferenceFramework::Sherpa);
    }
    if normalized == "qhexrt"
        || normalized == "qnn"
        || normalized == "npu"
        || normalized == "hexagon"
        || normalized == "hexagon-npu"
    {
        return Ok(v1::InferenceFramework::Qhexrt);
    }
    Err(format!("unsupported engine '{engine}'"))
}

/// The `--engine` values this build accepts, for help text: "mlx, llamacpp,
/// onnx, sherpa", with NeuRT and QHexRT names added only when the kit linked
/// them. Keeps every command's help in step with parse_engine_hint().
pub fn engine_choices() -> &'static str {
    // Built from the same kit macros parse_engine_hint() gates on, so a help
    // page never advertises an engine this binary cannot register.
    #[cfg(all(wally_has_neurt, wally_has_qhexrt))]
    {
        "neurt|coreml|ane, mlx, llamacpp, onnx, sherpa, qhexrt"
    }
    #[cfg(all(wally_has_neurt, not(wally_has_qhexrt)))]
    {
        "neurt|coreml|ane, mlx, llamacpp, onnx, sherpa"
    }
    #[cfg(all(not(wally_has_neurt), wally_has_qhexrt))]
    {
        "mlx, llamacpp, onnx, sherpa, qhexrt"
    }
    #[cfg(all(not(wally_has_neurt), not(wally_has_qhexrt)))]
    {
        "mlx, llamacpp, onnx, sherpa"
    }
}

pub fn resolve_engine_hint(engine: &str) -> Result<EngineHintResolution, String> {
    let mut resolution = EngineHintResolution {
        framework: parse_engine_hint(engine)?,
        resolve_options: ResolveOptions::default(),
    };
    if resolution.framework != v1::InferenceFramework::Unspecified {
        resolution.resolve_options.has_framework = true;
        resolution.resolve_options.framework = resolution.framework;
    }
    Ok(resolution)
}
