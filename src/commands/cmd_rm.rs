//! `wally models rm <model>` (alias `wally models delete`/`wally models
//! remove`) — delete downloaded files + unregister.
//!
//! File deletion is CLI-owned (registry remove only unregisters, per the
//! rac_model_registry_remove contract). Deletion targets come from the
//! registry's local_path and are confined to the models directory before
//! anything is removed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::catalog::model_ref;
use crate::cli::{App, ValueType};
use crate::commands::model_setup::refresh_registry;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, v1, ProtoBuffer};
use crate::sys;
use crate::util::term;

// Resolve the directory to delete for a model. Single-file artifacts live in
// a per-model folder ({models}/{framework}/{id}/file) — delete the folder when
// its name matches the model id, otherwise just the file itself.
fn deletion_target(model: &v1::ModelInfo) -> PathBuf {
    let local = Path::new(&model.local_path);
    if local.is_dir() {
        return local.to_path_buf();
    }
    if let Some(parent) = local.parent() {
        if parent.file_name().and_then(|n| n.to_str()) == Some(model.id.as_str()) {
            return parent.to_path_buf();
        }
    }
    local.to_path_buf()
}

/// `std::filesystem::weakly_canonical`: canonicalize the longest existing
/// leading path, then append the (lexically normalized) remainder.
fn weakly_canonical(path: &Path) -> Option<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                let mut result = canonical;
                for part in remainder.iter().rev() {
                    result.push(part);
                }
                return Some(result);
            }
            Err(_) => {
                let file_name = existing.file_name()?.to_os_string();
                remainder.push(file_name);
                if !existing.pop() {
                    return None;
                }
            }
        }
    }
}

// The target must live strictly inside the models root.
fn confined_to(target: &Path, models_root: &Path) -> bool {
    let Some(canonical_target) = weakly_canonical(target) else {
        return false;
    };
    let Some(canonical_root) = weakly_canonical(models_root) else {
        return false;
    };
    let target_str = canonical_target.to_string_lossy().into_owned();
    let root_str = canonical_root.to_string_lossy().into_owned();
    target_str.len() > root_str.len() + 1 && target_str.starts_with(&format!("{root_str}/"))
}

fn confirm_on_tty(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut buffer = String::new();
    if std::io::stdin().read_line(&mut buffer).is_err() {
        return false;
    }
    matches!(buffer.chars().next(), Some('y') | Some('Y'))
}

fn run_rm(options: &GlobalOptions, reference: &str, force: bool) -> i32 {
    let Ok(env) = bootstrap(options) else {
        return 1;
    };

    let resolved = match model_ref::resolve(reference, None) {
        Ok(resolved) => resolved,
        Err((_, error)) => {
            out::error_line(&error);
            return 1;
        }
    };

    // Link on-disk artifacts before deciding what to delete — mirrors
    // list/ensure_model_ready so rm sees the same downloaded state.
    if let Err(error) = refresh_registry() {
        out::status_line(&format!("warning: registry refresh failed: {error}"));
    }

    let mut model_out = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle; model_out is a valid out-param for this call only.
    let model_id_c = std::ffi::CString::new(resolved.model_id.as_str()).unwrap_or_default();
    let get_rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            model_id_c.as_ptr(),
            model_out.as_mut_ptr(),
        )
    };
    // parse unconditionally: it interprets the {status,error_message} envelope
    // and frees the buffer on every path (no leak on get failure).
    let model: v1::ModelInfo = match parse_proto_buffer(model_out) {
        Ok(model) if get_rc == sys::SUCCESS => model,
        _ => {
            out::error_line(&format!("model not found: {}", resolved.model_id));
            return 1;
        }
    };

    let mut freed_bytes: u64 = 0;
    if !model.local_path.is_empty() {
        let target = deletion_target(&model);
        if !confined_to(&target, Path::new(&env.models_dir)) {
            out::error_line(&format!(
                "refusing to delete {} (outside models directory {})",
                target.display(),
                env.models_dir
            ));
            return 1;
        }
        if target.exists() {
            if !force
                && term::stdin_is_tty()
                && !confirm_on_tty(&format!("delete {}?", target.display()))
            {
                out::status_line("aborted");
                return 1;
            }
            // Best-effort size accounting before removal.
            if target.is_dir() {
                freed_bytes = directory_size(&target);
            } else if let Ok(metadata) = std::fs::metadata(&target) {
                if metadata.is_file() {
                    freed_bytes = metadata.len();
                }
            }
            if let Err(error) = remove_all(&target) {
                out::error_line(&format!("failed to delete {}: {error}", target.display()));
                return 1;
            }
        }
    } else {
        out::status_line(&format!("{} has no downloaded files", resolved.model_id));
    }

    let mut remove_out = ProtoBuffer::new();
    // SAFETY: as above.
    let proto_rc = unsafe {
        sys::rac_model_registry_remove_proto_buffer(
            sys::rac_get_model_registry(),
            model_id_c.as_ptr(),
            remove_out.as_mut_ptr(),
        )
    };
    if let Err(error) = parse_proto_buffer::<v1::ModelDeleteResult>(remove_out).and_then(|_| {
        if proto_rc == sys::SUCCESS {
            Ok(())
        } else {
            Err(out::describe_result(proto_rc))
        }
    }) {
        out::status_line(&format!("warning: registry unregister failed: {error}"));
    }

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_str("id", &resolved.model_id)
            .field_i64("freed_bytes", freed_bytes as i64)
            .end_object();
        out::result_line(json.str());
    } else {
        let suffix = if freed_bytes > 0 {
            format!(" (freed {})", out::human_bytes(freed_bytes))
        } else {
            String::new()
        };
        out::result_line(&format!("deleted {}{suffix}", resolved.model_id));
    }
    0
}

fn directory_size(target: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(target) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_dir() {
                total += directory_size(&path);
            } else if metadata.is_file() {
                total += metadata.len();
            }
        }
    }
    total
}

fn remove_all(target: &Path) -> std::io::Result<()> {
    if target.is_dir() {
        std::fs::remove_dir_all(target)
    } else {
        std::fs::remove_file(target)
    }
}

pub fn configure_models_delete(cmd: &mut App) {
    cmd.add_option("model", ValueType::Text, "Model id or alias")
        .required();
    cmd.add_flag("-f,--force", "Skip the confirmation prompt");
    cmd.callback(|p, g| {
        let reference = p.get_str("model").unwrap_or_default();
        let force = p.flag("--force");
        run_rm(g, &reference, force)
    });
}
