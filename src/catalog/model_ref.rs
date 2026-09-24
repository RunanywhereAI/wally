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

use crate::io::proto::v1;
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

/// Resolve `reference` to a registered model id. Catalog entries are assumed
/// registered (bootstrap runs catalog::register_all()); URL refs register a new
/// entry on the fly. The error is the result code plus a user-facing message
/// including did-you-mean suggestions.
pub fn resolve(
    reference: &str,
    options: Option<&ResolveOptions>,
) -> Result<Resolved, (sys::rac_result_t, String)> {
    todo!("catalog port: resolve ({reference}, {options:?})")
}
