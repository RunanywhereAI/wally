//! Port of src/commands/cmd_about.cpp. Owner: the maintenance/diagnostics port.
//!
//! `wally about` — a styled, richer environment panel: product, system,
//! runtime backends, paths, and the signed-in account. See `wally info` for
//! the terse sibling this expands on.

use crate::account::credentials;
use crate::bootstrap::bootstrap;
use crate::cli::App;
use crate::cli_formatter::{cli_color, color_output_enabled};
use crate::config::cli_paths;
use crate::device_info::collect_device_snapshot;
use crate::io::output as out;
use crate::sys;

use super::cmd_backends::collect_llm_backend_rows;

#[cfg(target_os = "macos")]
const PLATFORM: &str = "macos";
#[cfg(target_os = "linux")]
const PLATFORM: &str = "linux";
#[cfg(windows)]
const PLATFORM: &str = "windows";
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
const PLATFORM: &str = "unknown";

const LABEL_WIDTH: usize = 10;

/// "  label    value", the label column padded to LABEL_WIDTH. Built with plain
/// string ops rather than a fixed snprintf buffer -- a path or an account email
/// has no natural length cap, so nothing here should risk silently truncating
/// one. The label itself is left uncolored, same restraint --help's
/// descriptions get.
fn row(label: &str, value: &str) {
    let mut line = format!("  {label}");
    let pad = if line.len() < 2 + LABEL_WIDTH {
        2 + LABEL_WIDTH - line.len()
    } else {
        1
    };
    line.push_str(&" ".repeat(pad));
    line.push_str(value);
    out::result_line(&line);
}

fn heading(pal: &cli_color::Palette, title: &str) {
    out::result_line("");
    out::result_line(&format!("{}{title}{}:", pal.bold, pal.reset));
}

pub fn register_about(app: &mut App) {
    let cmd = app.add_subcommand("about", "Versions, backends, paths and account");
    cmd.callback(|_parsed, options| {
        let env = match bootstrap(options) {
            Ok(env) => env,
            Err(_) => return 1,
        };

        // SAFETY: rac_get_version returns a plain-old-data struct by value; its
        // `string` pointer is either null or a static string owned by commons.
        let commons = unsafe { sys::rac_get_version() };
        let commons_version = if commons.string.is_null() {
            "unknown".to_string()
        } else {
            // SAFETY: checked non-null above; commons guarantees a NUL-terminated,
            // process-lifetime string for this field.
            unsafe { std::ffi::CStr::from_ptr(commons.string) }
                .to_string_lossy()
                .into_owned()
        };

        let device = collect_device_snapshot();

        let mut memory = sys::rac_memory_info_t {
            total_bytes: 0,
            available_bytes: 0,
            used_bytes: 0,
        };
        let mut memory_ok = false;
        // SAFETY: rac_get_platform_adapter returns either null or a pointer the SDK
        // keeps valid for the process lifetime. get_memory_info is only called when
        // Some, and out_info/user_data are a live local and the adapter's own context.
        unsafe {
            if let Some(adapter) = sys::rac_get_platform_adapter().as_ref() {
                if let Some(get_memory_info) = adapter.get_memory_info {
                    memory_ok = get_memory_info(&mut memory, adapter.user_data) == sys::SUCCESS;
                }
            }
        }

        let engines = collect_llm_backend_rows();

        // Which bottle this binary is: WALLY_BAKED_CONSOLE_API_URL is compiled
        // in empty for a production build and non-empty for a dev one (see
        // baked_endpoints.h.in) -- the same compile-time fact
        // default_console_url() itself keys on. console_url is the *effective*
        // target though, since an env override still wins over the bake.
        let dev_channel = !env!("WALLY_BAKED_CONSOLE_API_URL").is_empty();
        let channel = if dev_channel {
            "development"
        } else {
            "production"
        };
        let console_url = credentials::default_console_url();

        let loaded = credentials::load().ok();
        let signed_in = loaded.as_ref().is_some_and(|c| c.signed_in());

        if options.json {
            let mut json = out::JsonWriter::new();
            json.begin_object()
                .field_str("wally", env!("WALLY_VERSION"))
                .field_str("commons", &commons_version)
                .field_str("idl_version", env!("WALLY_IDL_VERSION"))
                .field_str("idl_schema_sha256", env!("WALLY_IDL_SCHEMA_SHA256"))
                .field_str("platform", PLATFORM)
                .field_str("channel", channel)
                .field_str("console", &console_url)
                .field_str("os", &device.os_version)
                .field_str("cpu", &device.chip)
                .field_i64("core_count", device.core_count as i64)
                .field_i64("performance_cores", device.performance_cores as i64)
                .field_i64("efficiency_cores", device.efficiency_cores as i64)
                .field_str("architecture", &device.architecture);
            if memory_ok {
                json.field_i64("memory_total_bytes", memory.total_bytes as i64)
                    .field_i64("memory_available_bytes", memory.available_bytes as i64);
            }

            json.begin_array("backends");
            for (name, engine) in &engines {
                json.begin_array_object()
                    .field_str("name", name)
                    .field_str("display_name", &engine.display_name)
                    .field_str("version", &engine.version)
                    .field_i64("priority", engine.priority as i64);
                json.begin_array("primitives");
                for primitive in &engine.primitives {
                    json.value_str(primitive);
                }
                json.end_array().end_object();
            }
            json.end_array();

            json.field_str("home", &env.home)
                .field_str("models_dir", &env.models_dir)
                .field_str("state_dir", &cli_paths::state_dir())
                .field_bool("account_signed_in", signed_in);
            if signed_in {
                if let Some(c) = &loaded {
                    json.field_str("account_email", &c.email)
                        .field_str("account_console", &c.console_url);
                }
            }
            json.end_object();
            out::result_line(json.str());
            return 0;
        }

        let pal = cli_color::make_palette(color_output_enabled(options.no_color));

        heading(&pal, "Product");
        row("wally", env!("WALLY_VERSION"));
        row("commons", &commons_version);
        row(
            "idl",
            &format!(
                "{} sha256 {}",
                env!("WALLY_IDL_VERSION"),
                env!("WALLY_IDL_SCHEMA_SHA256")
            ),
        );
        row("platform", PLATFORM);
        row("channel", channel);
        // The console URL is deliberately NOT printed here. It is an internal
        // endpoint (today `/api-dev`), it means nothing to the person reading
        // `wally about`, and it was shown twice. It stays in `--json` so
        // support and tooling can still read it.

        heading(&pal, "System");
        row(
            "os",
            if device.os_version.is_empty() {
                "unknown"
            } else {
                &device.os_version
            },
        );
        row(
            "cpu",
            &format!(
                "{} ({} cores)",
                if device.chip.is_empty() {
                    "unknown"
                } else {
                    &device.chip
                },
                device.core_count
            ),
        );
        row(
            "arch",
            if device.architecture.is_empty() {
                "unknown"
            } else {
                &device.architecture
            },
        );
        if memory_ok {
            row(
                "memory",
                &format!(
                    "{} available of {}",
                    out::human_bytes(memory.available_bytes),
                    out::human_bytes(memory.total_bytes)
                ),
            );
        }

        heading(&pal, "Runtime");
        if engines.is_empty() {
            row("backends", "none registered");
        } else {
            for (name, engine) in &engines {
                let mut primitives = String::new();
                for primitive in &engine.primitives {
                    if primitives.is_empty() {
                        primitives.push_str(primitive);
                    } else {
                        primitives.push_str(", ");
                        primitives.push_str(primitive);
                    }
                }
                let label = format!("{}{name}{}", pal.bold_cyan, pal.reset);
                // Padding is computed from the plain (uncolored) name length,
                // same reasoning as CliFormatter -- see cli_formatter.rs.
                let pad = if name.len() < LABEL_WIDTH {
                    LABEL_WIDTH - name.len()
                } else {
                    1
                };
                out::result_line(&format!(
                    "  {label}{}priority {}  {primitives}",
                    " ".repeat(pad),
                    engine.priority
                ));
            }
        }

        heading(&pal, "Paths");
        row("home", &env.home);
        row("models", &env.models_dir);
        row("state", &cli_paths::state_dir());

        if signed_in {
            heading(&pal, "Account");
            if let Some(c) = &loaded {
                row("email", &c.email);
            }
        }
        out::result_line("");
        0
    });
}
