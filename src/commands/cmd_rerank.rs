//! `wally rerank <query>` — score documents against a query, best first. Port
//! of src/commands/cmd_rerank.cpp. Not registered in the app (LLM-only
//! release), as in the C++.

use std::ffi::CString;
use std::io::Write as _;

use crate::bootstrap::{self, GlobalOptions};
use crate::cli::{App, Validator, ValueType};
use crate::commands::model_setup::ensure_model_ready;
use crate::io::output::{describe_result, error_line, result_line, JsonWriter};
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;

/// Byte-exact reimplementation of `io::output::table` for the DOCUMENT
/// column, which (matching cmd_rerank.cpp) may legitimately contain a
/// truncated, non-UTF-8 byte sequence. `output::table` operates on `String`
/// and cannot hold that, so this renders raw bytes directly the same way the
/// C++ build's `fprintf("%s", ...)` does. Column widths are byte lengths, as
/// in the C++ (`std::string::size`).
fn print_table_bytes(header: &[&str], rows: &[Vec<Vec<u8>>]) {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.len()).collect();
    for row in rows {
        for (c, cell) in row.iter().enumerate().take(widths.len()) {
            widths[c] = widths[c].max(cell.len());
        }
    }
    let mut stdout = std::io::stdout().lock();
    let mut print_row = |row: &[Vec<u8>]| {
        let mut line: Vec<u8> = Vec::new();
        for (c, width) in widths.iter().enumerate() {
            let cell: &[u8] = row.get(c).map(Vec::as_slice).unwrap_or(&[]);
            line.extend_from_slice(cell);
            if c + 1 < widths.len() {
                line.resize(line.len() + (width - cell.len() + 4), b' ');
            }
        }
        line.push(b'\n');
        let _ = stdout.write_all(&line);
        let _ = stdout.flush();
    };
    let header_row: Vec<Vec<u8>> = header.iter().map(|s| s.as_bytes().to_vec()).collect();
    print_row(&header_row);
    for row in rows {
        print_row(row);
    }
}

fn read_text_file(path: &str) -> Result<String, String> {
    // Matches cmd_rerank.cpp read_text_file: opens in binary mode and accepts
    // any byte sequence verbatim, including non-UTF-8. Only failing to open
    // the file is an error; the bytes are lossily converted to UTF-8 (proto
    // string fields require valid UTF-8) rather than rejected.
    let bytes = std::fs::read(path).map_err(|_| format!("cannot open file: {path}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The rendered DOCUMENT column for one rerank result row. Matches
/// cmd_rerank.cpp's `documents[index].substr(0, 57) + "..."`: a raw
/// byte-index cut with no UTF-8 awareness, so a document whose byte offset
/// 57 falls inside a multi-byte character is truncated mid-character rather
/// than sanitized.
fn document_preview_bytes(doc_bytes: &[u8]) -> Vec<u8> {
    if doc_bytes.len() > 60 {
        let mut cut = doc_bytes[..57.min(doc_bytes.len())].to_vec();
        cut.extend_from_slice(b"...");
        cut
    } else {
        doc_bytes.to_vec()
    }
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
            // Matches cmd_rerank.cpp: parse_proto_buffer only ever writes
            // `error` on its own failure path (buffer status or decode). When
            // parsing succeeds but proto_rc still disagrees, C++'s `error`
            // stays empty, so the printed line is the bare prefix with
            // nothing after the colon — not `describe_result(proto_rc)`.
            error_line("rerank failed: ");
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
        let header = ["RANK", "INDEX", "SCORE", "DOCUMENT"];
        let mut rows: Vec<Vec<Vec<u8>>> = Vec::with_capacity(result.items.len());
        for (i, item) in result.items.iter().enumerate() {
            let doc = documents
                .get(item.index as usize)
                .map(String::as_str)
                .unwrap_or("");
            let preview = document_preview_bytes(doc.as_bytes());
            rows.push(vec![
                (i + 1).to_string().into_bytes(),
                item.index.to_string().into_bytes(),
                format!("{:.4}", item.relevance_score).into_bytes(),
                preview,
            ]);
        }
        print_table_bytes(&header, &rows);
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
    cmd.add_option(
        "--model,-m",
        ValueType::Text,
        "Reranker model id or on-disk path",
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_text_file_accepts_non_utf8_bytes() {
        // A lone 0xFF is not valid UTF-8; cmd_rerank.cpp's binary
        // read accepts it verbatim, so read_text_file must not reject an
        // openable-but-non-UTF-8 file as "cannot open".
        let dir = std::env::temp_dir().join(format!(
            "wally-fix-vision-rerank-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("doc.bin");
        std::fs::write(&path, [b'h', b'i', 0xFFu8, b'!']).expect("write temp file");

        let result = read_text_file(path.to_str().expect("utf8 path"));
        assert!(
            result.is_ok(),
            "non-UTF-8 file must be accepted, not reported as unopenable: {result:?}"
        );
        // The lone 0xFF is lossily replaced (Rust `String` cannot hold raw
        // invalid UTF-8), but the surrounding valid bytes survive intact.
        let text = result.expect("checked above");
        assert!(text.starts_with("hi"));
        assert!(text.ends_with('!'));
        assert!(text.contains('\u{FFFD}'));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_text_file_missing_file_still_cannot_open() {
        let result = read_text_file("/no/such/wally-fix-vision-rerank-path.bin");
        assert_eq!(
            result,
            Err("cannot open file: /no/such/wally-fix-vision-rerank-path.bin".to_string())
        );
    }

    #[test]
    fn document_preview_short_document_is_unchanged() {
        let doc = b"short document";
        assert_eq!(document_preview_bytes(doc), doc.to_vec());
    }

    #[test]
    fn document_preview_cuts_mid_multibyte_character_as_raw_bytes() {
        // 56 ASCII bytes followed by 'e' with an acute accent
        // (U+00E9, UTF-8 bytes 0xC3 0xA9) straddles the byte-57 cut point.
        // C++'s substr(0, 57) keeps the raw dangling lead byte 0xC3 verbatim;
        // Rust must not replace it with U+FFFD (EF BF BD).
        let mut doc = vec![b'a'; 56];
        doc.extend_from_slice("é more text to push past 60 bytes total".as_bytes());
        assert!(doc.len() > 60, "fixture must exceed the 60-byte threshold");

        let preview = document_preview_bytes(&doc);

        let mut expected = vec![b'a'; 56];
        expected.push(0xC3); // dangling lead byte of 'é', emitted verbatim
        expected.extend_from_slice(b"...");
        assert_eq!(preview, expected);
        // Must NOT contain the lossy replacement character's bytes.
        assert!(!preview.windows(3).any(|w| w == [0xEF, 0xBF, 0xBD]));
    }
}
