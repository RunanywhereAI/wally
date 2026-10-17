//! Shared ensure-downloaded + resolve-paths step for speech commands. Port of
//! src/commands/model_setup.cpp.
//!
//! Resolves a model ref, pulls it when missing (same flow as `wally models
//! pull`), and resolves the on-disk artifact paths through commons'
//! rac_model_lifecycle_resolve_paths_proto -- no engine load, no path
//! guessing in the CLI.

use std::ffi::CString;

use crate::bootstrap::GlobalOptions;
use crate::catalog::{self, model_ref};
use crate::harness;
use crate::io::output::{error_line, status_line};
use crate::io::proto::{self, parse_proto_buffer, v1, ProtoBuffer};
use crate::sys;
use crate::util::term;

use super::cmd_pull::pull_model_flow;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedModelPaths {
    pub model_id: String,
    pub display_name: String,
    /// resolved artifact (file or inner directory)
    pub primary_path: String,
}

fn resolve_paths(model_id: &str) -> Result<ResolvedModelPaths, String> {
    let request = v1::ModelLoadRequest {
        model_id: model_id.to_string(),
        ..Default::default()
    };
    let bytes = proto::serialize(&request);

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc = unsafe {
        sys::rac_model_lifecycle_resolve_paths_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    let result: v1::ModelLoadResult = match parse_proto_buffer(out_buffer) {
        Ok(result) if rc == sys::SUCCESS => result,
        Ok(_) => return Err(String::new()),
        Err(message) => return Err(message),
    };
    if let Some(err) = &result.error {
        return Err(err.message.clone());
    }
    Ok(ResolvedModelPaths {
        model_id: model_id.to_string(),
        display_name: String::new(),
        primary_path: result.resolved_path,
    })
}

/// Refresh the registry (rescan_local + downloaded-state reconciliation) so
/// on-disk artifacts -- including ones placed by the test rig or playground
/// -- are linked to their entries. Used by list and by ensure_model_ready.
pub fn refresh_registry() -> Result<(), String> {
    let request = v1::ModelRegistryRefreshRequest {
        rescan_local: true,
        include_downloaded_state: true,
        ..Default::default()
    };
    let bytes = proto::serialize(&request);

    let mut out_buffer = ProtoBuffer::new();
    // SAFETY: bytes/len describe a valid buffer for the call; out_buffer is
    // live/initialized.
    let rc = unsafe {
        sys::rac_model_registry_refresh_proto(
            sys::rac_get_model_registry(),
            bytes.as_ptr(),
            bytes.len(),
            out_buffer.as_mut_ptr(),
        )
    };
    if rc != sys::SUCCESS {
        return Err("registry refresh call failed".to_string());
    }
    parse_proto_buffer::<v1::ModelRegistryRefreshResult>(out_buffer).map(|_| ())
}

/// The registry's record of `model_id`, if it has one.
fn registry_info(model_id: &str) -> Option<v1::ModelInfo> {
    let model_id_c = CString::new(model_id).ok()?;
    let mut info_out = ProtoBuffer::new();
    // SAFETY: model_id_c is a valid, NUL-terminated C string kept alive for
    // the duration of this call; info_out is live/initialized.
    let get_rc = unsafe {
        sys::rac_model_registry_get_proto_buffer(
            sys::rac_get_model_registry(),
            model_id_c.as_ptr(),
            info_out.as_mut_ptr(),
        )
    };
    // Parsed unconditionally: it interprets the {status,error_message}
    // envelope and frees the buffer on every path (no leak on get failure).
    let info = parse_proto_buffer::<v1::ModelInfo>(info_out).ok()?;
    (get_rc == sys::SUCCESS).then_some(info)
}

/// Commons reconciliation (the refresh before any lookup) already validated
/// folder completeness; registry_status is the single authority.
fn is_downloaded(info: &v1::ModelInfo) -> bool {
    info.registry_status
        .and_then(|status| v1::ModelRegistryStatus::try_from(status).ok())
        == Some(v1::ModelRegistryStatus::Downloaded)
}

/// Another build of the same `models list` row that is already on disk, used
/// when the one `reference` names is not. `models list` shows a merged row as
/// downloaded when any of its builds is, so `-m <row id>` must be able to run
/// that build. Limited to decision models and offline callers, which would
/// otherwise refuse outright; everything else keeps pulling the build it named.
fn downloaded_sibling(
    reference: &str,
    model_id: &str,
    offline: bool,
) -> Option<(String, v1::ModelInfo)> {
    sibling_candidates(reference, model_id, offline)
        .into_iter()
        .find_map(|id| {
            let info = registry_info(id)?;
            is_downloaded(&info).then(|| (id.to_string(), info))
        })
}

/// The other builds of `model_id`'s merged row that may stand in for it.
fn sibling_candidates(reference: &str, model_id: &str, offline: bool) -> Vec<&'static str> {
    let named = catalog::find(reference).or_else(|| catalog::find(model_id));
    if !offline && named.map(|entry| entry.category) != Some(v1::ModelCategory::Decision) {
        return Vec::new();
    }
    let key = catalog::merge_key_for(model_id);
    catalog::all()
        .iter()
        .filter(|entry| entry.id != model_id && entry.merge_key.unwrap_or(entry.id) == key)
        .map(|entry| entry.id)
        .collect()
}

/// Resolve ref -> ensure downloaded (auto-pull with progress) -> resolve
/// paths. Err is the exit code the command should return (0 is never an Err
/// variant; errors are already printed to stderr before returning).
pub fn ensure_model_ready(
    options: &GlobalOptions,
    reference: &str,
) -> Result<ResolvedModelPaths, i32> {
    let resolved = match model_ref::resolve(reference, None) {
        Ok(resolved) => resolved,
        Err((_, message)) => {
            error_line(&message);
            return Err(1);
        }
    };
    // Link any on-disk artifacts before deciding whether to pull.
    if let Err(error) = refresh_registry() {
        status_line(&format!("warning: registry refresh failed: {error}"));
    }

    let mut model_id = resolved.model_id.clone();
    let mut registered = registry_info(&model_id);
    let mut downloaded = registered.as_ref().is_some_and(is_downloaded);
    if !downloaded {
        if let Some((sibling, info)) = downloaded_sibling(reference, &model_id, options.offline) {
            status_line(&format!("using the downloaded {sibling} for {reference}"));
            model_id = sibling;
            registered = Some(info);
            downloaded = true;
        }
    }
    let mut out = ResolvedModelPaths {
        model_id: model_id.clone(),
        display_name: registered.map(|info| info.name).unwrap_or_default(),
        ..Default::default()
    };
    let resolved = model_ref::Resolved {
        model_id,
        ..resolved
    };

    if !downloaded {
        // Offline callers (`wally decisions --local`) promise no network
        // during scoring, but an explicit pull is the caller's own download:
        // offer it on a terminal exactly like the harness commands do, then
        // run it with the network back on. A "no" (or no terminal, where no
        // answer can come) keeps the old refusal naming the pull command.
        if options.offline {
            if term::stdin_is_tty()
                && harness::confirm(&format!(
                    "model {} is not downloaded. Pull it now? [y/N] ",
                    resolved.model_id
                ))
            {
                let online = GlobalOptions {
                    offline: false,
                    ..options.clone()
                };
                status_line(&format!("pulling model {} now", resolved.model_id));
                let pull_code = pull_model_flow(&online, &resolved.model_id);
                if pull_code != 0 {
                    return Err(pull_code);
                }
            } else {
                error_line(&format!(
                    "model {} is not downloaded, and this command runs offline; \
                     pull it first with `wally models pull {}`",
                    resolved.model_id, resolved.model_id
                ));
                return Err(1);
            }
        } else {
            status_line(&format!(
                "model {} not downloaded — pulling",
                resolved.model_id
            ));
            let pull_code = pull_model_flow(options, &resolved.model_id);
            if pull_code != 0 {
                return Err(pull_code);
            }
        }
    }

    match resolve_paths(&resolved.model_id) {
        Ok(paths) => {
            out.primary_path = paths.primary_path;
            Ok(out)
        }
        Err(error) => {
            error_line(&format!(
                "cannot resolve model files for {}: {error}",
                resolved.model_id
            ));
            Err(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sibling_candidates;

    #[test]
    fn a_decision_row_offers_its_other_build() {
        assert_eq!(
            sibling_candidates("clef-flash-9b", "clef-flash-9b", false),
            ["mlx-clef-flash-9b-4bit"]
        );
    }

    #[test]
    fn a_chat_row_keeps_the_build_it_named_when_online() {
        assert!(
            sibling_candidates("qwen3-4b-instruct-2507", "qwen3-4b-instruct-2507", false)
                .is_empty()
        );
    }

    #[test]
    fn an_offline_caller_may_use_any_build_of_the_row() {
        assert_eq!(
            sibling_candidates("qwen3-4b-instruct-2507", "qwen3-4b-instruct-2507", true),
            ["mlx-qwen3-4b-instruct-2507-4bit"]
        );
    }

    #[test]
    fn a_model_outside_any_row_has_no_siblings() {
        assert!(sibling_candidates("silero-vad", "silero-vad", true).is_empty());
    }
}
