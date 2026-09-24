//! Port of src/commands/cmd_backends.cpp.
//!
//! `wally backends` — registered engine plugins per primitive.

use std::collections::BTreeMap;
use std::ffi::CStr;

use crate::bootstrap::bootstrap;
use crate::cli::App;
use crate::io::output as out;
use crate::sys;

use super::EngineRow;

// SAFETY helper: NUL-terminated, static/registry-owned C string or null -> owned String.
fn cstr_or_empty(ptr: *const std::os::raw::c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        // SAFETY: caller only passes pointers documented by the SDK as either
        // NULL or a NUL-terminated string valid for the call's duration.
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Snapshot of every registered inference backend, keyed by engine name.
/// Assumes bootstrap() has already run. Shared by `wally backends` and `wally about`.
///
/// Walk every live primitive. ONNX without RAG only advertises SEGMENT /
/// DIARIZE — omitting those made a registered onnx backend invisible.
pub fn collect_backend_rows() -> BTreeMap<String, EngineRow> {
    let mut engines: BTreeMap<String, EngineRow> = BTreeMap::new();
    for raw in 1..(sys::RAC_PRIMITIVE_COUNT as i64) {
        if raw == 6 {
            continue; // retired RERANK wire value
        }
        let primitive = raw as sys::rac_primitive_t;
        let mut plugins: [*const sys::rac_engine_vtable_t; 16] = [std::ptr::null(); 16];
        let mut count: usize = 0;
        // SAFETY: `plugins` is a valid, writable array of 16 slots; rac_plugin_list
        // writes at most `count` (<= 16) entries and each returned pointer is valid
        // for the registry's remaining lifetime.
        let rc = unsafe {
            sys::rac_plugin_list(primitive, plugins.as_mut_ptr(), plugins.len(), &mut count)
        };
        if rc != sys::SUCCESS {
            continue;
        }
        // SAFETY: rac_primitive_name returns a static string for any valid primitive.
        let primitive_name = cstr_or_empty(unsafe { sys::rac_primitive_name(primitive) });

        for slot in plugins.iter().take(count) {
            // SAFETY: rac_plugin_list guarantees the first `count` entries are
            // non-null and valid for the registry's remaining lifetime.
            let vtable = unsafe { &**slot };
            let meta = &vtable.metadata;
            let name = if meta.name.is_null() {
                "?".to_string()
            } else {
                cstr_or_empty(meta.name)
            };
            let row = engines.entry(name).or_default();
            if !meta.display_name.is_null() {
                row.display_name = cstr_or_empty(meta.display_name);
            }
            if !meta.engine_version.is_null() {
                row.version = cstr_or_empty(meta.engine_version);
            }
            row.priority = meta.priority;
            row.primitives.insert(primitive_name.clone());
        }
    }
    engines
}

/// collect_backend_rows() narrowed to engines that serve generate_text
/// (TEMP llm-only cut; see the C++ comment in commands.h).
pub fn collect_llm_backend_rows() -> BTreeMap<String, EngineRow> {
    let mut engines = collect_backend_rows();
    // SAFETY: rac_primitive_name returns a static string for a valid primitive.
    let llm = cstr_or_empty(unsafe { sys::rac_primitive_name(sys::RAC_PRIMITIVE_GENERATE_TEXT) });
    engines.retain(|_, row| row.primitives.contains(&llm));
    engines
}

pub fn register_backends(app: &mut App) {
    let cmd = app.add_subcommand("backends", "List registered inference backends");
    cmd.callback(|_parsed, options| {
        if bootstrap(options).is_err() {
            return 1;
        }

        let engines = collect_backend_rows();

        if options.json {
            let mut json = out::JsonWriter::new();
            json.begin_object().begin_array("backends");
            for (name, row) in &engines {
                json.begin_array_object()
                    .field_str("name", name)
                    .field_str("display_name", &row.display_name)
                    .field_str("version", &row.version)
                    .field_i64("priority", row.priority as i64);
                json.begin_array("primitives");
                for primitive in &row.primitives {
                    json.begin_array_object()
                        .field_str("name", primitive)
                        .end_object();
                }
                json.end_array().end_object();
            }
            json.end_array().end_object();
            out::result_line(json.str());
            return 0;
        }

        if engines.is_empty() {
            out::result_line("no backends registered");
            return 0;
        }
        let mut rows: Vec<Vec<String>> = Vec::new();
        for (name, row) in &engines {
            let mut primitives = String::new();
            for primitive in &row.primitives {
                if primitives.is_empty() {
                    primitives.push_str(primitive);
                } else {
                    primitives.push_str(", ");
                    primitives.push_str(primitive);
                }
            }
            rows.push(vec![name.clone(), row.priority.to_string(), primitives]);
        }
        out::table(
            &[
                "NAME".to_string(),
                "PRIORITY".to_string(),
                "PRIMITIVES".to_string(),
            ],
            &rows,
        );
        0
    });
}
