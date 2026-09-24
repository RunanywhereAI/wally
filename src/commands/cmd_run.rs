//! Port of src/commands/cmd_run.cpp. Owner: the run/llm/tool/serve port.
use crate::cli::App;

use super::{LlmVerb, ModelArg};

pub fn register_llm(app: &mut App) {
    let _ = app;
    todo!("run/llm/tool/serve port: register_llm")
}

pub fn register_vlm(app: &mut App) {
    let _ = app;
    todo!("run/llm/tool/serve port: register_vlm")
}

pub fn register_llm_aliases(app: &mut App) {
    let _ = app;
    todo!("run/llm/tool/serve port: register_llm_aliases")
}

pub fn configure_llm(cmd: &mut App, verb: LlmVerb, model_arg: ModelArg) {
    let _ = cmd;
    todo!("run port: configure_llm ({verb:?}, {model_arg:?})")
}

pub fn configure_vlm_generate(cmd: &mut App) {
    let _ = cmd;
    todo!("run port: configure_vlm_generate")
}
