//! `wally rag query` / `wally rag search` — retrieval-augmented generation via
//! the commons RAG session ABI. Port of src/commands/cmd_rag.cpp. Not
//! registered in the app (LLM-only release), as in the C++.
//!
//! Single-shot flow in one process (the CLI is stateless across invocations and
//! RAG indexes are in-memory only):
//!   rac_rag_session_create_proto(RAGConfiguration)   -> session handle
//!     -> rac_rag_ingest_proto(RAGDocument) per --doc / --file
//!     -> rac_rag_query_proto(RAGQueryOptions)          -> RAGResult
//!       or rac_rag_search_proto(RAGSearchRequest)     -> RAGSearchResponse
//!   rac_rag_session_destroy_proto(session)
//!
//! Commons resolves the embedding (+ optional LLM) model ids to filesystem paths
//! via the model registry and owns the pipeline; this command only translates
//! argv to the rac_rag_* C ABI and renders the result.
//!
//! `cmd_rag.cpp` defines `register_rag` twice under different `#if`s (RAG is
//! not folded into every binary, e.g. the Windows CLI preset); mirrored here
//! with `#[cfg(wally_has_rag)]`.

use crate::cli::App;

#[cfg(wally_has_rag)]
const DEFAULT_RAG_LLM: &str = "smollm2-135m";
#[cfg(wally_has_rag)]
const DEFAULT_RAG_EMBED: &str = "all-minilm-l6-v2";

#[cfg(wally_has_rag)]
struct RagParams {
    llm_model: String,
    embed_model: String,
    docs: Vec<String>,
    files: Vec<String>,
    system_prompt: String,
    top_k: i32,
    chunk_size: i32,
    chunk_overlap: i32,
    max_output_tokens: i32,
    temperature: f32,
    similarity_threshold: f32,
    require_llm: bool,
}

#[cfg(wally_has_rag)]
fn read_text_file(path: &str) -> Result<String, String> {
    // Matches cmd_rag.cpp read_text_file: opens in binary mode and accepts
    // any byte sequence verbatim, including non-UTF-8. Only failing to open
    // the file is an error; the bytes are lossily converted to UTF-8 (proto
    // string fields require valid UTF-8) rather than rejected.
    let bytes = std::fs::read(path).map_err(|_| format!("cannot open file: {path}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// One session covers the whole invocation: open -> ingest every document -> ask
// or search. The CLI cannot split those verbs apart the way the SDK spec does
// because commons keeps RAG indexes in memory only
// (RAGConfiguration.index_path / persist_index are not honored).
#[cfg(wally_has_rag)]
fn collect_documents(params: &RagParams) -> Result<Vec<String>, ()> {
    use crate::io::output::error_line;

    let mut documents = params.docs.clone();
    for path in &params.files {
        match read_text_file(path) {
            Ok(content) => documents.push(content),
            Err(error) => {
                error_line(&error);
                return Err(());
            }
        }
    }
    if documents.is_empty() {
        error_line("at least one document is required (--doc or --file)");
        return Err(());
    }
    Ok(documents)
}

#[cfg(wally_has_rag)]
fn open_and_ingest(
    options: &crate::bootstrap::GlobalOptions,
    params: &RagParams,
    documents: &[String],
) -> Option<crate::sys::rac_handle_t> {
    use crate::io::output::{describe_result, error_line, status_line};
    use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
    use crate::sys;

    // Models must already be downloaded -- the session resolves them from the
    // registry. (Pull them first with `wally models download <id>`.)
    let mut config = v1::RagConfiguration {
        embedding_model_id: params.embed_model.clone(),
        ..Default::default()
    };
    if params.require_llm || !params.llm_model.is_empty() {
        config.llm_model_id = params.llm_model.clone();
    }
    if params.top_k > 0 {
        config.top_k = Some(params.top_k);
    }
    if params.chunk_size > 0 {
        config.chunk_size = Some(params.chunk_size);
    }
    if params.chunk_overlap > 0 {
        config.chunk_overlap = Some(params.chunk_overlap);
    }
    if params.similarity_threshold >= 0.0 {
        config.score_threshold = Some(params.similarity_threshold);
    }

    let config_bytes = serialize(&config);
    let mut session: sys::rac_handle_t = std::ptr::null_mut();
    // SAFETY: config_bytes is a valid slice for its own length; `session` is a
    // valid out-pointer for the duration of the call.
    let rc = unsafe {
        sys::rac_rag_session_create_proto(config_bytes.as_ptr(), config_bytes.len(), &mut session)
    };
    if rc != sys::SUCCESS || session.is_null() {
        if params.require_llm || !params.llm_model.is_empty() {
            error_line(&format!(
                "RAG session create failed (check that '{}' and '{}' are downloaded)",
                params.embed_model, params.llm_model
            ));
        } else {
            error_line(&format!(
                "RAG session create failed (check that '{}' is downloaded)",
                params.embed_model
            ));
        }
        return None;
    }

    for (i, doc_text) in documents.iter().enumerate() {
        let document = v1::RagDocument {
            id: format!("doc-{i}"),
            text: doc_text.clone(),
            ..Default::default()
        };
        let doc_bytes = serialize(&document);
        let mut stats_buffer = ProtoBuffer::new();
        // SAFETY: session is the live handle created above; doc_bytes is valid
        // for its own length; stats_buffer.as_mut_ptr() is a valid, initialized
        // rac_proto_buffer_t for the call.
        let ingest_rc = unsafe {
            sys::rac_rag_ingest_proto(
                session,
                doc_bytes.as_ptr(),
                doc_bytes.len(),
                stats_buffer.as_mut_ptr(),
            )
        };
        match parse_proto_buffer::<v1::RagStatistics>(stats_buffer) {
            Ok(_) if ingest_rc == sys::SUCCESS => {}
            Ok(_) => {
                error_line(&format!(
                    "RAG ingest failed: {}",
                    describe_result(ingest_rc)
                ));
                // SAFETY: session is the live handle created above; not used again.
                unsafe { sys::rac_rag_session_destroy_proto(session) };
                return None;
            }
            Err(error) => {
                error_line(&format!("RAG ingest failed: {error}"));
                // SAFETY: session is the live handle created above; not used again.
                unsafe { sys::rac_rag_session_destroy_proto(session) };
                return None;
            }
        }
        if options.verbose {
            status_line(&format!("ingested doc-{i} ({} bytes)", doc_text.len()));
        }
    }
    Some(session)
}

#[cfg(wally_has_rag)]
fn run_rag_query(
    options: &crate::bootstrap::GlobalOptions,
    params: &RagParams,
    question: &str,
) -> i32 {
    use crate::io::output::{error_line, result_line, status_line, JsonWriter};
    use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
    use crate::sys;

    if crate::bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    if question.is_empty() {
        error_line("a question is required (positional argument)");
        return 2;
    }

    let documents = match collect_documents(params) {
        Ok(documents) => documents,
        Err(()) => return 2,
    };

    let session = match open_and_ingest(options, params, &documents) {
        Some(session) => session,
        None => return 1,
    };

    let mut query = v1::RagQueryOptions {
        query: question.to_string(),
        ..Default::default()
    };
    if params.max_output_tokens > 0 || params.temperature >= 0.0 || !params.system_prompt.is_empty()
    {
        let mut generation = v1::LlmGenerationOptions::default();
        if params.max_output_tokens > 0 {
            generation.max_output_tokens = Some(params.max_output_tokens);
        }
        if params.temperature >= 0.0 {
            generation.temperature = Some(params.temperature);
        }
        if !params.system_prompt.is_empty() {
            generation.system_prompt = Some(params.system_prompt.clone());
        }
        query.generation = Some(generation);
    }
    if params.similarity_threshold >= 0.0 || params.top_k > 0 {
        let mut retrieval = v1::RagRetrievalOptions::default();
        if params.similarity_threshold >= 0.0 {
            retrieval.score_threshold = Some(params.similarity_threshold);
        }
        if params.top_k > 0 {
            retrieval.top_k = Some(params.top_k);
        }
        query.retrieval = Some(retrieval);
    }

    let query_bytes = serialize(&query);
    let mut result_buffer = ProtoBuffer::new();
    // SAFETY: session is the live handle from open_and_ingest above;
    // query_bytes is valid for its own length; result_buffer.as_mut_ptr() is a
    // valid, initialized rac_proto_buffer_t for the call.
    let proto_rc = unsafe {
        sys::rac_rag_query_proto(
            session,
            query_bytes.as_ptr(),
            query_bytes.len(),
            result_buffer.as_mut_ptr(),
        )
    };
    let result = match parse_proto_buffer::<v1::RagResult>(result_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            // Matches cmd_rag.cpp: parse_proto_buffer only ever writes
            // `error` on its own failure path (buffer status or decode).
            // When parsing succeeds but proto_rc still disagrees, C++'s
            // `error` stays empty, so the printed line is the bare prefix
            // with nothing after the colon — not `describe_result(proto_rc)`.
            // (Contrast open_and_ingest above, which explicitly falls back to
            // describe_result when `error` is empty; run_rag_query does not.)
            error_line("RAG query failed: ");
            // SAFETY: session is the live handle from open_and_ingest; not used
            // again after this.
            unsafe { sys::rac_rag_session_destroy_proto(session) };
            return 1;
        }
        Err(error) => {
            error_line(&format!("RAG query failed: {error}"));
            // SAFETY: as above.
            unsafe { sys::rac_rag_session_destroy_proto(session) };
            return 1;
        }
    };

    if let Some(error) = &result.error {
        let message = if error.message.is_empty() {
            error.c_abi_code.unwrap_or(0).to_string()
        } else {
            error.message.clone()
        };
        error_line(&format!("RAG query failed: {message}"));
        // SAFETY: as above.
        unsafe { sys::rac_rag_session_destroy_proto(session) };
        return 1;
    }

    if options.json {
        let usage = result.usage.unwrap_or_default();
        let mut json = JsonWriter::new();
        json.begin_object();
        // RAGResult carries no total_time_ms of its own (deleted in the API
        // realignment pass) -- retrieval_time_ms + generation_time_ms is the
        // whole measured wall-clock, so sum them rather than drop the field.
        json.field_str("answer", &result.answer)
            .field_i64("retrieval_time_ms", result.retrieval_time_ms)
            .field_i64("generation_time_ms", result.generation_time_ms)
            .field_i64(
                "total_time_ms",
                result.retrieval_time_ms + result.generation_time_ms,
            )
            .field_i64("prompt_tokens", i64::from(usage.input_tokens))
            .field_i64("completion_tokens", i64::from(usage.output_tokens));
        json.begin_array("matches");
        for chunk in &result.retrieved_chunks {
            json.begin_array_object();
            json.field_str("text", &chunk.text)
                .field_f64("score", f64::from(chunk.score))
                .field_str("source", chunk.source_document.as_deref().unwrap_or(""));
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
    } else {
        result_line(&result.answer);
        if options.verbose {
            status_line(&format!(
                "chunks={} retrieval={}ms generation={}ms",
                result.retrieved_chunks.len(),
                result.retrieval_time_ms,
                result.generation_time_ms
            ));
        }
    }

    // SAFETY: session is the live handle from open_and_ingest; this is the
    // single cleanup path for the success case.
    unsafe { sys::rac_rag_session_destroy_proto(session) };
    0
}

#[cfg(wally_has_rag)]
fn run_rag_search(
    options: &crate::bootstrap::GlobalOptions,
    params: &RagParams,
    question: &str,
) -> i32 {
    use crate::io::output::{error_line, result_line, status_line, JsonWriter};
    use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
    use crate::sys;

    if crate::bootstrap::bootstrap(options).is_err() {
        return 1;
    }

    if question.is_empty() {
        error_line("a question is required (positional argument)");
        return 2;
    }

    let documents = match collect_documents(params) {
        Ok(documents) => documents,
        Err(()) => return 2,
    };

    let session = match open_and_ingest(options, params, &documents) {
        Some(session) => session,
        None => return 1,
    };

    let mut request = v1::RagSearchRequest {
        query: question.to_string(),
        ..Default::default()
    };
    if params.top_k > 0 || params.similarity_threshold >= 0.0 {
        let mut retrieval = v1::RagRetrievalOptions::default();
        if params.top_k > 0 {
            retrieval.top_k = Some(params.top_k);
        }
        if params.similarity_threshold >= 0.0 {
            retrieval.score_threshold = Some(params.similarity_threshold);
        }
        request.retrieval = Some(retrieval);
    }

    let request_bytes = serialize(&request);
    let mut response_buffer = ProtoBuffer::new();
    // SAFETY: session is the live handle from open_and_ingest above;
    // request_bytes is valid for its own length; response_buffer.as_mut_ptr()
    // is a valid, initialized rac_proto_buffer_t for the call.
    let proto_rc = unsafe {
        sys::rac_rag_search_proto(
            session,
            request_bytes.as_ptr(),
            request_bytes.len(),
            response_buffer.as_mut_ptr(),
        )
    };
    let response = match parse_proto_buffer::<v1::RagSearchResponse>(response_buffer) {
        Ok(response) if proto_rc == sys::SUCCESS => response,
        Ok(_) => {
            // Matches cmd_rag.cpp run_rag_search: parse_proto_buffer only
            // ever writes `error` on its own failure path. When parsing
            // succeeds but proto_rc still disagrees, C++'s `error` stays
            // empty, so the printed line is the bare prefix with nothing
            // after the colon.
            error_line("RAG search failed: ");
            // SAFETY: session is the live handle from open_and_ingest; not used
            // again after this.
            unsafe { sys::rac_rag_session_destroy_proto(session) };
            return 1;
        }
        Err(error) => {
            error_line(&format!("RAG search failed: {error}"));
            // SAFETY: as above.
            unsafe { sys::rac_rag_session_destroy_proto(session) };
            return 1;
        }
    };

    if let Some(error) = &response.error {
        let message = if error.message.is_empty() {
            error.c_abi_code.unwrap_or(0).to_string()
        } else {
            error.message.clone()
        };
        error_line(&format!("RAG search failed: {message}"));
        // SAFETY: as above.
        unsafe { sys::rac_rag_session_destroy_proto(session) };
        return 1;
    }

    if options.json {
        let mut json = JsonWriter::new();
        json.begin_object();
        json.field_i64("retrieval_time_ms", response.retrieval_time_ms)
            .field_str("request_id", &response.request_id);
        json.begin_array("matches");
        for chunk in &response.chunks {
            json.begin_array_object();
            json.field_str("text", &chunk.text)
                .field_f64("score", f64::from(chunk.score))
                .field_str("source", chunk.source_document.as_deref().unwrap_or(""));
            json.end_object();
        }
        json.end_array();
        json.end_object();
        result_line(json.str());
    } else {
        for chunk in &response.chunks {
            result_line(&chunk.text);
        }
        if options.verbose {
            status_line(&format!(
                "chunks={} retrieval={}ms",
                response.chunks.len(),
                response.retrieval_time_ms
            ));
        }
    }

    // SAFETY: session is the live handle from open_and_ingest; this is the
    // single cleanup path for the success case.
    unsafe { sys::rac_rag_session_destroy_proto(session) };
    0
}

// Corpus and retrieval flags are the same for search and query; only `query`
// generates an answer, so only it takes the generation knobs.
#[cfg(wally_has_rag)]
fn add_corpus_options(cmd: &mut App) {
    use crate::cli::ValueType;

    cmd.add_option(
        "--doc,-d",
        ValueType::Text,
        "Document text to index; repeat for several",
    )
    .multi();
    cmd.add_option(
        "--file,-f",
        ValueType::Text,
        "Text file to index; repeat for several",
    )
    .multi();
    cmd.add_option(
        "--embedding-model,--embed",
        ValueType::Text,
        &format!("Embedding model to index with (default: {DEFAULT_RAG_EMBED})"),
    );
    cmd.add_option(
        "--top-k",
        ValueType::Int,
        "Retrieve this many chunks per question",
    );
    cmd.add_option(
        "--chunk-size",
        ValueType::Int,
        "Tokens per chunk when splitting documents",
    );
    cmd.add_option(
        "--chunk-overlap",
        ValueType::Int,
        "Tokens shared between neighbouring chunks",
    );
    cmd.add_option(
        "--similarity-threshold",
        ValueType::Float,
        "Discard chunks scoring below this",
    );
}

#[cfg(wally_has_rag)]
pub fn register_rag(app: &mut App) {
    use crate::cli::ValueType;

    let cmd = app.add_subcommand("rag", "Answer questions over your own documents");
    cmd.require_subcommand(1, 1);

    let query_cmd = cmd.add_subcommand("query", "Answer a question from the documents");
    query_cmd
        .add_option(
            "question",
            ValueType::Text,
            "Question to answer over the documents",
        )
        .required();
    add_corpus_options(query_cmd);
    query_cmd.add_option(
        "--model,--llm",
        ValueType::Text,
        &format!("LLM that writes the answer (default: {DEFAULT_RAG_LLM})"),
    );
    query_cmd.add_option(
        "--system-prompt",
        ValueType::Text,
        "Steer the answer with a system instruction",
    );
    query_cmd.add_option(
        "--max-output-tokens,--max-tokens",
        ValueType::Int,
        "Cap the answer length in tokens",
    );
    query_cmd.add_option(
        "--temperature",
        ValueType::Float,
        "Raise for more random sampling",
    );

    query_cmd.callback(|p, g| {
        let params = RagParams {
            llm_model: p
                .get_str("--model")
                .unwrap_or_else(|| DEFAULT_RAG_LLM.to_string()),
            embed_model: p
                .get_str("--embedding-model")
                .unwrap_or_else(|| DEFAULT_RAG_EMBED.to_string()),
            docs: p.get_strs("--doc"),
            files: p.get_strs("--file"),
            system_prompt: p.get_str("--system-prompt").unwrap_or_default(),
            top_k: p.get_i64("--top-k").unwrap_or(0) as i32,
            chunk_size: p.get_i64("--chunk-size").unwrap_or(0) as i32,
            chunk_overlap: p.get_i64("--chunk-overlap").unwrap_or(0) as i32,
            max_output_tokens: p.get_i64("--max-output-tokens").unwrap_or(0) as i32,
            temperature: p.get_f64("--temperature").unwrap_or(-1.0) as f32,
            similarity_threshold: p.get_f64("--similarity-threshold").unwrap_or(-1.0) as f32,
            require_llm: true,
        };
        run_rag_query(g, &params, &p.get_str("question").unwrap_or_default())
    });

    let search_cmd = cmd.add_subcommand(
        "search",
        "Retrieve matching chunks without generating an answer",
    );
    search_cmd
        .add_option("question", ValueType::Text, "Query to retrieve chunks for")
        .required();
    add_corpus_options(search_cmd);
    // retrieval-only unless --model/--llm is passed
    search_cmd.add_option(
        "--model,--llm",
        ValueType::Text,
        "Optional LLM (needed only for multi-query / session rerank)",
    );

    search_cmd.callback(|p, g| {
        let params = RagParams {
            llm_model: p.get_str("--model").unwrap_or_default(),
            embed_model: p
                .get_str("--embedding-model")
                .unwrap_or_else(|| DEFAULT_RAG_EMBED.to_string()),
            docs: p.get_strs("--doc"),
            files: p.get_strs("--file"),
            system_prompt: String::new(),
            top_k: p.get_i64("--top-k").unwrap_or(0) as i32,
            chunk_size: p.get_i64("--chunk-size").unwrap_or(0) as i32,
            chunk_overlap: p.get_i64("--chunk-overlap").unwrap_or(0) as i32,
            max_output_tokens: 0,
            temperature: -1.0,
            similarity_threshold: p.get_f64("--similarity-threshold").unwrap_or(-1.0) as f32,
            require_llm: false,
        };
        run_rag_search(g, &params, &p.get_str("question").unwrap_or_default())
    });
}

#[cfg(all(test, wally_has_rag))]
mod tests {
    use super::read_text_file;

    #[test]
    fn read_text_file_accepts_non_utf8_bytes() {
        // A lone 0xFF is not valid UTF-8; cmd_rag.cpp's binary
        // read accepts it verbatim, so read_text_file must not reject an
        // openable-but-non-UTF-8 file as "cannot open".
        let dir = std::env::temp_dir().join(format!(
            "wally-fix-vision-rag-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("notes.txt");
        std::fs::write(&path, [b'h', b'i', 0xFFu8, b'!']).expect("write temp file");

        let result = read_text_file(path.to_str().expect("utf8 path"));
        assert!(
            result.is_ok(),
            "non-UTF-8 file must be accepted, not reported as unopenable: {result:?}"
        );
        let text = result.expect("checked above");
        assert!(text.starts_with("hi"));
        assert!(text.ends_with('!'));
        assert!(text.contains('\u{FFFD}'));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_text_file_missing_file_still_cannot_open() {
        let result = read_text_file("/no/such/wally-fix-vision-rag-path.bin");
        assert_eq!(
            result,
            Err("cannot open file: /no/such/wally-fix-vision-rag-path.bin".to_string())
        );
    }
}

// The RAG pipeline is not folded into this binary (RAC_BACKEND_RAG=OFF, e.g.
// the Windows CLI preset), so the rac_rag_*_proto symbols are unavailable.
// Register no `rag` subcommand rather than fail to link.
#[cfg(not(wally_has_rag))]
pub fn register_rag(app: &mut App) {
    let _ = app;
}
