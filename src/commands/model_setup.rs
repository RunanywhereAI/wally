//! Shared ensure-downloaded + resolve-paths step for speech commands. Port of
//! src/commands/model_setup.cpp. Owner: the run/llm/tool/serve port.
//!
//! Resolves a model ref, pulls it when missing (same flow as `wally models
//! pull`), and resolves the on-disk artifact paths through commons'
//! rac_model_lifecycle_resolve_paths_proto -- no engine load, no path
//! guessing in the CLI.

use std::ffi::CString;

use crate::bootstrap::GlobalOptions;
use crate::catalog::model_ref;
use crate::io::output::error_line;
use crate::io::proto::{self, parse_proto_buffer, v1, ProtoBuffer};
use crate::sys;

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
    let mut out = ResolvedModelPaths {
        model_id: resolved.model_id.clone(),
        ..Default::default()
    };

    // Link any on-disk artifacts before deciding whether to pull.
    if let Err(error) = refresh_registry() {
        crate::io::output::status_line(&format!("warning: registry refresh failed: {error}"));
    }

    // Display name (best effort) + downloaded check.
    let mut downloaded = false;
    if let Ok(model_id_c) = CString::new(resolved.model_id.as_str()) {
        let mut info_out = ProtoBuffer::new();
        // SAFETY: model_id_c is a valid, NUL-terminated C string kept alive
        // for the duration of this call; info_out is live/initialized.
        let get_rc = unsafe {
            sys::rac_model_registry_get_proto_buffer(
                sys::rac_get_model_registry(),
                model_id_c.as_ptr(),
                info_out.as_mut_ptr(),
            )
        };
        // Parsed unconditionally: it interprets the {status,error_message}
        // envelope and frees the buffer on every path (no leak on get
        // failure).
        if let Ok(info) = parse_proto_buffer::<v1::ModelInfo>(info_out) {
            if get_rc == sys::SUCCESS {
                out.display_name = info.name;
                // Commons reconciliation (refresh above) already validated
                // folder completeness; registry_status is the single
                // authority.
                downloaded = info
                    .registry_status
                    .and_then(|status| v1::ModelRegistryStatus::try_from(status).ok())
                    == Some(v1::ModelRegistryStatus::Downloaded);
            }
        }
    }

    if !downloaded {
        crate::io::output::status_line(&format!(
            "model {} not downloaded — pulling",
            resolved.model_id
        ));
        let pull_code = pull_model_flow(options, &resolved.model_id);
        if pull_code != 0 {
            return Err(pull_code);
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
