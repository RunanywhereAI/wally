//! `wally models show <model>` (alias `wally models get`) — registry entry
//! details.

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, ValueType};
use crate::commands::model_labels;
use crate::commands::model_setup::refresh_registry;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, v1, ProtoBuffer};
use crate::sys;

fn run_show(options: &GlobalOptions, reference: &str) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let resolved = match model_ref::resolve(reference, None) {
        Ok(resolved) => resolved,
        Err((_, error)) => {
            out::error_line(&error);
            return 1;
        }
    };

    // Link on-disk artifacts (incl. sidecar-less rig-placed files) before
    // reading state — mirrors list/ensure_model_ready so show reports the
    // same downloaded state.
    if let Err(error) = refresh_registry() {
        out::status_line(&format!("warning: registry refresh failed: {error}"));
    }

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle; out_buffer is a valid out-param for this call only.
    let model_id_c = std::ffi::CString::new(resolved.model_id.as_str()).unwrap_or_default();
    let get_rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(sys::rac_get_model_registry(), model_id_c.as_ptr(), out_buffer.as_mut_ptr())
    };
    // parse unconditionally: it interprets the {status,error_message}
    // envelope and frees the buffer on every path (no leak on get failure).
    let model: v1::ModelInfo = match parse_proto_buffer(out_buffer) {
        Ok(model) if get_rc == sys::SUCCESS => model,
        Ok(_) => {
            out::error_line(&format!("model not found: {}", resolved.model_id));
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("model not found: {} ({error})", resolved.model_id));
            return 1;
        }
    };

    let downloaded = model.registry_status == Some(v1::ModelRegistryStatus::Downloaded as i32) || !model.local_path.is_empty();

    if options.json {
        let category = v1::ModelCategory::try_from(model.category).unwrap_or(v1::ModelCategory::Unspecified);
        let framework = v1::InferenceFramework::try_from(model.framework).unwrap_or(v1::InferenceFramework::Unspecified);
        let format = v1::ModelFormat::try_from(model.format).unwrap_or(v1::ModelFormat::Unspecified);
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_str("id", &model.id)
            .field_str("name", &model.name)
            .field_str("category", model_labels::category(category))
            .field_str("framework", model_labels::backend(framework))
            .field_str("backend", model_labels::backend(framework))
            .field_str("format", model_labels::format(format))
            .field_str("download_url", &model.download_url)
            .field_str("local_path", &model.local_path)
            .field_i64("size_bytes", model.download_size_bytes)
            .field_i64("context_length", model.context_length as i64)
            .field_bool("supports_thinking", model.supports_thinking)
            .field_bool("downloaded", downloaded);
        if let Some(v1::model_info::Artifact::MultiFile(multi)) = &model.artifact {
            json.begin_array("files");
            for file in &multi.files {
                json.begin_array_object().field_str("filename", &file.filename).field_str("url", &file.url).end_object();
            }
            json.end_array();
        }
        json.end_object();
        out::result_line(json.str());
        return 0;
    }

    out::result_line(&format!("id          {}", model.id));
    out::result_line(&format!("name        {}", model.name));
    let framework = v1::InferenceFramework::try_from(model.framework).unwrap_or(v1::InferenceFramework::Unspecified);
    out::result_line(&format!("backend     {}", model_labels::backend(framework)));
    if model.download_size_bytes > 0 {
        out::result_line(&format!("size        {}", out::human_bytes(model.download_size_bytes as u64)));
    }
    if model.context_length > 0 {
        out::result_line(&format!("context     {}", model.context_length));
    }
    if let Some(v1::model_info::Artifact::MultiFile(multi)) = &model.artifact {
        // Filenames only; the download URL is provenance that means nothing to
        // the reader and stays in --json for tooling.
        for file in &multi.files {
            out::result_line(&format!("file        {}", file.filename));
        }
    }
    out::result_line(&format!("downloaded  {}", if downloaded { "yes" } else { "no" }));
    if !model.local_path.is_empty() {
        out::result_line(&format!("path        {}", model.local_path));
    }
    if model.supports_thinking {
        out::result_line("thinking    yes");
    }
    0
}

pub fn configure_models_get(cmd: &mut App) {
    cmd.add_option("model", ValueType::Text, "Model id, alias or URL").required();
    cmd.callback(|p, g| {
        let reference = p.get_str("model").unwrap_or_default();
        run_show(g, &reference)
    });
}
