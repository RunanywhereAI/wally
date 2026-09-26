//! `wally models list` (alias `wally models ls`) — downloaded models by
//! default, the whole catalog with --all (port of src/commands/cmd_list.cpp).
//!
//! The registry is refreshed with rescan_local so on-disk artifacts pulled by
//! previous runs (or by the test rig / playground tooling) are linked before
//! listing.

use std::collections::HashMap;

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::App;
use crate::commands::model_labels;
use crate::commands::model_setup::refresh_registry;
use crate::io::output as out;
use crate::io::proto::{parse_proto_buffer, v1, ProtoBuffer};
use crate::sys;

// The same model is registered once per backend it runs on (llama.cpp / MLX /
// ANE / NPU). `models list` collapses those into one row keyed by the catalog's
// merge_key, joining the backends into "mlx/llama.cpp"-style tags. Lower rank =
// listed first in the joined tag and preferred for the row's name/size.
fn backend_rank(framework: v1::InferenceFramework) -> i32 {
    match framework {
        v1::InferenceFramework::Mlx => 0,
        v1::InferenceFramework::LlamaCpp => 1,
        v1::InferenceFramework::Coreml => 2,
        v1::InferenceFramework::Qhexrt => 3,
        _ => 4,
    }
}

struct GroupedRow {
    id: String, // backend-neutral merge key shown on every platform
    #[allow(dead_code)]
    // id of the variant that set size_bytes; kept for parity with the C++ struct
    size_id: String,
    local_path: String, // local_path of the variant backing `id`, if downloaded
    name: String,
    category: v1::ModelCategory,
    size_bytes: i64,
    name_rank: i32, // rank of the variant that set name/category
    size_rank: i32, // rank of the variant that set a positive size
    // Best-ranked downloaded variant supplies a representative local path.
    path_rank: i32,
    downloaded: bool,
    harness_compatible: bool,
    // Distinct backends, ordered by (rank, label) so the join is stable.
    backends: std::collections::BTreeSet<(i32, &'static str)>,
}

impl Default for GroupedRow {
    fn default() -> Self {
        GroupedRow {
            id: String::new(),
            size_id: String::new(),
            local_path: String::new(),
            name: String::new(),
            category: v1::ModelCategory::Unspecified,
            size_bytes: 0,
            name_rank: i32::MAX,
            size_rank: i32::MAX,
            path_rank: i32::MAX,
            downloaded: false,
            harness_compatible: false,
            backends: std::collections::BTreeSet::new(),
        }
    }
}

// A short "how do I download one?" header for the human list. The pull id
// differs by backend, so show one example per backend this build can run:
// llama.cpp where the kit has it (not the Windows ARM64 kit); on Apple also
// MLX. Never printed in --json.
fn print_pull_examples() {
    out::result_line("Download a model with `wally models pull <id>`:");
    #[cfg(wally_has_llamacpp)]
    out::result_line("  wally models pull qwen3-4b-instruct-2507  # llama.cpp");
    #[cfg(target_os = "macos")]
    out::result_line("  wally models pull mlx-qwen3-4b-instruct-2507  # MLX (Apple GPU)");
    // TEMP(ane-cut): no ANE rows in the catalog, so nothing to point at.
    // out::result_line("  wally models pull ane-lfm2.5-350m   # ANE (Apple Neural Engine)");
    out::result_line("");
}

fn join_backends(row: &GroupedRow) -> String {
    let mut joined = String::new();
    for (_, label) in &row.backends {
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(label);
    }
    joined
}

// Collapse per-backend variants of the same model into one row, grouped by
// the catalog merge_key (a non-catalog id groups with itself). `row.id`
// starts out as that merge key, but a downloaded variant overrides it with
// its own id so copying the row's id inspects/pulls the same backend that's
// actually on disk. Backend-specific ids remain accepted by pull/show/rm, but
// the list stays stable across macOS, Linux, and Windows. Insertion order is
// kept so the list reads the same as the registry. Pulled out of run_list so
// it's testable without the FFI registry call.
fn group_models(
    models: &[v1::ModelInfo],
    downloaded_ids: &std::collections::HashSet<String>,
    show_all: bool,
) -> (Vec<String>, HashMap<String, GroupedRow>) {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, GroupedRow> = HashMap::new();
    for model in models {
        let is_downloaded = downloaded_ids.contains(&model.id)
            || model.registry_status == Some(v1::ModelRegistryStatus::Downloaded as i32);
        if !show_all && !is_downloaded {
            continue;
        }
        // LLM-only surface: a downloaded non-LLM model restored from a manifest
        // must not reappear in the list.
        if model.category != v1::ModelCategory::Language as i32 {
            continue;
        }
        let key = crate::catalog::merge_key_for(&model.id);
        let row = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            GroupedRow {
                id: key.clone(),
                ..GroupedRow::default()
            }
        });
        let framework = v1::InferenceFramework::try_from(model.framework)
            .unwrap_or(v1::InferenceFramework::Unspecified);
        let rank = backend_rank(framework);
        row.backends
            .insert((rank, model_labels::short_backend(framework)));
        row.downloaded = row.downloaded || is_downloaded;
        let catalog_entry = crate::catalog::find(&model.id);
        row.harness_compatible = row.harness_compatible
            || catalog_entry
                .map(|entry| entry.harness_compatible)
                .unwrap_or(false);
        if is_downloaded && rank < row.path_rank {
            row.path_rank = rank;
            row.local_path = model.local_path.clone();
            // Show the id of the variant actually on disk, not the merge
            // key, so copying it inspects/pulls the same backend that's
            // downloaded rather than falling back to catalog defaults.
            row.id = model.id.clone();
        }
        if rank < row.name_rank {
            row.name_rank = rank;
            row.name = model.name.clone();
            row.category = v1::ModelCategory::try_from(model.category)
                .unwrap_or(v1::ModelCategory::Unspecified);
        }
        let size = model.download_size_bytes;
        if size > 0 && rank < row.size_rank {
            row.size_rank = rank;
            row.size_bytes = size;
            row.size_id = model.id.clone();
        }
    }
    (order, groups)
}

fn run_list(options: &GlobalOptions, show_all: bool) -> i32 {
    let Ok(_env) = bootstrap(options) else {
        return 1;
    };

    if let Err(error) = refresh_registry() {
        out::status_line(&format!("warning: registry refresh failed: {error}"));
    }

    // Full list + downloaded list; membership marks the DOWNLOADED column.
    let mut all_out = ProtoBuffer::new();
    // SAFETY: rac_get_model_registry() returns the process-wide registry
    // handle (valid for the process lifetime); all_out is a valid out-param.
    let proto_rc = unsafe {
        sys::rac_model_registry_list_proto_buffer(
            sys::rac_get_model_registry(),
            all_out.as_mut_ptr(),
        )
    };
    let all_models: v1::ModelInfoList = match parse_proto_buffer(all_out) {
        Ok(models) if proto_rc == sys::SUCCESS => models,
        Ok(_) => {
            out::error_line("failed to list models: ");
            return 1;
        }
        Err(error) => {
            out::error_line(&format!("failed to list models: {error}"));
            return 1;
        }
    };

    let mut downloaded_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    {
        let mut downloaded_out = ProtoBuffer::new();
        // SAFETY: as above.
        let rc = unsafe {
            sys::rac_model_registry_list_downloaded_proto_buffer(
                sys::rac_get_model_registry(),
                downloaded_out.as_mut_ptr(),
            )
        };
        if rc == sys::SUCCESS {
            if let Ok(downloaded) = parse_proto_buffer::<v1::ModelInfoList>(downloaded_out) {
                for model in &downloaded.models {
                    downloaded_ids.insert(model.id.clone());
                }
            }
        }
    }

    let (order, groups) = group_models(&all_models.models, &downloaded_ids, show_all);

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object().begin_array("models");
        for key in &order {
            let row = &groups[key];
            json.begin_array_object()
                .field_str("id", &row.id)
                .field_str("name", &row.name)
                .field_str("modality", model_labels::category(row.category))
                .field_str("backend", &join_backends(row))
                .field_i64("size_bytes", row.size_bytes)
                .field_bool("downloaded", row.downloaded)
                .field_bool("harness_compatible", row.harness_compatible)
                // Path of the variant `id` refers to; empty when nothing in
                // the group is downloaded (mirrors the pre-merge shape, which
                // callers already treat "" as "not downloaded").
                .field_str("local_path", &row.local_path)
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return 0;
    }

    print_pull_examples();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for key in &order {
        let row = &groups[key];
        rows.push(vec![
            row.id.clone(),
            model_labels::category(row.category).to_string(),
            join_backends(row),
            if row.size_bytes > 0 {
                out::human_bytes(row.size_bytes as u64)
            } else {
                "-".to_string()
            },
            if row.downloaded { "yes" } else { "no" }.to_string(),
            if row.harness_compatible {
                "[harness-compatible]"
            } else {
                ""
            }
            .to_string(),
        ]);
    }

    if rows.is_empty() {
        out::result_line(if show_all {
            "no models registered"
        } else {
            "no models downloaded — try `wally models list --all` then `wally models pull <id>`"
        });
        return 0;
    }
    out::table(
        &["ID", "MODALITY", "BACKEND", "SIZE", "DOWNLOADED", "TAGS"].map(String::from),
        &rows,
    );
    0
}

pub fn configure_models_list(cmd: &mut App) {
    cmd.add_flag("--all,-a", "Include catalog models not yet downloaded");
    cmd.callback(|p, g| {
        let show_all = p.flag("--all");
        run_list(g, show_all)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // "qwen3-4b-instruct-2507" (llama.cpp) and "mlx-qwen3-4b-instruct-2507-4bit"
    // (MLX) are real catalog entries sharing merge_key "qwen3-4b-instruct-2507"
    // (src/catalog/catalog.rs), the same pair print_pull_examples points at.
    fn llamacpp_variant(local_path: &str) -> v1::ModelInfo {
        v1::ModelInfo {
            id: "qwen3-4b-instruct-2507".to_string(),
            name: "Qwen3 4B Instruct 2507 Q8_0".to_string(),
            category: v1::ModelCategory::Language as i32,
            framework: v1::InferenceFramework::LlamaCpp as i32,
            local_path: local_path.to_string(),
            ..Default::default()
        }
    }

    fn mlx_variant(local_path: &str) -> v1::ModelInfo {
        v1::ModelInfo {
            id: "mlx-qwen3-4b-instruct-2507-4bit".to_string(),
            name: "Qwen3 4B Instruct 2507".to_string(),
            category: v1::ModelCategory::Language as i32,
            framework: v1::InferenceFramework::Mlx as i32,
            local_path: local_path.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn downloaded_variant_id_wins_over_merge_key() {
        // Only the MLX variant is downloaded. Without the fix, row.id stayed
        // the merge key even though the merge key names no downloaded
        // backend here, so copying it would pull/inspect the wrong id.
        let models = vec![
            llamacpp_variant(""),
            mlx_variant("/models/mlx-qwen3-4b-instruct-2507-4bit"),
        ];
        let downloaded: std::collections::HashSet<String> =
            std::iter::once("mlx-qwen3-4b-instruct-2507-4bit".to_string()).collect();
        let (order, groups) = group_models(&models, &downloaded, true);
        assert_eq!(order, vec!["qwen3-4b-instruct-2507".to_string()]);
        let row = &groups["qwen3-4b-instruct-2507"];
        assert_eq!(row.id, "mlx-qwen3-4b-instruct-2507-4bit");
        assert_eq!(row.local_path, "/models/mlx-qwen3-4b-instruct-2507-4bit");
    }

    #[test]
    fn catalog_only_row_keeps_merge_key_when_nothing_downloaded() {
        // Neither variant downloaded: row.id stays the merge key, same as
        // before the fix (the id-override branch only runs for a downloaded
        // variant).
        let models = vec![llamacpp_variant(""), mlx_variant("")];
        let downloaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (_, groups) = group_models(&models, &downloaded, true);
        let row = &groups["qwen3-4b-instruct-2507"];
        assert_eq!(row.id, "qwen3-4b-instruct-2507");
        assert!(!row.downloaded);
    }
}
