//! `wally models list` (alias `wally models ls`) — downloaded models by
//! default; the whole local catalog with --local; the signed-in account's
//! cloud models with --cloud; both with --all (port of
//! src/commands/cmd_list.cpp).
//!
//! The registry is refreshed with rescan_local so on-disk artifacts pulled by
//! previous runs (or by the test rig / playground tooling) are linked before
//! listing.

use std::collections::HashMap;

use crate::account::{ConsoleClient, ConsoleSession};
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
    // Every member as (id, rank, download size), so the row's size can be
    // taken from the variant its id actually pulls once the id is settled.
    variants: Vec<(String, i32, i64)>,
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
            variants: Vec::new(),
        }
    }
}

// The table id is the model name, with no backend prefix. The same two
// spellings work for every row: the id as written is llama.cpp, and `mlx-`
// in front of it is the Apple GPU build. Never printed in --json.
fn print_pull_examples() {
    out::result_line(
        "Each id below is the model. Pull it as written, or add mlx- for the Apple GPU build:",
    );
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
        // must not reappear in the list. LOCAL(decision): decision models are
        // part of the list, matching catalog.rs::is_llm.
        if model.category != v1::ModelCategory::Language as i32
            && model.category != v1::ModelCategory::Decision as i32
        {
            continue;
        }
        let key = crate::catalog::merge_key_for(&model.id);
        let shown = shown_model_id(&key);
        let row = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            GroupedRow {
                id: shown,
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
        // Remember where a downloaded copy lives. The shown id stays the
        // model name either way, so a downloaded MLX build does not rename
        // the row to `mlx-…`.
        let id_rank = if model.id == key { i32::MIN } else { rank };
        if is_downloaded && id_rank < row.path_rank {
            row.path_rank = id_rank;
            row.local_path = model.local_path.clone();
        }
        if rank < row.name_rank {
            row.name_rank = rank;
            row.name = model.name.clone();
            row.category = v1::ModelCategory::try_from(model.category)
                .unwrap_or(v1::ModelCategory::Unspecified);
        }
        let size = model.download_size_bytes;
        row.variants.push((model.id.clone(), rank, size));
        if size > 0 && rank < row.size_rank {
            row.size_rank = rank;
            row.size_bytes = size;
            row.size_id = model.id.clone();
        }
    }
    for row in groups.values_mut() {
        settle_row_id_and_size(row);
    }
    (order, groups)
}

/// Drop a leading `mlx-` only when the bare name is not already a different
/// catalog model. `llama3.2` is the GGUF 3B; the MLX 1B stays `mlx-llama3.2`.
fn shown_model_id(key: &str) -> String {
    let Some(stripped) = key.strip_prefix("mlx-") else {
        return key.to_string();
    };
    if crate::catalog::names_different_model(stripped, key) {
        return key.to_string();
    }
    stripped.to_string()
}

fn variant_for_shown_id(row: &GroupedRow) -> Option<String> {
    if row.variants.iter().any(|(id, _, _)| id == &row.id) {
        return Some(row.id.clone());
    }
    let prefixed = format!("mlx-{}", row.id);
    row.variants
        .iter()
        .find(|(id, _, _)| *id == prefixed || crate::catalog::merge_key_for(id) == row.id)
        .map(|(id, _, _)| id.clone())
}

// Size follows the build `pull <id>` fetches. When that name is only an
// Apple GPU model, `pull mlx-<id>` is the build, and the size is that one.
// The shown id itself is left alone.
fn settle_row_id_and_size(row: &mut GroupedRow) {
    let pulled = variant_for_shown_id(row);
    let pulled = match pulled {
        Some(id) => id,
        None => return,
    };
    if let Some((id, rank, size)) = row.variants.iter().find(|(id, _, _)| *id == pulled) {
        // An unknown size shows as `-`, not as another backend's size.
        row.size_bytes = *size;
        row.size_rank = if *size > 0 { *rank } else { i32::MAX };
        row.size_id = if *size > 0 { id.clone() } else { String::new() };
    }
}

// A list command must not sit on a dead network; the cached list is the
// fallback.
const CLOUD_LOOKUP_TIMEOUT_MS: i32 = 3_000;

/// One hosted row. `decisions_only` marks models the gateway hides from
/// /v1/models; they arrive via the price catalog instead, never hardcoded.
struct CloudRow {
    id: String,
    decisions_only: bool,
}

/// Where the cloud rows came from.
enum CloudModels {
    Live(Vec<CloudRow>),
    /// The console could not be reached; these are the ids the last
    /// successful lookup saved (`account::cached_model_ids`).
    Cached(Vec<String>),
}

/// The hosted models the signed-in account can use, or why there are none to
/// show. Ordered as the console lists them.
fn cloud_models() -> Result<CloudModels, String> {
    let mut session = ConsoleSession::open(ConsoleClient::default())?;
    match session
        .call(|client, url, token| client.fetch_models_within(url, token, CLOUD_LOOKUP_TIMEOUT_MS))
    {
        Ok(models) => {
            let mut rows: Vec<CloudRow> = models
                .into_iter()
                .map(|model| model.id)
                .filter(|id| !id.is_empty())
                .map(|id| CloudRow {
                    id,
                    decisions_only: false,
                })
                .collect();
            // The price catalog is this account's entitlements. Chat models
            // it names that /v1/models left out are gated: shown only when
            // this account holds the grant. Decision-only models never appear
            // above. A failed price lookup is not fatal: those rows stay missing.
            if let Ok(prices) = session.call(|client, url, token| {
                client.fetch_catalog_within(url, token, CLOUD_LOOKUP_TIMEOUT_MS)
            }) {
                let listed: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
                for id in crate::account::gated_chat_ids(&listed, &prices) {
                    rows.push(CloudRow {
                        id,
                        decisions_only: false,
                    });
                }
                for price in &prices {
                    if price.decisions_only
                        && !price.id.is_empty()
                        && !rows.iter().any(|row| row.id == price.id)
                    {
                        rows.push(CloudRow {
                            id: price.id.clone(),
                            decisions_only: true,
                        });
                    }
                }
            }
            Ok(CloudModels::Live(rows))
        }
        Err(reason) => {
            let cached = crate::account::cached_model_ids();
            if cached.is_empty() {
                Err(reason)
            } else {
                Ok(CloudModels::Cached(cached))
            }
        }
    }
}

/// The `[cloud]` tag, blue where stdout takes color.
fn cloud_tag(no_color: bool) -> String {
    let color = !no_color
        && crate::util::term::stdout_is_tty()
        && crate::util::getenv("NO_COLOR").is_none();
    if color {
        "\x1b[34m[cloud]\x1b[0m".to_string()
    } else {
        "[cloud]".to_string()
    }
}

/// Which rows `models list` shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Downloaded,
    Local,
    Cloud,
    All,
}

impl Scope {
    fn local_rows(self) -> bool {
        self != Scope::Cloud
    }
    fn whole_catalog(self) -> bool {
        matches!(self, Scope::Local | Scope::All)
    }
    fn cloud_rows(self) -> bool {
        matches!(self, Scope::Cloud | Scope::All)
    }
}

fn run_list(options: &GlobalOptions, scope: Scope) -> i32 {
    let show_all = scope.whole_catalog();
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

    let (order, groups) = if scope.local_rows() {
        group_models(&all_models.models, &downloaded_ids, show_all)
    } else {
        (Vec::new(), HashMap::new())
    };

    // Under --all a failure to reach the cloud never fails the local list; it
    // is one line on stderr saying why they are missing. Under --cloud the
    // cloud is all that was asked for, so the same failure is the error.
    let mut cloud_ids: Vec<CloudRow> = Vec::new();
    if scope.cloud_rows() {
        match cloud_models() {
            Ok(CloudModels::Live(rows)) => cloud_ids = rows,
            Ok(CloudModels::Cached(ids)) => {
                if !options.json {
                    out::status_line(
                        "could not reach Wally Cloud; cloud models are from the last time wally did",
                    );
                }
                cloud_ids = ids
                    .into_iter()
                    .map(|id| CloudRow {
                        id,
                        decisions_only: false,
                    })
                    .collect();
            }
            Err(reason) if scope == Scope::Cloud => {
                out::error_line(&format!("could not list cloud models: {reason}"));
                return 1;
            }
            Err(reason) if !options.json => {
                out::status_line(&format!("cloud models not shown: {reason}"));
            }
            Err(_) => {}
        }
    }

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
                .field_bool("cloud", false)
                .end_object();
        }
        for row in &cloud_ids {
            json.begin_array_object()
                .field_str("id", &row.id)
                .field_str("name", &row.id)
                .field_str(
                    "modality",
                    if row.decisions_only {
                        "decision"
                    } else {
                        "llm"
                    },
                )
                .field_str("backend", "cloud")
                .field_i64("size_bytes", 0)
                .field_bool("downloaded", false)
                .field_bool("harness_compatible", true)
                .field_str("local_path", "")
                .field_bool("cloud", true)
                .end_object();
        }
        json.end_array().end_object();
        out::result_line(json.str());
        return 0;
    }

    if scope.local_rows() {
        print_pull_examples();
    }

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

    let tag = cloud_tag(options.no_color);
    for row in &cloud_ids {
        rows.push(vec![
            row.id.clone(),
            if row.decisions_only {
                "decision"
            } else {
                "llm"
            }
            .to_string(),
            "cloud".to_string(),
            "-".to_string(),
            "-".to_string(),
            tag.clone(),
        ]);
    }

    if rows.is_empty() {
        out::result_line(if scope == Scope::Cloud {
            "no cloud models on this account"
        } else if show_all {
            "no models registered"
        } else {
            "no models downloaded — try `wally models list --local` then `wally models pull <id>`"
        });
        return 0;
    }
    out::table(
        &["ID", "MODALITY", "BACKEND", "SIZE", "DOWNLOADED", "TAGS"].map(String::from),
        &rows,
    );
    0
}

/// The scope the flags ask for, or why they contradict each other.
fn list_scope(all: bool, local: bool, cloud: bool) -> Result<Scope, &'static str> {
    match (all, local, cloud) {
        (false, false, false) => Ok(Scope::Downloaded),
        (true, false, false) => Ok(Scope::All),
        (false, true, false) => Ok(Scope::Local),
        (false, false, true) => Ok(Scope::Cloud),
        _ => Err("pick one of --all, --local or --cloud"),
    }
}

pub fn configure_models_list(cmd: &mut App) {
    cmd.add_flag(
        "--all,-a",
        "Every local catalog model, plus your account's cloud models",
    );
    cmd.add_flag("--local", "Every local catalog model, without cloud models");
    cmd.add_flag("--cloud", "Only your account's cloud models");
    cmd.callback(
        |p, g| match list_scope(p.flag("--all"), p.flag("--local"), p.flag("--cloud")) {
            Ok(scope) => run_list(g, scope),
            Err(message) => {
                out::error_line(message);
                2
            }
        },
    );
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
            download_size_bytes: 4_280_000_000,
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
            download_size_bytes: 2_360_000_000,
            name: "Qwen3 4B Instruct 2507".to_string(),
            category: v1::ModelCategory::Language as i32,
            framework: v1::InferenceFramework::Mlx as i32,
            local_path: local_path.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn downloaded_mlx_variant_keeps_the_model_name() {
        // Only the MLX variant is downloaded. The row still shows the model
        // name. The user adds mlx- when they want that build.
        let models = vec![
            llamacpp_variant(""),
            mlx_variant("/models/mlx-qwen3-4b-instruct-2507-4bit"),
        ];
        let downloaded: std::collections::HashSet<String> =
            std::iter::once("mlx-qwen3-4b-instruct-2507-4bit".to_string()).collect();
        let (order, groups) = group_models(&models, &downloaded, true);
        assert_eq!(order, vec!["qwen3-4b-instruct-2507".to_string()]);
        let row = &groups["qwen3-4b-instruct-2507"];
        assert_eq!(row.id, "qwen3-4b-instruct-2507");
        assert_eq!(row.local_path, "/models/mlx-qwen3-4b-instruct-2507-4bit");
    }

    #[test]
    fn merge_key_variant_keeps_its_id_when_both_backends_are_downloaded() {
        // Both variants downloaded. The merge key is itself a downloaded id,
        // so the row keeps it (as the C++ list did) instead of switching to
        // the better-ranked MLX variant and hiding the llama.cpp one.
        let models = vec![
            llamacpp_variant("/models/qwen3-4b-instruct-2507-q8_0.gguf"),
            mlx_variant("/models/mlx-qwen3-4b-instruct-2507-4bit"),
        ];
        let downloaded: std::collections::HashSet<String> = [
            "qwen3-4b-instruct-2507".to_string(),
            "mlx-qwen3-4b-instruct-2507-4bit".to_string(),
        ]
        .into_iter()
        .collect();
        let (_, groups) = group_models(&models, &downloaded, false);
        let row = &groups["qwen3-4b-instruct-2507"];
        assert_eq!(row.id, "qwen3-4b-instruct-2507");
        assert_eq!(row.local_path, "/models/qwen3-4b-instruct-2507-q8_0.gguf");
        assert!(row.downloaded);
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

    #[test]
    fn size_is_the_variant_the_row_id_pulls() {
        let models = vec![llamacpp_variant(""), mlx_variant("")];
        let downloaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (_, groups) = group_models(&models, &downloaded, true);
        let row = &groups["qwen3-4b-instruct-2507"];
        assert_eq!(row.id, "qwen3-4b-instruct-2507");
        assert_eq!(row.size_bytes, 4_280_000_000);
    }

    #[test]
    fn downloaded_mlx_row_keeps_the_llama_cpp_size() {
        // The shown id pulls the llama.cpp build, so the size is that build
        // even when only the MLX copy is on disk.
        let models = vec![
            llamacpp_variant(""),
            mlx_variant("/models/mlx-qwen3-4b-instruct-2507-4bit"),
        ];
        let downloaded: std::collections::HashSet<String> =
            std::iter::once("mlx-qwen3-4b-instruct-2507-4bit".to_string()).collect();
        let (_, groups) = group_models(&models, &downloaded, true);
        assert_eq!(groups["qwen3-4b-instruct-2507"].size_bytes, 4_280_000_000);
    }

    #[test]
    fn mlx_only_model_drops_the_prefix() {
        // ternary-bonsai-27b ships only an mlx- build. The row shows the
        // model name; `pull mlx-ternary-bonsai-27b` is how you get it.
        let models = vec![v1::ModelInfo {
            id: "mlx-ternary-bonsai-27b-2bit".to_string(),
            name: "Ternary Bonsai 27B".to_string(),
            category: v1::ModelCategory::Language as i32,
            framework: v1::InferenceFramework::Mlx as i32,
            download_size_bytes: 8_480_000_000,
            ..Default::default()
        }];
        let downloaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (order, groups) = group_models(&models, &downloaded, true);
        let row = &groups[&order[0]];
        assert_eq!(order, vec!["ternary-bonsai-27b".to_string()]);
        assert_eq!(row.id, "ternary-bonsai-27b");
        assert_eq!(row.size_bytes, 8_480_000_000);
    }

    #[test]
    fn mlx_only_id_stays_when_the_bare_name_is_another_model() {
        // `llama3.2` is the GGUF Llama 3.2 3B alias. Stripping the prefix
        // would list the MLX 1B under an id that pulls the other model.
        let models = vec![v1::ModelInfo {
            id: "mlx-llama-3.2-1b-instruct-4bit".to_string(),
            name: "Llama 3.2 1B Instruct".to_string(),
            category: v1::ModelCategory::Language as i32,
            framework: v1::InferenceFramework::Mlx as i32,
            download_size_bytes: 712_575_975,
            ..Default::default()
        }];
        let downloaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (order, groups) = group_models(&models, &downloaded, true);
        let row = &groups[&order[0]];
        assert_eq!(row.id, "mlx-llama3.2");
        assert_eq!(
            crate::catalog::merge_key_for("mlx-llama-3.2-1b-instruct-4bit"),
            row.id
        );
    }

    #[test]
    fn unknown_size_of_the_pulled_build_is_not_filled_from_another() {
        let mut gguf = llamacpp_variant("");
        gguf.download_size_bytes = 0;
        let models = vec![gguf, mlx_variant("")];
        let downloaded: std::collections::HashSet<String> = std::collections::HashSet::new();
        let (_, groups) = group_models(&models, &downloaded, true);
        assert_eq!(groups["qwen3-4b-instruct-2507"].size_bytes, 0);
    }

    #[test]
    fn list_scope_maps_one_flag_to_one_scope() {
        assert_eq!(list_scope(false, false, false), Ok(Scope::Downloaded));
        assert_eq!(list_scope(true, false, false), Ok(Scope::All));
        assert_eq!(list_scope(false, true, false), Ok(Scope::Local));
        assert_eq!(list_scope(false, false, true), Ok(Scope::Cloud));
    }

    #[test]
    fn list_scope_refuses_two_flags_at_once() {
        assert!(list_scope(true, true, false).is_err());
        assert!(list_scope(false, true, true).is_err());
        assert!(list_scope(true, false, true).is_err());
    }

    #[test]
    fn cloud_tag_is_plain_when_color_is_off() {
        assert_eq!(cloud_tag(true), "[cloud]");
    }
}
