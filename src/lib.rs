//! wally — RunAnywhere's command-line interface.
//!
//! Thin dispatch layer: global flags + subcommands. All real work happens in
//! commons behind the rac_* C ABI (see AGENTS.md layering rule). Module layout
//! mirrors the C++ tree this was ported from, file for file
//! (`src/commands/cmd_run.cpp` → `src/commands/cmd_run.rs`).
//!
//! Exit codes: 0 success, 1 runtime/SDK error, 2 usage error.

pub mod account;
pub mod anthropic;
pub mod app;
pub mod bootstrap;
pub mod catalog;
pub mod cli;
pub mod cli_formatter;
pub mod commands;
pub mod config;
pub mod desktop;
pub mod device_info;
pub mod harness;
pub mod io;
pub mod net;
pub mod progress;
pub mod repl;
pub mod sys;
pub mod util;

use std::ffi::{c_char, c_int, CStr, OsString};

/// The product version from versions.toml (checked against Cargo.toml by build.rs).
pub const WALLY_VERSION: &str = env!("WALLY_VERSION");
/// The kit version this tree is pinned to (versions.toml `kit_version`).
pub const WALLY_PINNED_SDK_VERSION: &str = env!("WALLY_PINNED_SDK_VERSION");
/// The model a harness launch falls back to (CMake `WALLY_DEFAULT_MODEL_ID`).
pub const WALLY_DEFAULT_MODEL_ID: &str = env!("WALLY_DEFAULT_MODEL_ID");

/// Called from Swift, before MLX.register() — measured, not inferred: the Swift
/// host logs 3 more INFO lines during that call (Swift callbacks registered, MLX
/// backend registered, RunAnywhereMLX backend registered successfully), all
/// before wally_run_main ever runs, so muting only inside wally_run_main left 5
/// RAC lines on `wally --version` instead of 2. Splitting the mute into its own
/// entry point, called from WallyMLX.swift ahead of MLX.register(), is what
/// actually gets there.
#[no_mangle]
pub extern "C" fn wally_quiet_sdk_logging() {
    // SAFETY: plain scalar setter on the SDK logger; no pointers involved.
    unsafe { sys::rac_logger_set_min_level(sys::RAC_LOG_ERROR) };
}

/// The one entry point both binaries use: `main()` in src/main.rs, and the Swift
/// MLX host that ships as `wally` on Apple. Anything that must happen before a
/// command runs belongs behind this, not in `main()`, which the product binary
/// on Apple skips.
///
/// # Safety
/// `argv` must point to `argc` valid NUL-terminated strings (the C `main`
/// contract); they are copied before anything else runs.
#[no_mangle]
pub unsafe extern "C" fn wally_run_main(argc: c_int, argv: *mut *mut c_char) -> c_int {
    let mut args = Vec::with_capacity(argc.max(0) as usize);
    for i in 0..argc.max(0) as usize {
        // SAFETY: guaranteed by the caller (C main / Swift CommandLine).
        let arg = unsafe { *argv.add(i) };
        if arg.is_null() {
            break;
        }
        // SAFETY: non-null, NUL-terminated per the contract above.
        let bytes = unsafe { CStr::from_ptr(arg) }.to_bytes();
        args.push(os_string_from_bytes(bytes));
    }
    run_main(args)
}

/// `wally_run_main` for Rust callers (src/main.rs, in-process tests).
pub fn run_main(args: Vec<OsString>) -> c_int {
    // Covers every entry that skips the Swift host, where
    // wally_quiet_sdk_logging() is never called. Idempotent with it.
    //
    // The 2 RAC lines still on stderr on every entry point are backend
    // registration WARNs emitted during static initialisation, which completes
    // before any entry point runs. No call from inside the process can catch
    // them; silencing them needs a pre-registration hook in the kit, and the
    // kit owns backend registration.
    //
    // `--verbose` raises the level again in bootstrap().
    wally_quiet_sdk_logging();
    let args: Vec<String> = args
        .into_iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match std::panic::catch_unwind(|| app::run(&args)) {
        Ok(code) => code,
        Err(_) => {
            // A panic is a wally bug; the message is already on stderr. Still
            // release the SDK, but never let a second panic unwind into C.
            let _ = std::panic::catch_unwind(bootstrap::shutdown);
            1
        }
    }
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    // Windows CRT argv is in the active code page; wally's C++ entry treated it
    // as UTF-8 bytes, which is what this does.
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}
