//! test_wally_unit.cpp cases owned by the coding-tool harness port.

#[allow(unused_imports)]
use super::common;

// A tool installed to ~/.local/bin but not yet on PATH (a fresh `npm i -g`
// before a shell restart is the common case) must still count as installed,
// and EnsureInstalled must put that dir on PATH so the launch can exec it.
//
// Windows is skipped: the probe's Windows locations key off
// APPDATA/LOCALAPPDATA and _putenv_s, exercised by the Windows CI build, as
// the C++ test also skipped itself there.
#[cfg(not(windows))]
#[test]
fn ensure_installed_finds_tool_off_path() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let _lock = common::env_lock();
    let mut env = common::EnvGuard::new();

    let home = tempfile::tempdir().expect("temp home dir");
    let bin = home.path().join(".local").join("bin");
    fs::create_dir_all(&bin).expect("create ~/.local/bin");
    let tool = "wallyfaketool12345";
    let tool_path = bin.join(tool);
    fs::write(&tool_path, "#!/bin/sh\n").expect("write fake tool");
    fs::set_permissions(&tool_path, fs::Permissions::from_mode(0o700)).expect("chmod fake tool");

    // Register PATH for restoration before EnsureInstalled prepends to it,
    // without changing its current value.
    let original_path = std::env::var("PATH").unwrap_or_default();
    env.set("PATH", &original_path);
    env.set("HOME", home.path());

    let found = wally::harness::ensure_installed(tool);
    let bin_str = bin.to_string_lossy().into_owned();
    let path_updated = std::env::var("PATH")
        .map(|p| p.starts_with(&bin_str))
        .unwrap_or(false);

    assert!(
        found,
        "EnsureInstalled missed a tool sitting in ~/.local/bin off PATH"
    );
    assert!(
        path_updated,
        "the tool's directory was not prepended to PATH for the launch"
    );
}
