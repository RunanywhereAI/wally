//! Models downloaded on this machine, for coding tools (port of
//! src/harness/local_models.cpp).

use std::fs;
use std::path::{Path, PathBuf};

use crate::catalog;
use crate::sys;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalModel {
    pub id: String,
    pub framework: String,
    pub dir: String,
    pub path: String,
    pub bytes: i64,
}

fn is_weight_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("gguf") | Some("safetensors") | Some("onnx") | Some("bin")
    )
}

/// Written by the download orchestrator as files land rather than once they all
/// have, so a partial download carries one too. Presence means "this came from
/// a download", not "the download finished".
const MANIFEST: &str = ".rac-manifest.binpb";

/// Sums bytes and finds the manifest/first weight file for one model directory,
/// walking recursively. Best-effort: a file that vanishes mid-walk is skipped
/// rather than failing the whole listing.
fn scan_model_dir(dir: &Path) -> (String, bool, i64) {
    let mut weights = String::new();
    let mut manifest = false;
    let mut bytes: i64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = match fs::read_dir(&current) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // `file_type()` describes the link itself, not what it points at,
            // so a symlinked weight file would otherwise look like neither a
            // directory nor a regular file and vanish from the walk.
            // `fs::metadata` follows the link to learn the target's real
            // type and size -- `DirEntry::metadata()` (used below for a
            // plain file) does not follow a symlink either, and lstat's
            // reported size for one is the length of the target path
            // string, not the target's content, so the resolved metadata
            // fetched here is reused for the byte count too.
            let target_metadata = if file_type.is_symlink() {
                match fs::metadata(&path) {
                    Ok(target) => Some(target),
                    // Broken link, or a stat that failed: skip, same as this
                    // function's other best-effort cases.
                    Err(_) => continue,
                }
            } else {
                None
            };
            let is_dir = match &target_metadata {
                Some(target) if target.is_dir() => true,
                Some(target) if target.is_file() => false,
                // A symlink to a special file (socket, device, ...): skip.
                Some(_) => continue,
                None if file_type.is_dir() => true,
                None if file_type.is_file() => false,
                None => continue,
            };
            if is_dir {
                stack.push(path);
                continue;
            }
            // A file that vanished between the listing and the stat is skipped
            // rather than counted as size 0, which would otherwise silently
            // undercount the total.
            let size = match &target_metadata {
                Some(target) => Some(target.len()),
                None => entry.metadata().ok().map(|m| m.len()),
            };
            if let Some(size) = size {
                bytes += size as i64;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some(MANIFEST) {
                manifest = true;
            } else if weights.is_empty() && is_weight_file(&path) {
                weights = path.to_string_lossy().into_owned();
            }
        }
    }
    (weights, manifest, bytes)
}

/// Models present under `home` right now, found by walking the storage tree.
///
/// Walking rather than asking the registry: this only has to answer "is there
/// something here the local server can open", and the walk says that about a
/// model placed by hand as readily as one that was downloaded.
pub fn local_models(home: &str) -> Vec<LocalModel> {
    let mut models = Vec::new();
    if home.is_empty() {
        return models;
    }

    // Both layouts are real: the SDK's path docs describe
    // {base}/RunAnywhere/Models, and the desktop default base directory already
    // ends in "runanywhere", so models land directly under {base}/Models.
    let mut root: PathBuf = Path::new(home).join("RunAnywhere").join("Models");
    if !root.is_dir() {
        root = Path::new(home).join("Models");
    }
    if !root.is_dir() {
        return models;
    }

    let Ok(frameworks) = fs::read_dir(&root) else {
        return models;
    };
    for framework_entry in frameworks.flatten() {
        let Ok(framework_type) = framework_entry.file_type() else {
            continue;
        };
        if !framework_type.is_dir() {
            continue;
        }
        let framework_name = framework_entry.file_name().to_string_lossy().into_owned();
        let Ok(entries) = fs::read_dir(framework_entry.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(entry_type) = entry.file_type() else {
                continue;
            };
            if !entry_type.is_dir() {
                continue;
            }
            let entry_path = entry.path();
            let (weights, manifest, bytes) = scan_model_dir(&entry_path);
            // A directory holding weights but no manifest was placed by hand.
            // It is still loadable, so it still counts.
            if !manifest && weights.is_empty() {
                continue;
            }
            models.push(LocalModel {
                id: entry.file_name().to_string_lossy().into_owned(),
                framework: framework_name.clone(),
                dir: entry_path.to_string_lossy().into_owned(),
                path: weights,
                bytes,
            });
        }
    }

    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// The context window a local server is started with for `model_id`, in
/// tokens. Sized from this machine's memory in tiers (8k / 16k / 32k / 64k),
/// because a coding agent's first request is a 15k-token system prompt and a
/// fixed 8k window rejected it outright. Capped at the model's own window when
/// the catalog knows it (even when that is below the 8k memory tier). One
/// function, so the server, the picker's declared limits, and the shim all
/// quote the same number.
pub fn local_context_size(model_id: &str) -> i64 {
    const FLOOR: i64 = 8192;
    const GIB: i64 = 1024 * 1024 * 1024;

    // Physical memory decides the tier. On Apple Silicon this is the unified
    // pool the GPU draws from too, which is why it stands in for VRAM here.
    let mut tier = FLOOR;
    // SAFETY: rac_get_platform_adapter() returns either null or a pointer the
    // SDK keeps valid for the process lifetime; the function pointer slot is
    // checked for null before it is called.
    unsafe {
        let adapter = sys::rac_get_platform_adapter();
        if !adapter.is_null() {
            if let Some(get_memory_info) = (*adapter).get_memory_info {
                let mut memory = sys::rac_memory_info_t {
                    total_bytes: 0,
                    available_bytes: 0,
                    used_bytes: 0,
                };
                if get_memory_info(&mut memory, (*adapter).user_data) == sys::SUCCESS
                    && memory.total_bytes > 0
                {
                    let total = memory.total_bytes as i64;
                    if total >= 48 * GIB {
                        tier = 65536;
                    } else if total >= 24 * GIB {
                        tier = 32768;
                    } else if total >= 12 * GIB {
                        tier = 16384;
                    }
                }
            }
        }
    }

    // The model's own window caps it: asking llama.cpp for more than the model
    // was trained on stretches RoPE and degrades every answer. 0 means the
    // catalog does not know, and a model that is not in the catalog at all
    // (an hf.co ref, a hand-placed folder) gets the tier as is.
    let mut window = tier;
    if let Some(entry) = catalog::find(model_id) {
        if entry.context_length > 0 {
            window = tier.min(entry.context_length as i64);
        }
    }
    window
}

/// Leave most of a local context for the coding prompt and conversation.
/// Zero means no local context was configured (a hosted endpoint).
pub fn local_output_size(context_window: i64) -> i64 {
    if context_window > 1 {
        4096.min(context_window / 4)
    } else {
        0
    }
}
