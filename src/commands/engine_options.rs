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
    todo!("run port: parse_engine_hint ({engine})")
}

pub fn engine_choices() -> &'static str {
    todo!("run port: engine_choices")
}

pub fn resolve_engine_hint(engine: &str) -> Result<EngineHintResolution, String> {
    todo!("run port: resolve_engine_hint ({engine})")
}
