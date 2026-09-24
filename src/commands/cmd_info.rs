//! Port of src/commands/cmd_info.cpp. Owner: the maintenance/diagnostics port.
//!
//! `wally info` — environment summary (versions, paths, memory, plugins).

use crate::bootstrap::bootstrap;
use crate::cli::App;
use crate::config::cli_paths;
use crate::io::output as out;
use crate::sys;

use super::cmd_backends::collect_llm_backend_rows;

pub fn register_info(app: &mut App) {
    let cmd = app.add_subcommand("info", "Show versions, paths, memory and backends");
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

        #[cfg(target_os = "macos")]
        let platform = "macos";
        #[cfg(target_os = "linux")]
        let platform = "linux";
        #[cfg(windows)]
        let platform = "windows";
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        let platform = "unknown";

        let backends = collect_llm_backend_rows().len() as i64;

        if options.json {
            let mut json = out::JsonWriter::new();
            json.begin_object()
                .field_str("wally", env!("WALLY_VERSION"))
                .field_str("commons", &commons_version)
                .field_str("platform", platform)
                .field_str("home", &env.home)
                .field_str("models_dir", &env.models_dir)
                .field_str("state_dir", &cli_paths::state_dir())
                .field_i64("backends", backends);
            if memory_ok {
                json.field_i64("memory_total_bytes", memory.total_bytes as i64)
                    .field_i64("memory_available_bytes", memory.available_bytes as i64);
            }
            json.end_object();
            out::result_line(json.str());
            return 0;
        }

        // One column for the labels so every value starts at the same offset.
        // The widest label is "platform"/"backends" (8); pad to 10.
        let row = |label: &str, value: &str| {
            let mut key = label.to_string();
            if key.len() < 10 {
                key.push_str(&" ".repeat(10 - key.len()));
            }
            out::result_line(&(key + value));
        };
        row("wally", env!("WALLY_VERSION"));
        row("commons", &commons_version);
        row("platform", platform);
        row("home", &env.home);
        row("models", &env.models_dir);
        row("backends", &backends.to_string());
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
        0
    });
}
