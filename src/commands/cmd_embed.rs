//! `wally embed [input]` — text embeddings via the commons lifecycle path.
//! Port of src/commands/cmd_embed.cpp. Owner: the dormant vision/text
//! modalities port. Not registered in the app (LLM-only release); ported
//! completely anyway per the migration brief.

use crate::bootstrap::{self, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, Validator, ValueType};
use crate::commands::engine_options::{engine_choices, resolve_engine_hint};
use crate::io::output::{error_line, result_line, status_line, JsonWriter};
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::progress::progress_bar::DownloadProgressScope;
use crate::sys;

const DEFAULT_EMBEDDING_MODEL: &str = "minilm";

fn preview_values(vector: &v1::EmbeddingVector) -> String {
    let count = vector.values.len().min(8);
    vector.values[..count]
        .iter()
        .map(|v| format!("{v:.5}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn print_json_result(model_id: &str, texts: &[String], result: &v1::EmbeddingsResult) {
    let mut json = JsonWriter::new();
    json.begin_object();
    let model = match &result.model_id {
        Some(id) if !id.is_empty() => id.as_str(),
        _ => model_id,
    };
    json.field_str("model", model)
        .field_i64("dimension", i64::from(result.dimension))
        .field_i64("count", result.vectors.len() as i64)
        .field_i64("tokens_used", i64::from(result.tokens_used))
        .field_i64("total_ms", result.processing_time_ms);
    json.begin_array("vectors");
    // EmbeddingVector.text/dimension are gone: text is looked up by
    // input_index (the batch position this vector answers, always set) and
    // dimension is the one shared EmbeddingsResult.dimension above.
    for vector in &result.vectors {
        let index = vector.input_index as usize;
        let text = texts.get(index).map(String::as_str).unwrap_or("");
        json.begin_array_object();
        json.field_str("text", text)
            .field_i64("dimension", i64::from(result.dimension));
        json.begin_array("values");
        for value in &vector.values {
            json.value_f64(f64::from(*value));
        }
        json.end_array();
        json.end_object();
    }
    json.end_array();
    json.end_object();
    result_line(json.str());
}

fn print_text_result(model_id: &str, result: &v1::EmbeddingsResult, verbose: bool) {
    result_line(&format!("model\t{model_id}"));
    result_line(&format!("dimension\t{}", result.dimension));
    result_line(&format!("count\t{}", result.vectors.len()));
    for (i, vector) in result.vectors.iter().enumerate() {
        result_line(&format!("vector[{i}]\t{}", preview_values(vector)));
    }
    if verbose {
        status_line(&format!("({} ms)", result.processing_time_ms));
    }
}

fn load_embeddings_model(
    options: &GlobalOptions,
    model_id: &str,
    framework: v1::InferenceFramework,
) -> bool {
    let _progress_scope = DownloadProgressScope::new(model_id, !options.no_progress && !options.json);
    let mut request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        validate_availability: true,
        category: Some(v1::ModelCategory::Embedding as i32),
        ..Default::default()
    };
    if framework != v1::InferenceFramework::Unspecified {
        request.framework = Some(framework as i32);
    }

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry handle
    // (valid once bootstrap() has run); out_buffer.as_mut_ptr() is a valid
    // pointer to an initialized rac_proto_buffer_t for the duration of the call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::ModelLoadResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            error_line(&format!(
                "embedding model load failed: {}",
                crate::io::output::describe_result(proto_rc)
            ));
            return false;
        }
        Err(error) => {
            error_line(&format!("embedding model load failed: {error}"));
            return false;
        }
    };
    if let Some(error) = &result.error {
        let message = if error.message.is_empty() {
            "unknown error"
        } else {
            error.message.as_str()
        };
        error_line(&format!("embedding model load failed: {message}"));
        return false;
    }
    if options.verbose {
        status_line(&format!("loaded {}", result.resolved_path));
    }
    true
}

fn parse_normalize(mode: &str) -> Result<Option<bool>, ()> {
    if mode.is_empty() {
        return Ok(None);
    }
    match mode {
        "l2" => Ok(Some(true)),
        "none" => Ok(Some(false)),
        _ => Err(()),
    }
}

// `--input-type`. Asymmetric embedders prepend a different prompt for a query than for a
// document, and getting it wrong is silent: on Nemotron-3-Embed-1B the unprefixed pair separates
// a relevant passage from an irrelevant one by ~0.001, and the prefixed pair by 0.48. A model
// that declares no prompt table ignores this and returns the same vector either way.
fn parse_input_type(mode: &str) -> Result<v1::EmbeddingsInputType, ()> {
    match mode {
        "" => Ok(v1::EmbeddingsInputType::Unspecified),
        "query" => Ok(v1::EmbeddingsInputType::Query),
        "document" | "doc" => Ok(v1::EmbeddingsInputType::Document),
        _ => Err(()),
    }
}

fn parse_pooling(mode: &str) -> Result<v1::EmbeddingsPoolingStrategy, ()> {
    match mode {
        "" => Ok(v1::EmbeddingsPoolingStrategy::Unspecified),
        "mean" => Ok(v1::EmbeddingsPoolingStrategy::Mean),
        "cls" => Ok(v1::EmbeddingsPoolingStrategy::Cls),
        "last" => Ok(v1::EmbeddingsPoolingStrategy::Last),
        _ => Err(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_embed(
    options: &GlobalOptions,
    reference: &str,
    engine: &str,
    texts: &[String],
    normalize: &str,
    pooling: &str,
    input_type: &str,
) -> i32 {
    if bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    if texts.is_empty() {
        error_line("at least one text input is required");
        return 2;
    }

    let mut engine_hint = match resolve_engine_hint(engine) {
        Ok(hint) => hint,
        Err(error) => {
            error_line(&error);
            return 2;
        }
    };
    engine_hint.resolve_options.has_category = true;
    engine_hint.resolve_options.category = v1::ModelCategory::Embedding;

    let selected_ref = if reference.is_empty() {
        DEFAULT_EMBEDDING_MODEL
    } else {
        reference
    };
    let resolved = match model_ref::resolve(selected_ref, Some(&engine_hint.resolve_options)) {
        Ok(resolved) => resolved,
        Err((_, error)) => {
            error_line(&error);
            return 1;
        }
    };

    // An explicit --engine is honoured whatever the ref resolved to. This used to
    // read `resolved.from_catalog ? UNSPECIFIED : engine_hint.framework`, which
    // silently DISCARDED the flag for built-in catalog entries — contradicting the
    // `--engine` help text. When the flag is absent engine_hint.framework is
    // UNSPECIFIED, so catalog entries still fall back to their own declared
    // framework exactly as before. Mirrors cmd_run.cpp.
    if !load_embeddings_model(options, &resolved.model_id, engine_hint.framework) {
        return 1;
    }

    let normalize_value = match parse_normalize(normalize) {
        Ok(value) => value,
        Err(()) => {
            error_line(
                "--normalize expects l2|none, --pooling expects mean|cls|last, --input-type \
                 expects query|document",
            );
            return 2;
        }
    };
    let pooling_strategy = match parse_pooling(pooling) {
        Ok(value) => value,
        Err(()) => {
            error_line(
                "--normalize expects l2|none, --pooling expects mean|cls|last, --input-type \
                 expects query|document",
            );
            return 2;
        }
    };
    let input_type_value = match parse_input_type(input_type) {
        Ok(value) => value,
        Err(()) => {
            error_line(
                "--normalize expects l2|none, --pooling expects mean|cls|last, --input-type \
                 expects query|document",
            );
            return 2;
        }
    };

    let mut request = v1::EmbeddingsRequest {
        model_id: Some(resolved.model_id.clone()),
        texts: texts.to_vec(),
        ..Default::default()
    };
    if normalize_value.is_some()
        || pooling_strategy != v1::EmbeddingsPoolingStrategy::Unspecified
        || input_type_value != v1::EmbeddingsInputType::Unspecified
    {
        let mut opts = v1::EmbeddingsOptions::default();
        if let Some(value) = normalize_value {
            opts.normalize = Some(value);
        }
        if pooling_strategy != v1::EmbeddingsPoolingStrategy::Unspecified {
            opts.pooling = pooling_strategy as i32;
        }
        if input_type_value != v1::EmbeddingsInputType::Unspecified {
            opts.input_type = input_type_value as i32;
        }
        request.options = Some(opts);
    }

    let bytes = serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes is a valid slice for its own length; out_buffer.as_mut_ptr()
    // points at a live, initialized rac_proto_buffer_t.
    let proto_rc = unsafe {
        sys::rac_embeddings_embed_batch_lifecycle_proto(
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    // EmbeddingsResult carries no error field: failures travel out-of-band on
    // the rac_proto_buffer_t status envelope, already checked here.
    let result = match parse_proto_buffer::<v1::EmbeddingsResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            error_line(&format!(
                "embedding failed: {}",
                crate::io::output::describe_result(proto_rc)
            ));
            return 1;
        }
        Err(error) => {
            error_line(&format!("embedding failed: {error}"));
            return 1;
        }
    };

    if options.json {
        print_json_result(&resolved.model_id, texts, &result);
    } else {
        print_text_result(&resolved.model_id, &result, options.verbose);
    }
    0
}

pub fn register_embed(app: &mut App) {
    let cmd = app.add_subcommand("embed", "Turn text into embedding vectors");
    cmd.add_option("input", ValueType::Text, "Text to embed");
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        &format!("Embedding model to use (default: {DEFAULT_EMBEDDING_MODEL})"),
    )
    .default_val(DEFAULT_EMBEDDING_MODEL);
    cmd.add_option(
        "--engine",
        ValueType::Text,
        &format!(
            "Engine hint ({}). Honoured for catalog models too, not just URL/HF refs. Omit to \
             let catalog framework / plugin priority pick.",
            engine_choices()
        ),
    );
    cmd.add_option(
        "--text,-t",
        ValueType::Text,
        "Embed this text too; repeat to batch several",
    )
    .multi();
    cmd.add_option(
        "--normalize",
        ValueType::Text,
        "Scale vectors to unit length or leave them raw",
    )
    .check(Validator::IsMember(vec!["l2".to_string(), "none".to_string()]));
    cmd.add_option(
        "--pooling",
        ValueType::Text,
        "Collapse token vectors with this strategy",
    )
    .check(Validator::IsMember(vec![
        "mean".to_string(),
        "cls".to_string(),
        "last".to_string(),
    ]));
    cmd.add_option(
        "--input-type",
        ValueType::Text,
        "Which side of a retrieval pair this text is. Asymmetric models (Nemotron-3-Embed, bge, \
         e5, gte) embed a query and a document differently; symmetric models ignore it.",
    )
    .check(Validator::IsMember(vec![
        "query".to_string(),
        "document".to_string(),
        "doc".to_string(),
    ]));

    cmd.callback(|p, g| {
        let mut texts: Vec<String> = Vec::new();
        if let Some(positional) = p.get_str("input") {
            if !positional.is_empty() {
                texts.push(positional);
            }
        }
        texts.extend(p.get_strs("--text"));
        run_embed(
            g,
            &p.get_str("--model")
                .unwrap_or_else(|| DEFAULT_EMBEDDING_MODEL.to_string()),
            &p.get_str("--engine").unwrap_or_default(),
            &texts,
            &p.get_str("--normalize").unwrap_or_default(),
            &p.get_str("--pooling").unwrap_or_default(),
            &p.get_str("--input-type").unwrap_or_default(),
        )
    });
}
