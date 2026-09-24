//! Built-in model catalog (port of src/catalog/catalog.cpp) — the CLI's curated
//! equivalent of the example apps' ModelCatalog. Entries use the proto-generated
//! enums and register through the same single-call commons entry points the SDKs
//! use (rac_register_model_from_url_proto / rac_register_multi_file_model_proto).
//! Registration is idempotent per process. Owner: the catalog port.

use crate::io::proto::v1;
use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogFile {
    pub url: &'static str,
    pub filename: &'static str,
    pub required: bool,
    pub size_bytes: i64,
    pub checksum_sha256: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CatalogEntry {
    pub id: &'static str,
    /// short name accepted by `pull/run/...` (None = none)
    pub alias: Option<&'static str>,
    pub name: &'static str,
    pub category: v1::ModelCategory,
    pub framework: v1::InferenceFramework,
    pub format: v1::ModelFormat,
    /// single-file / archive primary (None → multi-file)
    pub url: Option<&'static str>,
    /// multi-file artifacts (VLM pairs, embeddings)
    pub files: &'static [CatalogFile],
    /// approximate, for display/planning
    pub download_size_bytes: i64,
    /// 0 = unknown/not applicable
    pub context_length: i32,
    pub supports_thinking: bool,
    /// 0 = unknown/not applicable
    pub memory_required_bytes: i64,
    /// Computer-Use-Agent profile id ("" = none)
    pub cua_profile: &'static str,
    /// Shared base for the same model across backends (llama.cpp / MLX / ANE /
    /// NPU). None → the row stands alone. `models list` groups by this.
    pub merge_key: Option<&'static str>,
}

/// All built-in entries.
pub fn all() -> &'static [CatalogEntry] {
    todo!("catalog port: all")
}

/// Exact id or alias lookup.
pub fn find(id_or_alias: &str) -> Option<&'static CatalogEntry> {
    todo!("catalog port: find ({id_or_alias})")
}

/// Closest-match candidates for error messages (substring match, ≤ max).
pub fn suggestions(input: &str, max: usize) -> Vec<String> {
    todo!("catalog port: suggestions ({input}, {max})")
}

/// The merge base for a registry id: the entry's merge_key when set, else the
/// id itself.
pub fn merge_key_for(id: &str) -> String {
    todo!("catalog port: merge_key_for ({id})")
}

/// Register every entry with the global model registry. Logs (does not fail
/// on) individual rejections so one bad entry can't take the CLI down.
pub fn register_all() -> sys::rac_result_t {
    todo!("catalog port: register_all")
}
