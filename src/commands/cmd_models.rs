//! `wally models …` — the model-lifecycle namespace from the public API spec
//! (list, get, register, download, delete, load, unload, state).
//!
//! list / get / download / delete reuse the configure_* functions owned by
//! cmd_list, cmd_show, cmd_pull and cmd_rm, so the namespaced verbs and the
//! top-level aliases (`list`, `show`, `pull`, `rm`) are the same command.
//! register / load / unload / state are thin translations of the commons
//! lifecycle ABI:
//!   register → model_ref::resolve (rac_register_model_from_url_proto inside)
//!   load     → rac_model_lifecycle_load_proto(validate_availability=true)
//!   unload   → rac_model_lifecycle_unload_proto
//!   state    → rac_model_lifecycle_current_model_proto per category

use std::path::{Path, PathBuf};

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::cmd_list::configure_models_list;
use crate::commands::cmd_pull::configure_models_download;
use crate::commands::cmd_rm::configure_models_delete;
use crate::commands::cmd_show::configure_models_get;
use crate::commands::engine_options::resolve_engine_hint;
use crate::commands::model_labels;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, v1, ProtoBuffer};
use crate::progress::progress_bar::DownloadProgressScope;
use crate::sys;

// Categories `models state` reports, in the order they are printed.
const STATE_CATEGORIES: [v1::ModelCategory; 7] = [
    v1::ModelCategory::Language,
    v1::ModelCategory::Multimodal,
    v1::ModelCategory::SpeechRecognition,
    v1::ModelCategory::SpeechSynthesis,
    v1::ModelCategory::VoiceActivityDetection,
    v1::ModelCategory::Embedding,
    v1::ModelCategory::ImageGeneration,
];

fn parse_category(name: &str) -> Option<v1::ModelCategory> {
    STATE_CATEGORIES
        .into_iter()
        .find(|&category| name == model_labels::category(category))
}

fn category_choices() -> String {
    let mut choices = String::new();
    for category in STATE_CATEGORIES {
        if !choices.is_empty() {
            choices.push_str(", ");
        }
        choices.push_str(model_labels::category(category));
    }
    choices
}

fn model_id_cstring(model_id: &str) -> std::ffi::CString {
    std::ffi::CString::new(model_id).unwrap_or_default()
}

fn run_register(options: &GlobalOptions, reference: &str, engine: &str) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let engine_hint = match resolve_engine_hint(engine) {
        Ok(hint) => hint,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };

    let resolved = match model_ref::resolve(reference, Some(&engine_hint.resolve_options)) {
        Ok(resolved) => resolved,
        Err((_, error)) => {
            out::error_line(&error);
            return 1;
        }
    };

    let mut info_out = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle; info_out is a valid out-param for this call only.
    let get_rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            model_id_cstring(&resolved.model_id).as_ptr(),
            info_out.as_mut_ptr(),
        )
    };
    let model: v1::ModelInfo = match parse_proto_buffer(info_out) {
        Ok(model) if get_rc == sys::SUCCESS => model,
        Ok(_) => {
            out::error_line("registration failed: ");
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("registration failed: {error}"));
            return 1;
        }
    };

    if options.json {
        let category =
            v1::ModelCategory::try_from(model.category).unwrap_or(v1::ModelCategory::Unspecified);
        let framework = v1::InferenceFramework::try_from(model.framework)
            .unwrap_or(v1::InferenceFramework::Unspecified);
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_str("id", &model.id)
            .field_str("name", &model.name)
            .field_str("modality", model_labels::category(category))
            .field_str("backend", model_labels::backend(framework))
            .field_str("download_url", &model.download_url)
            .field_bool(
                "downloaded",
                model.registry_status == Some(v1::ModelRegistryStatus::Downloaded as i32),
            )
            .end_object();
        out::result_line(json.str());
    } else {
        out::result_line(&format!("registered {}", model.id));
    }
    0
}

/// The `ModelLoadRequest.framework` an explicit `--engine` asks for, or `None`
/// to leave the SDK falling back to the ref's own declared framework. Pulled
/// out of run_load so the "an explicit engine always wins, even for a catalog
/// ref" rule is provable under `cargo test` without bootstrap()/the SDK.
fn framework_override(engine: v1::InferenceFramework) -> Option<i32> {
    if engine != v1::InferenceFramework::Unspecified {
        Some(engine as i32)
    } else {
        None
    }
}

fn run_load(options: &GlobalOptions, reference: &str, engine: &str, category_name: &str) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let mut engine_hint = match resolve_engine_hint(engine) {
        Ok(hint) => hint,
        Err(error) => {
            out::error_line(&error);
            return 2;
        }
    };

    let mut category = v1::ModelCategory::Unspecified;
    if !category_name.is_empty() {
        match parse_category(category_name) {
            Some(parsed) => category = parsed,
            None => {
                out::error_line(&format!(
                    "unknown category '{category_name}' ({})",
                    category_choices()
                ));
                return 2;
            }
        }
        engine_hint.resolve_options.has_category = true;
        engine_hint.resolve_options.category = category;
    }

    let resolved = match model_ref::resolve(reference, Some(&engine_hint.resolve_options)) {
        Ok(resolved) => resolved,
        Err((_, error)) => {
            out::error_line(&error);
            return 1;
        }
    };

    let _progress_scope =
        DownloadProgressScope::new(&resolved.model_id, !options.no_progress && !options.json);
    let mut request = v1::ModelLoadRequest {
        model_id: resolved.model_id.clone(),
        validate_availability: true,
        ..Default::default()
    };
    if category != v1::ModelCategory::Unspecified {
        request.category = Some(category as i32);
    }
    // An explicit --engine is honoured whatever the ref resolved to. Gating this
    // on `!resolved.from_catalog` silently DISCARDED the flag for catalog
    // entries — the SDK never saw the requested pin. When the flag is absent
    // engine_hint.framework is UNSPECIFIED, so catalog entries still fall back
    // to their own declared framework exactly as before. Mirrors cmd_run.rs /
    // cmd_embed.rs.
    if let Some(framework) = framework_override(engine_hint.framework) {
        request.framework = Some(framework);
    }

    let bytes = crate::io::proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/out_buffer are valid for the duration of this call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_load_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result: v1::ModelLoadResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            out::error_line("model load failed: ");
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("model load failed: {error}"));
            return 1;
        }
    };
    if let Some(err) = &result.error {
        let message = if err.message.is_empty() {
            "unknown error"
        } else {
            &err.message
        };
        out::error_line(&format!("model load failed: {message}"));
        return 1;
    }

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_str("id", &result.model_id)
            .field_str("modality", model_labels::category_i32(result.category))
            .field_str("backend", model_labels::backend_i32(result.framework))
            .field_str("path", &result.resolved_path)
            .field_bool("already_loaded", result.already_loaded)
            .end_object();
        out::result_line(json.str());
    } else {
        out::result_line(&format!(
            "loaded {} → {}",
            result.model_id, result.resolved_path
        ));
    }
    0
}

fn run_unload(options: &GlobalOptions, category_name: &str) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    let mut request = v1::ModelUnloadRequest::default();
    if category_name.is_empty() {
        request.unload_all = true;
    } else {
        match parse_category(category_name) {
            Some(category) => request.category = Some(category as i32),
            None => {
                out::error_line(&format!(
                    "unknown category '{category_name}' ({})",
                    category_choices()
                ));
                return 2;
            }
        }
    }

    let bytes = crate::io::proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/out_buffer are valid for the duration of this call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_unload_proto(bytes.as_ptr(), bytes.len(), out_buffer.as_mut_ptr())
    };
    let result: v1::ModelUnloadResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS => result,
        Ok(_) => {
            out::error_line("unload failed: ");
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("unload failed: {error}"));
            return 1;
        }
    };
    // Freeing everything when nothing is resident is not a failure, even though
    // the lifecycle service reports "no loaded model matched".
    if result.error.is_some() && !(request.unload_all && result.unloaded_model_ids.is_empty()) {
        let message = result
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "unknown error".to_string());
        out::error_line(&format!("unload failed: {message}"));
        return 1;
    }

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object().begin_array("unloaded");
        for id in &result.unloaded_model_ids {
            json.begin_array_object().field_str("id", id).end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
    } else if result.unloaded_model_ids.is_empty() {
        out::result_line("nothing was loaded");
    } else {
        for id in &result.unloaded_model_ids {
            out::result_line(&format!("unloaded {id}"));
        }
    }
    0
}

// Loaded model for one category, or an empty id when nothing is resident.
fn loaded_model_id(category: v1::ModelCategory) -> String {
    let request = v1::CurrentModelRequest {
        category: Some(category as i32),
        ..Default::default()
    };
    let bytes = crate::io::proto::serialize(&request);
    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/out_buffer are valid for the duration of this call.
    let proto_rc = unsafe {
        sys::rac_model_lifecycle_current_model_proto(
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    match parse_proto_buffer::<v1::CurrentModelResult>(out_buffer) {
        Ok(result) if proto_rc == sys::SUCCESS && result.found => result.model_id,
        _ => String::new(),
    }
}

struct Storage {
    used_bytes: u64,
    free_bytes: u64,
}

fn recursive_file_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // skip_permission_denied: an unreadable child is silently excluded,
        // not a hard failure of the whole scan.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            total += recursive_file_size(&path);
        } else if metadata.is_file() {
            total += metadata.len();
        } else if metadata.is_symlink() {
            // C++'s fs::recursive_directory_iterator + is_regular_file(ec)/
            // file_size(ec) resolve through status(), which follows symlinks,
            // so a symlink to a regular file counts the target's real size.
            // It does not recurse into symlinked directories either (the
            // iterator's default follow_directory_symlink is off), so only
            // symlinks that resolve to a regular file are counted here; a
            // broken symlink or one pointing at a directory contributes 0,
            // matching is_regular_file(ec) returning false there.
            if let Ok(target_metadata) = std::fs::metadata(&path) {
                if target_metadata.is_file() {
                    total += target_metadata.len();
                }
            }
        }
    }
    total
}

#[cfg(unix)]
fn disk_space_available(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string; stat is a correctly
    // sized, zeroed out-param for the duration of this call.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    // The field widths differ by platform (fsblkcnt_t is 32-bit on macOS).
    #[allow(clippy::unnecessary_cast)]
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
fn disk_space_available(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_bytes: u64 = 0;
    // SAFETY: wide is a valid NUL-terminated UTF-16 buffer; free_bytes is a
    // valid out-param for the duration of this call.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(free_bytes)
}

fn models_storage(models_dir: &str) -> Storage {
    let mut storage = Storage {
        used_bytes: recursive_file_size(Path::new(models_dir)),
        free_bytes: 0,
    };
    // The models directory may not exist yet; free space comes from the
    // nearest ancestor that does.
    let mut path = PathBuf::from(models_dir);
    loop {
        if let Some(available) = disk_space_available(&path) {
            storage.free_bytes = available;
            break;
        }
        match path.parent() {
            Some(parent) if parent != path.as_path() => path = parent.to_path_buf(),
            _ => break,
        }
    }
    storage
}

fn run_state(options: &GlobalOptions) -> i32 {
    let Ok(env) = bootstrap(options) else {
        return 1;
    };

    let mut loaded: Vec<(&'static str, String)> = Vec::new();
    for category in STATE_CATEGORIES {
        let id = loaded_model_id(category);
        if !id.is_empty() {
            loaded.push((model_labels::category(category), id));
        }
    }
    let storage = models_storage(&env.models_dir);

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object().begin_array("loaded");
        for (modality, id) in &loaded {
            json.begin_array_object()
                .field_str("modality", modality)
                .field_str("id", id)
                .end_object();
        }
        json.end_array()
            .field_i64("storage_used_bytes", storage.used_bytes as i64)
            .field_i64("storage_free_bytes", storage.free_bytes as i64)
            .end_object();
        out::result_line(json.str());
        return 0;
    }

    if loaded.is_empty() {
        out::result_line("no models loaded");
    } else {
        let rows: Vec<Vec<String>> = loaded
            .iter()
            .map(|(modality, id)| vec![modality.to_string(), id.clone()])
            .collect();
        out::table(&["MODALITY".to_string(), "LOADED".to_string()], &rows);
    }
    out::result_line(&format!(
        "storage    {} used, {} free",
        out::human_bytes(storage.used_bytes),
        out::human_bytes(storage.free_bytes)
    ));
    0
}

pub fn register_models(app: &mut App) {
    let ns = app.add_subcommand("models", "Manage local models");
    ns.require_subcommand(1, 1);

    let list_cmd = ns.add_subcommand("list", "List downloaded models (--all for the catalog)");
    list_cmd.alias("ls");
    list_cmd.footer(&examples_footer(&[Example::new(
        "wally models list --all",
        "Browse the whole catalog",
    )]));
    configure_models_list(list_cmd);

    let show_cmd = ns.add_subcommand("show", "Show a model's details");
    show_cmd.alias("get");
    show_cmd.footer(&examples_footer(&[Example::new(
        "wally models show granite-4.2-8b",
        "",
    )]));
    configure_models_get(show_cmd);

    let pull_cmd = ns.add_subcommand("pull", "Download a model");
    pull_cmd.alias("download");
    pull_cmd.footer(&examples_footer(&[
        Example::new(
            "wally models pull qwen3-4b-instruct-2507",
            "Certified for coding harnesses",
        ),
        Example::new(
            "wally models pull hf.co/<org>/<repo>/<file>",
            "From Hugging Face",
        ),
    ]));
    configure_models_download(pull_cmd);

    let delete_cmd = ns.add_subcommand("rm", "Delete a downloaded model");
    delete_cmd.footer(&examples_footer(&[Example::new(
        "wally models rm qwen3-4b-instruct-2507",
        "",
    )]));
    delete_cmd.alias("remove");
    delete_cmd.alias("delete");
    configure_models_delete(delete_cmd);

    // Advanced lifecycle verbs stay callable but out of the --help tree (empty
    // group), so `models` shows just the four CRUD branches.
    let register_cmd = ns.add_subcommand(
        "register",
        "Add a model from a URL or hf.co ref to the registry",
    );
    register_cmd.group("");
    register_cmd
        .add_option(
            "model",
            ValueType::Text,
            "hf.co/org/repo/file, hf:// or http(s) URL",
        )
        .required();
    register_cmd.add_option(
        "--engine",
        ValueType::Text,
        "Pin the inference engine (mlx, llamacpp, onnx, sherpa)",
    );
    register_cmd.callback(|p, g| {
        let reference = p.get_str("model").unwrap_or_default();
        let engine = p.get_str("--engine").unwrap_or_default();
        run_register(g, &reference, &engine)
    });

    let load_cmd = ns.add_subcommand("load", "Load a model now instead of on first use");
    load_cmd.group("");
    load_cmd
        .add_option(
            "model",
            ValueType::Text,
            "Model id, alias, hf.co/... ref or URL",
        )
        .required();
    load_cmd.add_option(
        "--engine,--framework",
        ValueType::Text,
        "Pin the inference engine (mlx, llamacpp, onnx, sherpa)",
    );
    load_cmd.add_option(
        "--category",
        ValueType::Text,
        &format!("Load it as this modality ({})", category_choices()),
    );
    load_cmd.callback(|p, g| {
        let reference = p.get_str("model").unwrap_or_default();
        let engine = p.get_str("--engine").unwrap_or_default();
        let category = p.get_str("--category").unwrap_or_default();
        run_load(g, &reference, &engine, &category)
    });

    let unload_cmd = ns.add_subcommand("unload", "Free loaded models, all of them by default");
    unload_cmd.group("");
    unload_cmd.add_option(
        "category",
        ValueType::Text,
        &format!("Only free this modality ({})", category_choices()),
    );
    unload_cmd.callback(|p, g| {
        let category = p.get_str("category").unwrap_or_default();
        run_unload(g, &category)
    });

    let state_cmd = ns.add_subcommand("state", "Report resident models and disk usage");
    state_cmd.group("");
    state_cmd.callback(|_p, g| run_state(g));
}

pub fn register_models_aliases(app: &mut App) {
    // CLI11's help banner always names the subcommand's primary registered
    // name, never the alias actually typed (App::get_display_name() ignores
    // it) — so with "list" primary, `wally ls --help` rendered "Usage: wally
    // list [OPTIONS]". Registering the shorter name as primary matches `rm`
    // below, which already makes the same call for the same reason.
    let list = app.add_subcommand("ls", "List models (alias of `models list`)");
    list.alias("list");
    configure_models_list(list);

    configure_models_get(app.add_subcommand("show", "Show model details (alias of `models get`)"));
    configure_models_download(
        app.add_subcommand("pull", "Download a model (alias of `models download`)"),
    );

    let remove = app.add_subcommand("rm", "Delete a model (alias of `models delete`)");
    remove.alias("remove");
    configure_models_delete(remove);
}

#[cfg(test)]
mod recursive_file_size_tests {
    use super::recursive_file_size;
    use std::path::Path;

    #[test]
    fn symlinked_regular_file_counts_target_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob.gguf");
        std::fs::write(&blob, b"hello world data").expect("write blob");

        let model_dir = dir.path().join("somemodel");
        std::fs::create_dir(&model_dir).expect("mkdir");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&blob, model_dir.join("model.gguf")).expect("symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&blob, model_dir.join("model.gguf")).expect("symlink");

        // C++'s recursive_directory_iterator + is_regular_file(ec)/file_size(ec)
        // resolve through status(), which follows the symlink to the real
        // 16-byte target; the old Rust code used the non-following
        // DirEntry::metadata() and silently skipped the entry (0 bytes).
        assert_eq!(
            recursive_file_size(Path::new(model_dir.to_str().unwrap())),
            16
        );
    }

    #[test]
    #[cfg(unix)]
    fn broken_symlink_contributes_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("dangling"))
            .expect("symlink");
        assert_eq!(recursive_file_size(dir.path()), 0);
    }
}

#[cfg(test)]
mod framework_override_tests {
    use super::framework_override;
    use crate::io::proto::v1;

    // The helper takes only the engine, on purpose: the bug was gating an
    // explicit --engine on `!resolved.from_catalog`, so a signature that
    // cannot see `from_catalog` at all proves it can no longer suppress the
    // override.
    #[test]
    fn explicit_engine_is_returned() {
        assert_eq!(
            framework_override(v1::InferenceFramework::Mlx),
            Some(v1::InferenceFramework::Mlx as i32)
        );
    }

    #[test]
    fn absent_engine_defers_to_the_catalog() {
        assert_eq!(
            framework_override(v1::InferenceFramework::Unspecified),
            None
        );
    }
}
