//! `wally rerank <query>` — score documents against a query, best first. Port
//! of src/commands/cmd_rerank.cpp. Owner: the dormant vision/text modalities
//! port. Not registered in the app (LLM-only release); ported completely
//! anyway per the migration brief.

use std::ffi::CString;

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, Validator, ValueType};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{describe_result, error_line, result_line, table, JsonWriter};
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;

fn read_text_file(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|_| format!("cannot open file: {path}"))
}

#[allow(clippy::too_many_arguments)]
fn run_rerank(
    options: &GlobalOptions,
    model_ref: &str,
    query: &str,
    docs: &[String],
    files: &[String],
    top_n: i64,
) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    if model_ref.is_empty() {
        error_line("--model is required (a reranker model id or on-disk path)");
        return 2;
    }

    let mut documents: Vec<String> = docs.to_vec();
    for file in files {
        match read_text_file(file) {
            Ok(content) => documents.push(content),
            Err(error) => {
                error_line(&error);
                return 2;
            }
        }
    }
    if documents.is_empty() {
        error_line("at least one document is required (--doc or --file)");
        return 2;
    }

    let model = match ensure_model_ready(options, model_ref) {
        Ok(model) => model,
        Err(exit_code) => return exit_code,
    };

    let mut handle: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: `handle` is a valid out-pointer for the duration of the call.
    let rc = unsafe { sys::rac_rerank_component_create(&mut handle) };
    if rc != sys::SUCCESS || handle.is_null() {
        error_line("failed to create rerank component");
        return 1;
    }

    let model_path = CString::new(model.primary_path.as_str());
    let model_id = CString::new(model.model_id.as_str());
    let model_name = CString::new(model.display_name.as_str());
    let (model_path, model_id, model_name) = match (model_path, model_id, model_name) {
        (Ok(p), Ok(i), Ok(n)) => (p, i, n),
        _ => {
            error_line("model path, id or name contains a NUL byte");
            // SAFETY: handle was just created above.
            unsafe { sys::rac_rerank_component_destroy(handle) };
            return 1;
        }
    };
    // SAFETY: handle is the live component created above; the three C strings
    // outlive this call.
    let rc = unsafe {
        sys::rac_rerank_component_load_model(
            handle,
            model_path.as_ptr(),
            model_id.as_ptr(),
            model_name.as_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        error_line(&format!("failed to load reranker: {}", describe_result(rc)));
        // SAFETY: handle is the live component created above.
        unsafe { sys::rac_rerank_component_destroy(handle) };
        return 1;
    }

    let mut request = v1::RerankRequest {
        query: query.to_string(),
        documents: documents.clone(),
        ..Default::default()
    };
    if top_n > 0 {
        request.options = Some(v1::RerankOptions {
            top_n: top_n as u32,
            ..Default::default()
        });
    }

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: handle is the live, loaded component; bytes/out_buffer are valid
    // for the duration of the call.
    let proto_rc = unsafe {
        sys::rac_rerank_component_rerank_proto(
            handle,
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::RerankResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            error_line(&format!("rerank failed: {}", describe_result(proto_rc)));
            // SAFETY: handle is the live component created above.
            unsafe { sys::rac_rerank_component_destroy(handle) };
            return 1;
        }
        Err(error) => {
            error_line(&format!("rerank failed: {error}"));
            // SAFETY: handle is the live component created above.
            unsafe { sys::rac_rerank_component_destroy(handle) };
            return 1;
        }
    };
    // SAFETY: handle is the live component created above; not used again.
    unsafe { sys::rac_rerank_component_destroy(handle) };

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        let model_field = if result.model_id.is_empty() {
            model.model_id.as_str()
        } else {
            result.model_id.as_str()
        };
        json.field_str("model", model_field)
            .field_i64("processing_time_ms", result.processing_time_ms);
        json.begin_array("results");
        // items() is pre-sorted by score descending; the loop position IS the
        // rank (RerankScoredItem carries no rank field of its own).
        for (i, item) in result.items.iter().enumerate() {
            json.begin_array_object();
            json.field_i64("index", i64::from(item.index))
                .field_i64("rank", (i + 1) as i64)
                .field_f64("relevance_score", f64::from(item.relevance_score));
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
    } else {
        let header = ["RANK", "INDEX", "SCORE", "DOCUMENT"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let mut rows = Vec::with_capacity(result.items.len());
        for (i, item) in result.items.iter().enumerate() {
            let doc = documents
                .get(item.index as usize)
                .map(String::as_str)
                .unwrap_or("");
            let doc_bytes = doc.as_bytes();
            let preview = if doc_bytes.len() > 60 {
                let cut = &doc_bytes[..57.min(doc_bytes.len())];
                format!("{}...", String::from_utf8_lossy(cut))
            } else {
                doc.to_string()
            };
            rows.push(vec![
                (i + 1).to_string(),
                item.index.to_string(),
                format!("{:.4}", item.relevance_score),
                preview,
            ]);
        }
        table(&header, &rows);
    }
    0
}

pub fn register_rerank(app: &mut App) {
    let cmd = app.add_subcommand("rerank", "Score documents against a query, best first");
    cmd.add_option(
        "query",
        ValueType::Text,
        "Query the documents are scored against",
    )
    .required();
    cmd.add_option("--model,-m", ValueType::Text, "Reranker model id or on-disk path")
        .required();
    cmd.add_option(
        "--doc,-d",
        ValueType::Text,
        "Document text to score; repeat for several",
    )
    .multi();
    cmd.add_option(
        "--file,-f",
        ValueType::Text,
        "Text file to score; repeat for several",
    )
    .multi();
    // 0 or negative used to reach run_rerank unrejected and fall through the
    // `top_n > 0` guard there, silently returning every document instead of
    // the usage error a nonsensical count should be.
    cmd.add_option(
        "--top-n",
        ValueType::Int,
        "Return only this many best matches",
    )
    .check(Validator::Range(1, i64::from(i32::MAX)));

    cmd.callback(|p, g| {
        run_rerank(
            g,
            &p.get_str("--model").unwrap_or_default(),
            &p.get_str("query").unwrap_or_default(),
            &p.get_strs("--doc"),
            &p.get_strs("--file"),
            p.get_i64("--top-n").unwrap_or(0),
        )
    });
}
