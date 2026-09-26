//! Port of src/commands/cmd_version.cpp.
//!
//! `wally version` — CLI + commons versions. No bootstrap needed.

use crate::cli::App;
use crate::io::output as out;
use crate::sys;

pub fn register_version(app: &mut App) {
    let cmd = app.add_subcommand("version", "Show wally and SDK versions");
    cmd.callback(|_parsed, options| {
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
        let pinned_sdk = env!("WALLY_PINNED_SDK_VERSION");
        let pin_ok = pinned_sdk.is_empty() || commons_version == pinned_sdk;

        if options.json {
            let mut json = out::JsonWriter::new();
            json.begin_object()
                .field_str("wally", env!("WALLY_VERSION"))
                .field_str("commons", &commons_version)
                .field_str("pinned_sdk", pinned_sdk)
                .field_str("idl_version", env!("WALLY_IDL_VERSION"))
                .field_str("idl_schema_sha256", env!("WALLY_IDL_SCHEMA_SHA256"))
                .field_str("idl_protoc", env!("WALLY_IDL_PROTOC_VERSION"))
                .field_bool("pin_ok", pin_ok)
                .end_object();
            out::result_line(json.str());
        } else {
            out::result_line(&format!(
                "wally {} (commons {commons_version})",
                env!("WALLY_VERSION")
            ));
            out::result_line(&format!(
                "idl {} sha256 {}",
                env!("WALLY_IDL_VERSION"),
                env!("WALLY_IDL_SCHEMA_SHA256")
            ));
        }
        if !pin_ok {
            out::error_line(&format!(
                "commons {commons_version} does not match pin {pinned_sdk}"
            ));
            return 1;
        }
        0
    });
}
