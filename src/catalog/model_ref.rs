//! Model reference resolution for pull/run/show/rm arguments (port of
//! src/catalog/model_ref.cpp). Owner: the catalog port.
//!
//! Accepted forms, resolved in order:
//!   1. catalog id            qwen3-0.6b
//!   2. catalog alias         qwen3
//!   3. registered id         (manifest-restored URL/HF pulls, discovered models)
//!   4. hf.co/<org>/<repo>[:quant|/file], hf://..., huggingface.co/...
//!   5. http(s)://...
//!
//! URL and HF forms go through rac_register_model_from_url_proto so the whole
//! grammar lives in commons — the CLI never guesses.

use std::ffi::CString;
use std::path::Path;

use crate::catalog::catalog;
use crate::io::output::describe_result;
use crate::io::proto::{parse_proto_buffer, serialize, v1, ProtoBuffer};
use crate::sys;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// registry id to operate on
    pub model_id: String,
    pub from_catalog: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolveOptions {
    pub has_framework: bool,
    pub framework: v1::InferenceFramework,
    pub has_category: bool,
    pub category: v1::ModelCategory,
}

impl Default for ResolveOptions {
    fn default() -> Self {
        ResolveOptions {
            has_framework: false,
            framework: v1::InferenceFramework::Unspecified,
            has_category: false,
            category: v1::ModelCategory::Unspecified,
        }
    }
}

fn is_http_url(reference: &str) -> bool {
    reference.starts_with("http://") || reference.starts_with("https://")
}

/// Thin gate only — the full HF ref grammar (repo refs, quant tags, explicit
/// file paths, shard/mmproj resolution) is owned by commons inside
/// rac_register_model_from_url_proto; the CLI just decides whether `reference`
/// is worth handing over versus reporting "unknown model".
fn looks_like_hf_ref(reference: &str) -> bool {
    ["hf://", "hf.co/", "huggingface.co/"]
        .iter()
        .any(|prefix| reference.starts_with(prefix))
}

/// A ref that names an existing directory or file on disk. Checked BEFORE the
/// HF/URL branch but AFTER the catalog and the registry, so a local path can
/// never shadow a real model id.
fn is_local_path(reference: &str) -> bool {
    if reference.is_empty() || is_http_url(reference) || looks_like_hf_ref(reference) {
        return false;
    }
    // Only treat something as a path when it looks like one. A bare word is a
    // model id; requiring a separator (or a Windows drive prefix) keeps
    // `wally run qwen3` from probing the cwd and finding a stray directory.
    let has_sep = reference.contains('/') || reference.contains('\\');
    let bytes = reference.as_bytes();
    let win_drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if !has_sep && !win_drive {
        return false;
    }
    Path::new(reference).exists()
}

/// A trailing `/` carries no meaning but would break both the basename split
/// and the `.mlpackage` suffix tests below, so strip it once, here.
fn without_trailing_slashes(path: &str) -> String {
    let mut out = path.to_string();
    while out.len() > 1 && (out.ends_with('/') || out.ends_with('\\')) {
        out.pop();
    }
    out
}

/// Mirrors `std::filesystem::path::filename()` on the raw, unstripped input:
/// unlike `Path::file_name()`, which strips a trailing separator before
/// taking the last component, this returns empty when `path` ends with a
/// separator (matching the C++ standard's documented behaviour).
fn cpp_style_filename(path: &str) -> String {
    if path.ends_with('/') || path.ends_with('\\') {
        return String::new();
    }
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Best-effort `std::filesystem::weakly_canonical`: resolve symlinks/`..`
/// through the real path when it exists (the common case here — callers only
/// reach this after `is_local_path` confirmed the path exists), falling back
/// to the original string on any error exactly like the C++ did.
fn weakly_canonical(path: &str) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Derive a stable, collision-free registry id from a bundle path. Using one
/// hardcoded id (as the diffusion path does) is fine for a single model and
/// wrong the moment two bundles are registered in one process — the second
/// silently overwrites the first, and every later load resolves to whichever
/// won.
fn id_for_local_path(path: &str) -> String {
    let mut full = without_trailing_slashes(path);
    // Canonicalize first so the same bundle spelled differently (relative, `..`,
    // a symlink) keeps ONE id; the raw path is the fallback when it cannot be
    // resolved.
    if let Some(canonical) = weakly_canonical(&full) {
        if !canonical.is_empty() {
            full = canonical;
        }
    }

    // file_name() handles BOTH separators; a manual rfind('/') would return
    // the whole path as the basename on a Windows-style path.
    let mut base = Path::new(&full)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base.is_empty() {
        base = full.clone();
    }
    let base: String = base
        .chars()
        .map(|c| {
            let lower = c.to_ascii_lowercase();
            if lower.is_ascii_alphanumeric() || lower == '-' || lower == '.' {
                lower
            } else {
                '-'
            }
        })
        .collect();

    // The basename alone collides — every `.../model.mlpackage` sanitizes to the
    // same string — so pin the id to the whole path with an FNV-1a digest.
    let mut hash: u64 = 14695981039346656037; // FNV-1a 64-bit offset basis
    for byte in full.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(1099511628211);
    }
    let digest = format!("{hash:016x}");
    format!("local-{base}-{digest}")
}

/// Best-effort format/framework inference from the bundle layout. Only used when
/// the caller did not pin one with --engine, and deliberately narrow: it answers
/// "which of the shapes the CLI can actually load is this", not "what model is
/// this". Unknown layouts are left UNSPECIFIED so commons falls back to its own
/// resolution rather than acting on a CLI guess.
fn infer_local_kind(path: &str) -> (v1::InferenceFramework, v1::ModelFormat) {
    let mut framework = v1::InferenceFramework::Unspecified;
    let mut format = v1::ModelFormat::Unspecified;

    let p = Path::new(path);
    if !p.exists() {
        return (framework, format);
    }
    if !p.is_dir() {
        if path.ends_with(".gguf") {
            framework = v1::InferenceFramework::LlamaCpp;
            format = v1::ModelFormat::Gguf;
        }
        return (framework, format);
    }
    // A Core ML package IS a directory, so a ref naming the package itself lands
    // here too — and its children (Manifest.json, Data/) match nothing in the
    // scan below. Check the path's own name before descending into it.
    let self_path = without_trailing_slashes(path);
    if self_path.ends_with(".mlpackage") || self_path.ends_with(".mlmodelc") {
        framework = v1::InferenceFramework::Coreml;
        format = v1::ModelFormat::Mlpackage;
        return (framework, format);
    }
    // A directory holding at least one .mlpackage is an Apple bundle — the shape
    // every runanywhere/*_ANE repo ships.
    if let Ok(entries) = std::fs::read_dir(p) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".mlpackage") || name.ends_with(".mlmodelc") {
                return (v1::InferenceFramework::Coreml, v1::ModelFormat::Mlpackage);
            }
        }
    }
    // QHexRT HNPU bundles: Hexagon arch folder (v75/v79/v81), a context.bin, or
    // a top-level non-aux .json next to QNN binaries. `_HNPU` is the published
    // repo suffix; honor it so `wally run --engine qhexrt <dir>` is not required.
    //
    // Use the C++-style leaf, not `Path::file_name()`: the latter strips a
    // trailing separator before taking the last component (so
    // `Path::new("a/HNPU/").file_name()` is `Some("HNPU")`), while
    // `std::filesystem::path::filename()` on the same unstripped input
    // returns empty. A `wally run ./models/Foo_HNPU/` (trailing slash) must
    // miss the name-suffix shortcut exactly like C++ does.
    let leaf = cpp_style_filename(path);
    let looks_qnn = || -> bool {
        if leaf.contains("_HNPU") || leaf.contains("-npu") {
            return true;
        }
        let mut saw_json = false;
        let mut saw_bin = false;
        if let Ok(entries) = std::fs::read_dir(p) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "v75" || name == "v79" || name == "v81" || name == "context.bin" {
                    return true;
                }
                if name.ends_with(".json")
                    && name != "tokenizer.json"
                    && name != "config.json"
                    && name != "tokenizer_config.json"
                    && name != "generation_config.json"
                {
                    saw_json = true;
                }
                if name.ends_with(".bin") || name.ends_with(".raw") {
                    saw_bin = true;
                }
            }
        }
        saw_json && saw_bin
    };
    if looks_qnn() {
        framework = v1::InferenceFramework::Qhexrt;
        format = v1::ModelFormat::QnnContext;
    }
    (framework, format)
}

/// Register an already-present local bundle so the lifecycle loader can resolve
/// it by id, with no download. Mirrors what `wally image` has always done for a
/// local CoreML diffusion bundle (cmd_image.cpp::register_local_bundle) — this
/// is that capability moved down to the shared resolver, so every command that
/// takes a model ref gets it instead of just one.
fn register_local_path(
    path: &str,
    options: Option<&ResolveOptions>,
) -> Result<String, (sys::rac_result_t, String)> {
    let (mut framework, format) = infer_local_kind(path);
    // An explicit --engine always wins over inference.
    if let Some(opts) = options {
        if opts.has_framework {
            framework = opts.framework;
        }
    }

    let id = id_for_local_path(path);
    let category = match options {
        Some(opts) if opts.has_category => opts.category,
        _ => v1::ModelCategory::Language,
    };
    // ModelInfo.framework/format are plain (non-optional) proto3 fields, so
    // setting them to UNSPECIFIED (0) is indistinguishable from the C++
    // leaving them unset.
    let model = v1::ModelInfo {
        id: id.clone(),
        name: path.to_string(),
        local_path: path.to_string(),
        source: v1::ModelSource::Local as i32,
        category: category as i32,
        framework: framework as i32,
        format: format as i32,
        ..Default::default()
    };

    let bytes = serialize(&model);
    // SAFETY: `bytes` is a valid, alive byte buffer for the duration of the
    // call; the registry handle is the process-global singleton.
    let rc = unsafe {
        let path_ptr = bytes.as_ptr();
        sys::rac_model_registry_register_proto(sys::rac_get_model_registry(), path_ptr, bytes.len())
    };
    if rc != sys::SUCCESS {
        return Err((
            rc,
            format!(
                "failed to register local bundle '{path}': {}",
                describe_result(rc)
            ),
        ));
    }
    Ok(id)
}

/// Registers a URL or Hugging Face ref through the commons factory and returns
/// the saved id. Durable persistence is commons-owned: once the model
/// downloads, the model-folder manifest sidecar restores the entry on the next
/// launch (no CLI-side registry needed).
fn register_url(
    url: &str,
    options: Option<&ResolveOptions>,
) -> Result<String, (sys::rac_result_t, String)> {
    let request = v1::RegisterModelFromUrlRequest {
        url: url.to_string(),
        framework: options
            .filter(|o| o.has_framework)
            .map(|o| o.framework as i32),
        category: options
            .filter(|o| o.has_category)
            .map(|o| o.category as i32),
        ..Default::default()
    };

    let bytes = serialize(&request);
    let mut out = ProtoBuffer::new();
    // SAFETY: `bytes` outlives the call; `out` is a freshly-initialised buffer.
    let rc = unsafe {
        sys::rac_register_model_from_url_proto(bytes.as_ptr(), bytes.len(), out.as_mut_ptr())
    };
    if rc != sys::SUCCESS {
        // C++ here checks only for a null error_message pointer (not
        // null-or-empty, unlike the shared parse_proto_buffer convention
        // below), so a non-null empty error_message yields an empty trailing
        // detail rather than falling back to describe_result(rc).
        let detail = out
            .error_message_nullable()
            .unwrap_or_else(|| describe_result(rc));
        return Err((rc, format!("failed to register {url}: {detail}")));
    }

    let saved: v1::ModelInfo = match parse_proto_buffer(out) {
        Ok(model) => model,
        Err(parse_error) => {
            return Err((
                sys::RAC_ERROR_INVALID_ARGUMENT,
                format!("failed to register {url}: {parse_error}"),
            ));
        }
    };
    Ok(saved.id)
}

/// Resolve `reference` to a registered model id. Catalog entries are assumed
/// registered (bootstrap runs catalog::register_all()); URL refs register a new
/// entry on the fly. The error is the result code plus a user-facing message
/// including did-you-mean suggestions.
pub fn resolve(
    reference: &str,
    options: Option<&ResolveOptions>,
) -> Result<Resolved, (sys::rac_result_t, String)> {
    if reference.is_empty() {
        return Err((
            sys::RAC_ERROR_INVALID_ARGUMENT,
            "empty model reference".to_string(),
        ));
    }

    if let Some(entry) = catalog::find(reference) {
        return Ok(Resolved {
            model_id: entry.id.to_string(),
            from_catalog: true,
        });
    }

    // Registered but non-catalog ids: manifest-restored URL/HF pulls,
    // discovered models.
    if !is_http_url(reference) {
        if let Ok(id_c) = CString::new(reference) {
            let mut found = ProtoBuffer::new();
            // SAFETY: `id_c` is a valid NUL-terminated string for the call;
            // `found` is a freshly-initialised buffer.
            let rc = unsafe {
                sys::rac_model_registry_get_proto_buffer(
                    sys::rac_get_model_registry(),
                    id_c.as_ptr(),
                    found.as_mut_ptr(),
                )
            };
            if rc == sys::SUCCESS && found.status() == sys::SUCCESS {
                return Ok(Resolved {
                    model_id: reference.to_string(),
                    from_catalog: false,
                });
            }
        }
    }

    // A path on disk. Checked after the catalog + registry so it can never
    // shadow a real id, and before the "unknown model" arm so an ANE bundle
    // sitting in a directory is reachable at all — it previously was not, which
    // made every local Core ML LLM unloadable through the CLI.
    if is_local_path(reference) {
        return register_local_path(reference, options).map(|model_id| Resolved {
            model_id,
            from_catalog: false,
        });
    }

    if is_http_url(reference) || looks_like_hf_ref(reference) {
        return register_url(reference, options).map(|model_id| Resolved {
            model_id,
            from_catalog: false,
        });
    }

    let mut error = format!("unknown model '{reference}'");
    let close = catalog::suggestions(reference, 3);
    if !close.is_empty() {
        error.push_str(" — did you mean: ");
        error.push_str(&close.join(", "));
        error.push('?');
    } else {
        // No close local match: this is as likely a hosted console model id
        // (glm-5.3-flash, ...) as a typo, and `wally run`/`llm generate` only
        // ever loads a model on this machine — there is no cloud fallback here
        // to dead-end into quietly. Point at the one that exists.
        error.push_str(&format!(
            " (try `wally models list --all`, an hf.co/org/repo[:quant] ref, a \
             direct URL, or a path to a local bundle directory — or, if it's a \
             model your account has on the hosted console, `wally claude-code -m \
             {reference}` / `wally opencode --cloud -m {reference}`)"
        ));
    }
    Err((sys::RAC_ERROR_NOT_FOUND, error))
}

#[cfg(test)]
mod fix_run_tests {
    use super::*;

    /// A directory whose ONLY QHexRT signal is the `_HNPU` name suffix (no
    /// v75/v79/v81 arch folder, no context.bin, no qualifying json+bin pair)
    /// so detection can only succeed via the name-suffix shortcut.
    fn make_hnpu_dir_with_no_other_signal() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let bundle = dir.path().join("MyModel_HNPU");
        std::fs::create_dir(&bundle).expect("create bundle dir");
        std::fs::write(bundle.join("readme.txt"), b"nothing here").expect("write stray file");
        dir
    }

    #[test]
    fn trailing_slash_defeats_hnpu_suffix_detection_like_cpp_filename() {
        let dir = make_hnpu_dir_with_no_other_signal();
        let bundle = dir.path().join("MyModel_HNPU");

        // No trailing slash: the `_HNPU` suffix shortcut fires.
        let (framework, format) = infer_local_kind(bundle.to_str().unwrap());
        assert_eq!(framework, v1::InferenceFramework::Qhexrt);
        assert_eq!(format, v1::ModelFormat::QnnContext);

        // Trailing slash: std::filesystem::path::filename() on the raw,
        // unstripped input returns "" here, so C++ (and now this port) misses
        // the shortcut and falls through to the other checks, which also
        // fail for this bundle's contents -- framework/format stay
        // UNSPECIFIED.
        let with_slash = format!("{}/", bundle.to_str().unwrap());
        let (framework, format) = infer_local_kind(&with_slash);
        assert_eq!(framework, v1::InferenceFramework::Unspecified);
        assert_eq!(format, v1::ModelFormat::Unspecified);
    }

    #[test]
    fn cpp_style_filename_is_empty_on_trailing_separator() {
        assert_eq!(cpp_style_filename("/a/b/HNPU/"), "");
        assert_eq!(cpp_style_filename("/a/b/HNPU"), "HNPU");
        assert_eq!(cpp_style_filename("HNPU"), "HNPU");
    }
}
