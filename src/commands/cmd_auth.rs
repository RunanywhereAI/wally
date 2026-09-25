//! `wally auth login` -- real control-plane handshake (port of
//! src/commands/cmd_auth.cpp).
//!
//! Runs the canonical staging/production auth sequence against the configured
//! backend (--base-url/--api-key/--environment or their RUNANYWHERE_* env
//! vars): authenticate (API key -> JWT + refresh token), device registration,
//! and model-assignment fetch -- all through commons entry points
//! (net::login -> rac_auth_* + rac_sdk_init_phase2_proto).
//!
//! Not wired into `wally`'s command tree (see src/app.cpp: `auth` duplicates
//! `account login` and is commented out there) -- this file still ports the
//! command in full, as the account area owns it.

use crate::bootstrap::{bootstrap, GlobalOptions};
use crate::cli::App;
use crate::io::output as out;
use crate::net::control_plane;

/// Howard Hinnant's `civil_from_days` (public domain): the proleptic Gregorian
/// calendar date for `z` days since 1970-01-01, in UTC. Used instead of
/// gmtime_r/gmtime_s so formatting a timestamp has no platform-specific
/// calendar dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

pub(crate) fn format_epoch_seconds(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_string();
    }
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let sec = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{sec:02}Z")
}

fn run_auth_login(options: &GlobalOptions) -> i32 {
    if bootstrap(options).is_err() {
        return 1;
    }

    let summary = match control_plane::login() {
        Ok(summary) => summary,
        Err((_, error)) => {
            out::error_line(&error);
            return 1;
        }
    };

    // A staging/production login that never completes HTTP/auth setup is a
    // broken control plane -- surface it as a failure, not a footnote.
    let ok = summary.has_completed_http_setup;

    if options.json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_bool("success", ok)
            .field_str("organization_id", &summary.organization_id)
            .field_str("user_id", &summary.user_id)
            .field_str("device_id", &summary.backend_device_id)
            .field_str("device_uuid", &summary.persistent_device_id)
            .field_str(
                "token_expires_at",
                &format_epoch_seconds(summary.token_expires_at),
            )
            .field_bool("has_completed_http_setup", summary.has_completed_http_setup)
            .field_i64("assignments", summary.assignment_count as i64);
        if !summary.warning.is_empty() {
            json.field_str("warning", &summary.warning);
        }
        json.end_object();
        out::result_line(json.str());
    } else {
        out::result_line(&format!("organization   {}", summary.organization_id));
        out::result_line(&format!(
            "user           {}",
            if summary.user_id.is_empty() {
                "-"
            } else {
                &summary.user_id
            }
        ));
        out::result_line(&format!("device         {}", summary.backend_device_id));
        out::result_line(&format!("device-uuid    {}", summary.persistent_device_id));
        out::result_line(&format!(
            "token expires  {}",
            format_epoch_seconds(summary.token_expires_at)
        ));
        out::result_line(&format!(
            "http setup     {}",
            if summary.has_completed_http_setup {
                "completed"
            } else {
                "NOT completed"
            }
        ));
        out::result_line(&format!(
            "assignments    {} model(s)",
            summary.assignment_count
        ));
        if !summary.warning.is_empty() {
            out::status_line(&format!("warning: {}", summary.warning));
        }
    }

    if !ok {
        out::error_line(&format!(
            "HTTP/auth setup did not complete{}",
            if summary.warning.is_empty() {
                String::new()
            } else {
                format!(": {}", summary.warning)
            }
        ));
        return 1;
    }
    0
}

pub fn register_auth(app: &mut App) {
    let cmd = app.add_subcommand("auth", "Sign this device in to the control plane");
    cmd.require_subcommand(1, 1);

    let login_cmd = cmd.add_subcommand(
        "login",
        "Exchange the API key for a JWT, register this device and fetch model \
         assignments. Requires --environment production with --base-url and \
         --api-key (or RUNANYWHERE_* env vars). Keyless development has no login \
         path.",
    );
    login_cmd.callback(|_p, g| run_auth_login(g));
}
