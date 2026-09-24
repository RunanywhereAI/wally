//! `wally lora apply|remove|list|catalog` — LoRA adapters on the loaded LLM.
//! Port of src/commands/cmd_lora.cpp. Not registered in the app (LLM-only
//! release), as in the C++.
//!
//! `list` reports the adapters currently attached (rac_lora_state_proto) — the
//! spec's LoraState — while `catalog` lists registered adapter metadata. This
//! file only translates argv <-> proto bytes, per the repo layering rule.
//!
//! `import` is retired: idl/lora_options.proto deleted
//! LoraAdapterImportRequest/Result outright, and commons permanently stubs
//! rac_lora_adapter_import_proto to RAC_ERROR_NOT_IMPLEMENTED. No replacement
//! verb exists yet.

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, ValueType};
use crate::io::output::{error_line, result_line, table, JsonWriter};
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;

fn require_lora_registry() -> Option<sys::rac_lora_registry_handle_t> {
    // SAFETY: rac_get_lora_registry() returns the process-wide LoRA registry
    // handle (valid once the SDK is initialized); it takes no arguments and
    // has no output parameters to validate.
    let registry = unsafe { sys::rac_get_lora_registry() };
    if registry.is_null() {
        error_line("LoRA registry unavailable (SDK not initialized)");
        return None;
    }
    Some(registry)
}

fn run_lora_catalog(options: &GlobalOptions) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }
    let registry = match require_lora_registry() {
        Some(registry) => registry,
        None => return 1,
    };

    let request = v1::LoraAdapterCatalogListRequest::default();
    let request_bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: registry is the live handle returned by require_lora_registry;
    // request_bytes is valid for its own length; out_buffer.as_mut_ptr() is a
    // valid, initialized rac_proto_buffer_t for the call.
    let rc = unsafe {
        sys::rac_lora_catalog_list_proto(
            registry,
            request_bytes.as_ptr(),
            request_bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // Fails on a parse error, a bad rc, OR the result itself carrying an
    // error (result.has_error()) -- all three are folded into one branch
    // here, mirroring the C++ `||` chain.
    let result = match parse_proto_buffer::<v1::LoraAdapterCatalogListResult>(out_buffer) {
        Ok(result) if rc == sys::SUCCESS && result.error.is_none() => result,
        Ok(result) => {
            let message = result.error.map(|e| e.message).unwrap_or_default();
            error_line(&format!("list failed: {message}"));
            return 1;
        }
        Err(error) => {
            error_line(&format!("list failed: {error}"));
            return 1;
        }
    };

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_i64("count", result.entries.len() as i64);
        json.begin_array("entries");
        for entry in &result.entries {
            let downloaded = entry.local_path.as_deref().is_some_and(|p| !p.is_empty());
            json.begin_array_object();
            json.field_str("id", &entry.id)
                .field_str("name", &entry.name)
                .field_bool("downloaded", downloaded)
                .field_str("local_path", entry.local_path.as_deref().unwrap_or(""));
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
        return 0;
    }
    if result.entries.is_empty() {
        result_line("no LoRA adapters registered");
        return 0;
    }
    for entry in &result.entries {
        let downloaded = entry.local_path.as_deref().is_some_and(|p| !p.is_empty());
        let suffix = if downloaded { "  [downloaded]" } else { "" };
        result_line(&format!("{}  {}{}", entry.id, entry.name, suffix));
    }
    0
}

fn print_lora_state(options: &GlobalOptions, state: &v1::LoraState) {
    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_str(
            "base_model_id",
            state.base_model_id.as_deref().unwrap_or(""),
        );
        json.begin_array("applied");
        for adapter in &state.loaded_adapters {
            json.begin_array_object();
            json.field_str("id", &adapter.adapter_id)
                .field_str("path", &adapter.adapter_path)
                .field_f64("scale", f64::from(adapter.scale))
                .field_bool("applied", adapter.applied);
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
        return;
    }
    if state.loaded_adapters.is_empty() {
        result_line("no adapters applied");
        return;
    }
    let header = ["ADAPTER", "SCALE", "APPLIED"]
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut rows = Vec::with_capacity(state.loaded_adapters.len());
    for adapter in &state.loaded_adapters {
        let name = if adapter.adapter_id.is_empty() {
            adapter.adapter_path.clone()
        } else {
            adapter.adapter_id.clone()
        };
        // std::to_string(float) in C++ is printf("%f", ...): fixed, 6 decimals.
        rows.push(vec![
            name,
            format!("{:.6}", adapter.scale),
            if adapter.applied {
                "yes".to_string()
            } else {
                "no".to_string()
            },
        ]);
    }
    table(&header, &rows);
}

fn run_lora_list(options: &GlobalOptions) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    // rac_lora_state_proto ignores its input payload (state is a pure read);
    // an empty serialized LoraState is the canonical "no request" shape.
    let empty_request = v1::LoraState::default();
    let request_bytes = serialize(&empty_request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: request_bytes is valid for its own length; out_buffer.as_mut_ptr()
    // is a valid, initialized rac_proto_buffer_t for the call.
    let rc = unsafe {
        sys::rac_lora_state_proto(
            request_bytes.as_ptr(),
            request_bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // The C++ error text here is a bare "prefix: " + error, with `error` left
    // empty when parsing succeeded but rc failed (not the describe_result
    // fallback other commands use) -- ported literally.
    let state = match parse_proto_buffer::<v1::LoraState>(out_buffer) {
        Ok(state) if rc == sys::SUCCESS => state,
        Ok(_) => {
            error_line("cannot read LoRA state: ");
            return 1;
        }
        Err(error) => {
            error_line(&format!("cannot read LoRA state: {error}"));
            return 1;
        }
    };
    print_lora_state(options, &state);
    0
}

fn run_lora_remove(options: &GlobalOptions, adapter: &str) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    // LoraRemoveRequest.adapter_paths was deleted: adapter_ids is the only
    // identity path now besides clear_all, so a bare path can no longer be
    // named directly here.
    let mut request = v1::LoraRemoveRequest::default();
    if adapter.is_empty() {
        request.clear_all = true;
    } else {
        request.adapter_ids.push(adapter.to_string());
    }

    let request_bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: request_bytes is valid for its own length; out_buffer.as_mut_ptr()
    // is a valid, initialized rac_proto_buffer_t for the call.
    let rc = unsafe {
        sys::rac_lora_remove_proto(
            request_bytes.as_ptr(),
            request_bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // Same bare-prefix quirk as run_lora_list: ported literally.
    let state = match parse_proto_buffer::<v1::LoraState>(out_buffer) {
        Ok(state) if rc == sys::SUCCESS => state,
        Ok(_) => {
            error_line("remove failed: ");
            return 1;
        }
        Err(error) => {
            error_line(&format!("remove failed: {error}"));
            return 1;
        }
    };
    if let Some(error) = &state.error {
        error_line(&format!("remove failed: {}", error.message));
        return 1;
    }
    print_lora_state(options, &state);
    0
}

// Load an LLM through the model-lifecycle service so rac_lora_apply_proto can
// acquire it. validate_availability=true auto-pulls the model if missing.
fn load_llm_for_lora(_options: &GlobalOptions, model_id: &str) -> bool {
    let request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        category: Some(v1::ModelCategory::Language as i32),
        validate_availability: true,
        ..Default::default()
    };

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle (valid once bootstrap() has run); bytes is valid for its own
    // length; out_buffer.as_mut_ptr() is a valid, initialized
    // rac_proto_buffer_t for the call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // Same bare-prefix quirk as run_lora_list: ported literally.
    let result = match parse_proto_buffer::<v1::ModelLoadResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            error_line("LLM load failed: ");
            return false;
        }
        Err(error) => {
            error_line(&format!("LLM load failed: {error}"));
            return false;
        }
    };
    if let Some(error) = &result.error {
        let message = if error.message.is_empty() {
            "unknown error"
        } else {
            error.message.as_str()
        };
        error_line(&format!("LLM load failed: {message}"));
        return false;
    }
    true
}

fn run_lora_apply(options: &GlobalOptions, model_id: &str, adapter_path: &str, scale: f32) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    // Checked before the (potentially multi-gigabyte) base model load: a
    // missing adapter file has nothing to do with the model, and loading it
    // first only to fail on the adapter afterward reports "failed to load the
    // model" for a problem that was never about the model.
    let is_regular_file = std::fs::metadata(adapter_path)
        .map(|meta| meta.is_file())
        .unwrap_or(false);
    if !is_regular_file {
        error_line(&format!("adapter file not found: {adapter_path}"));
        return 1;
    }

    if !load_llm_for_lora(options, model_id) {
        return 1;
    }

    // keep_existing left unset (false): SET semantics -- `adapters` becomes the
    // complete active set, matching the removed explicit replace_existing(true).
    let request = v1::LoraApplyRequest {
        adapters: vec![v1::LoraAdapterConfig {
            adapter_path: Some(adapter_path.to_string()),
            scale: Some(scale),
            ..Default::default()
        }],
        ..Default::default()
    };

    let request_bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: request_bytes is valid for its own length; out_buffer.as_mut_ptr()
    // is a valid, initialized rac_proto_buffer_t for the call.
    let rc = unsafe {
        sys::rac_lora_apply_proto(
            request_bytes.as_ptr(),
            request_bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // Same bare-prefix quirk as run_lora_list: ported literally.
    let result = match parse_proto_buffer::<v1::LoraApplyResult>(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        Ok(_) => {
            error_line("apply failed: ");
            return 1;
        }
        Err(error) => {
            error_line(&format!("apply failed: {error}"));
            return 1;
        }
    };
    if let Some(error) = &result.error {
        let message = if error.message.is_empty() {
            error.c_abi_code.unwrap_or(0).to_string()
        } else {
            error.message.clone()
        };
        error_line(&format!("apply failed: {message}"));
        return 1;
    }

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_bool("success", result.error.is_none())
            .field_i64("adapters", result.adapters.len() as i64);
        json.end_object();
        result_line(json.str());
    } else {
        result_line(&format!(
            "applied {} adapter(s) to {model_id}",
            result.adapters.len()
        ));
    }
    0
}

pub fn register_lora(app: &mut App) {
    let cmd = app.add_subcommand("lora", "Attach LoRA adapters to a language model");
    cmd.require_subcommand(1, 1);

    let apply_cmd = cmd.add_subcommand("apply", "Attach an adapter to a model, loading both");
    apply_cmd
        .add_option(
            "adapter",
            ValueType::Text,
            "Path to the adapter file (.gguf)",
        )
        .required();
    apply_cmd
        .add_option(
            "--model,-m",
            ValueType::Text,
            "LLM to attach the adapter to",
        )
        .required();
    apply_cmd
        .add_option(
            "--scale",
            ValueType::Float,
            "How strongly the adapter applies (default 1.0)",
        )
        .default_val("1.0");
    apply_cmd.callback(|p, g| {
        run_lora_apply(
            g,
            &p.get_str("--model").unwrap_or_default(),
            &p.get_str("adapter").unwrap_or_default(),
            p.get_f64("--scale").unwrap_or(1.0) as f32,
        )
    });

    let remove_cmd = cmd.add_subcommand("remove", "Detach one adapter, or every adapter");
    remove_cmd.add_option(
        "adapter",
        ValueType::Text,
        "Adapter id or path (omit to detach all)",
    );
    remove_cmd.callback(|p, g| run_lora_remove(g, &p.get_str("adapter").unwrap_or_default()));

    let list_cmd = cmd.add_subcommand("list", "Show the adapters currently attached");
    list_cmd.callback(|_p, g| run_lora_list(g));

    let catalog_cmd = cmd.add_subcommand("catalog", "List adapters registered with the SDK");
    catalog_cmd.callback(|_p, g| run_lora_catalog(g));

    // `import` is retired: idl/lora_options.proto deleted
    // LoraAdapterImportRequest/Result outright (adapter files are acquired
    // through the models domain's download/import verbs now), and
    // rac_lora_adapter_import_proto in commons is a permanent stub returning
    // RAC_ERROR_NOT_IMPLEMENTED (see lora_import.cpp). No replacement verb
    // exists on this namespace yet, so the command is left out per this
    // package's own rule: never wire a flag/verb the C ABI cannot serve.
}
