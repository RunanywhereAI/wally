//! `wally llm tool-call` — exercise the tool-calling loop end to end. Port of
//! src/commands/cmd_tool.cpp. Owner: the run/llm/tool/serve port.
//!
//! Thin wrapper over rac_tool_calling_run_loop_proto: load an LLM, hand
//! commons a prompt plus two built-in demo tools (get_weather, calculate),
//! and let commons drive the whole decide -> call -> execute -> synthesize
//! loop. The host executor here returns canned JSON so the loop can complete
//! offline; the point is to see whether a given model actually emits a
//! well-formed tool call and whether commons parses it and produces a
//! grounded final answer.
//!
//! `on_execute` / `on_handle_published` are both documented as invoked
//! synchronously, on the calling thread, before
//! `rac_tool_calling_run_loop_proto` returns -- unlike the LLM streaming
//! callback (Triage A1 in cmd_run.rs), there is no possibility of either
//! running after this function has returned, so no Arc/quiesce handoff is
//! needed here. Each body still runs inside `catch_unwind`, since a panic
//! must never cross the FFI boundary regardless of the call's threading
//! model.

use std::ffi::c_void;

use crate::bootstrap::{self, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output::{error_line, result_line, status_line, JsonWriter};
use crate::io::proto::{self, parse_proto_buffer, v1, ProtoBuffer};
use crate::progress::progress_bar::DownloadProgressScope;
use crate::sys;

use super::engine_options::resolve_engine_hint;

// Small instruct GGUF that ships in the built-in catalog. Override with
// --model (e.g. `lfm2-350m-q8_0` to exercise the LFM2 tool-call format path).
const DEFAULT_TOOL_MODEL: &str = "qwen3-0.6b";

#[derive(Debug, Clone, Default)]
struct ToolCallParams {
    prompt: String,
    model: String,
    engine: String,
    /// auto | required | none | specific (default auto)
    tool_choice: String,
    /// name for tool_choice=specific
    force_tool: String,
    max_tool_calls: i32,
}

fn parse_tool_choice(mode: &str) -> Option<v1::ToolChoiceMode> {
    match mode {
        "" | "auto" => Some(v1::ToolChoiceMode::Auto),
        "required" => Some(v1::ToolChoiceMode::Required),
        "none" => Some(v1::ToolChoiceMode::None),
        "specific" => Some(v1::ToolChoiceMode::Specific),
        _ => None,
    }
}

// ToolParameter is gone: ToolDefinition.parameters is now one OpenAI-style
// JSON Schema object describing all of a tool's arguments (the same shape
// solutions.proto's ToolSpec already carries).
fn single_string_param_schema(name: &str, description: &str) -> String {
    format!(
        "{{\"type\":\"object\",\"properties\":{{\"{name}\":{{\"type\":\"string\",\"description\":\"{description}\"}}}},\"required\":[\"{name}\"]}}"
    )
}

/// Synchronous host executor: commons hands us a serialized ToolCall and
/// expects an owned serialized ToolResult back. Echoes the call to stderr and
/// returns a canned result per tool so the loop can synthesize a final answer
/// offline.
extern "C" fn demo_executor(
    in_bytes: *const u8,
    in_size: usize,
    out_result: *mut sys::rac_proto_buffer_t,
    user_data: *mut c_void,
) -> sys::rac_result_t {
    let outcome = std::panic::catch_unwind(|| {
        let _ = user_data;
        let call = if in_bytes.is_null() || in_size == 0 {
            v1::ToolCall::default()
        } else {
            // SAFETY: in_bytes/in_size describe a buffer valid for the
            // duration of this call, per rac_tool_execute_callback_fn's
            // synchronous, on-calling-thread contract.
            let bytes = unsafe { std::slice::from_raw_parts(in_bytes, in_size) };
            <v1::ToolCall as prost::Message>::decode(bytes).unwrap_or_default()
        };
        status_line(&format!(
            "  executing {}({})",
            call.name, call.arguments_json
        ));

        let mut result = v1::ToolResult {
            tool_call_id: call.id.clone(),
            name: call.name.clone(),
            is_error: false,
            ..Default::default()
        };
        result.result_json = match call.name.as_str() {
            "get_weather" => "{\"temperature_c\":18,\"condition\":\"cloudy\"}".to_string(),
            "calculate" => "{\"note\":\"demo executor does not evaluate expressions\"}".to_string(),
            _ => "{\"ok\":true}".to_string(),
        };

        let bytes = proto::serialize(&result);
        // SAFETY: out_result is a live rac_proto_buffer_t handed to us by the
        // SDK for this call; init then copy is the documented pattern for
        // filling an out-param buffer the caller owns.
        unsafe {
            sys::rac_proto_buffer_init(out_result);
        }
        let data_ptr = if bytes.is_empty() {
            std::ptr::null()
        } else {
            bytes.as_ptr()
        };
        // SAFETY: data_ptr/len describe `bytes`, which outlives this call;
        // out_result was just initialized above.
        unsafe { sys::rac_proto_buffer_copy(data_ptr, bytes.len(), out_result) }
    });
    // On a panic, a generic failure code; the specific value doesn't matter
    // since run_tool_call reports from the parsed ToolCallingResult envelope,
    // not from this raw rc.
    outcome.unwrap_or(-1)
}

/// `rac_tool_calling_run_loop_proto` invokes this synchronously and
/// unconditionally (it is not null-checked), so a real no-op is required even
/// when the CLI has no use for the cancellable handle.
extern "C" fn ignore_published_handle(_handle: u64, _user_data: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {});
}

fn load_model(options: &GlobalOptions, model_id: &str, framework: v1::InferenceFramework) -> bool {
    let _progress_scope =
        DownloadProgressScope::new(model_id, !options.no_progress && !options.json);
    let mut request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        validate_availability: true,
        ..Default::default()
    };
    if framework != v1::InferenceFramework::Unspecified {
        request.framework = Some(framework as i32);
    }
    let bytes = proto::serialize(&request);

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result: v1::ModelLoadResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        // parse_proto_buffer only populates an error detail on its own
        // failure path; when the buffer parsed cleanly but the call's own rc
        // is a failure, C++ prints the still-empty `error` string here, so
        // match that with no trailing detail rather than describe_result(rc).
        Ok(_) => {
            error_line("model load failed: ");
            return false;
        }
        Err(message) => {
            error_line(&format!("model load failed: {message}"));
            return false;
        }
    };
    if let Some(err) = &result.error {
        let message = if err.message.is_empty() {
            "unknown error"
        } else {
            &err.message
        };
        error_line(&format!("model load failed: {message}"));
        return false;
    }
    true
}

fn run_tool_call(options: &GlobalOptions, params: &ToolCallParams) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    let mut engine_hint = match resolve_engine_hint(&params.engine) {
        Ok(hint) => hint,
        Err(message) => {
            error_line(&message);
            return 2;
        }
    };
    engine_hint.resolve_options.has_category = true;
    engine_hint.resolve_options.category = v1::ModelCategory::Language;

    let resolved = match model_ref::resolve(&params.model, Some(&engine_hint.resolve_options)) {
        Ok(resolved) => resolved,
        Err((_, message)) => {
            error_line(&message);
            return 1;
        }
    };

    let load_framework = if resolved.from_catalog {
        v1::InferenceFramework::Unspecified
    } else {
        engine_hint.framework
    };
    if !load_model(options, &resolved.model_id, load_framework) {
        return 1;
    }

    // ToolCallingSessionCreateRequest collapsed to {prompt, history, options}:
    // max_tokens/auto_execute/max_tool_calls/tools/tool_choice/forced_tool_name
    // all now live on the nested ToolCallingOptions (there is no standalone
    // max_tokens/temperature slot anywhere on this request any more --
    // sampling for the tool loop comes from the enclosing generation state's
    // own defaults).
    let mut tool_options = v1::ToolCallingOptions {
        auto_execute: Some(true),
        ..Default::default()
    };
    if params.max_tool_calls > 0 {
        tool_options.max_tool_calls = Some(params.max_tool_calls);
    }
    tool_options.tools.push(v1::ToolDefinition {
        name: "get_weather".to_string(),
        description: "Get the current weather for a city".to_string(),
        parameters: single_string_param_schema("location", "City name, e.g. Tokyo"),
        ..Default::default()
    });
    tool_options.tools.push(v1::ToolDefinition {
        name: "calculate".to_string(),
        description: "Evaluate an arithmetic expression".to_string(),
        parameters: single_string_param_schema("expression", "Expression such as 45 * 12"),
        ..Default::default()
    });

    let choice = match parse_tool_choice(&params.tool_choice) {
        Some(choice) => choice,
        None => {
            error_line("--tool-choice expects auto|required|none|specific");
            return 2;
        }
    };
    if !params.force_tool.is_empty() {
        tool_options.tool_choice = v1::ToolChoiceMode::Specific as i32;
        tool_options.forced_tool_name = Some(params.force_tool.clone());
    } else if choice != v1::ToolChoiceMode::Auto {
        tool_options.tool_choice = choice as i32;
    }

    let request = v1::ToolCallingSessionCreateRequest {
        prompt: params.prompt.clone(),
        options: Some(tool_options),
        ..Default::default()
    };
    let bytes = proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized; demo_executor/ignore_published_handle are extern "C"
    // fns matching the declared callback types and are invoked synchronously
    // on this thread before the call returns.
    let rc = unsafe {
        sys::rac_tool_calling_run_loop_proto(
            bytes.as_ptr(),
            bytes.len(),
            Some(demo_executor),
            std::ptr::null_mut(),
            Some(ignore_published_handle),
            std::ptr::null_mut(),
            out_buffer.as_mut_ptr(),
        )
    };

    // The run loop writes a structured ToolCallingResult even when it returns
    // a non-success rc (e.g. a generation failure lands in
    // error_code/error_message), so parse the envelope first and report from
    // it rather than the bare rc.
    let result: v1::ToolCallingResult = match parse_proto_buffer(out_buffer) {
        Ok(result) => result,
        Err(message) => {
            let message = if message.is_empty() {
                rc.to_string()
            } else {
                message
            };
            error_line(&format!("tool-calling failed: {message}"));
            return 1;
        }
    };

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object()
            .field_str("text", &result.text)
            .field_bool("is_complete", result.is_complete)
            .field_i64("iterations", result.iterations_used as i64)
            .field_i64("tool_calls", result.tool_calls.len() as i64)
            .end_object();
        result_line(json.str());
        return if result.is_complete { 0 } else { 1 };
    }

    for (index, call) in result.tool_calls.iter().enumerate() {
        status_line(&format!(
            "tool call {}: {}({})",
            index + 1,
            call.name,
            call.arguments_json
        ));
    }
    if result.error_code != 0 {
        error_line(&format!(
            "tool-calling error: {}",
            result.error_message.as_deref().unwrap_or("")
        ));
    }
    status_line(&format!(
        "iterations: {}, tool calls: {}",
        result.iterations_used,
        result.tool_calls.len()
    ));
    result_line(&result.text);
    if result.is_complete {
        0
    } else {
        1
    }
}

pub fn configure_tool_call(cmd: &mut App) {
    cmd.add_option("prompt", ValueType::Text, "What to ask the model")
        .required();
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("Model to use (default {DEFAULT_TOOL_MODEL})"),
    )
    .default_val(DEFAULT_TOOL_MODEL);
    cmd.add_option("--engine", ValueType::Text, "Engine to run on");
    cmd.add_option(
        "--tool-choice",
        ValueType::Text,
        "When the model may call tools: auto, required, none, specific",
    );
    cmd.add_option(
        "--force-tool",
        ValueType::Text,
        "Force one tool by name (implies --tool-choice specific)",
    );
    cmd.add_option(
        "--max-tool-calls",
        ValueType::Int,
        "Cap on tool calls per turn (default 3)",
    )
    .default_val("3");
    cmd.callback(move |p, options| {
        let params = ToolCallParams {
            prompt: p.get_str("prompt").unwrap_or_default(),
            model: p.get_str("--model").unwrap_or_default(),
            engine: p.get_str("--engine").unwrap_or_default(),
            tool_choice: p.get_str("--tool-choice").unwrap_or_default(),
            force_tool: p.get_str("--force-tool").unwrap_or_default(),
            max_tool_calls: p.get_i64("--max-tool-calls").unwrap_or(3) as i32,
        };
        run_tool_call(options, &params)
    });
}

pub fn register_tool(app: &mut App) {
    // Tool calling is an LLM capability, so it lives under the `llm`
    // namespace that register_llm() already created (register_llm runs
    // first: app.rs / register order, not something this function checks).
    let ns = app
        .get_subcommand_mut("llm")
        .expect("register_llm must run before register_tool");
    configure_tool_call(
        ns.add_subcommand("tool-call", "Try tool calling with two demo tools")
            .footer(&examples_footer(&[
                Example::new(
                    "wally llm tool-call \"weather in Paris?\"",
                    "Calls get_weather",
                ),
                Example::new(
                    "wally llm tool-call \"what is 19 * 23?\"",
                    "Calls calculate",
                ),
            ])),
    );
}
