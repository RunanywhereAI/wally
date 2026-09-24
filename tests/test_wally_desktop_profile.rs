//! Claude Desktop profile files: which contents count as blank, exactly as
//! claude_profile.cpp decided it (only " \t\r\n").

mod common;

use common::{env_lock, EnvGuard, TempHome};

// ---------------------------------------------------------------------------
// Blank-file detection in the profile JSON reader must only
// treat space/tab/CR/LF as blank, matching C++'s
// `find_first_not_of(" \t\r\n")`.
// ---------------------------------------------------------------------------

#[test]
fn vertical_tab_only_profile_file_is_not_treated_as_blank() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut guard = EnvGuard::new();
    guard.set("HOME", home.path());

    let meta_path = home.join("Library/Application Support/Claude-3p/configLibrary/_meta.json");
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("create configLibrary dir");
    // A lone vertical tab: not blank under C++'s " \t\r\n" definition, but
    // `str::trim()` (Unicode White_Space) used to treat it as blank.
    std::fs::write(&meta_path, "\u{0B}").expect("write meta file");

    let result = wally::desktop::restore_gateway();

    assert!(
        result.is_err(),
        "a lone vertical-tab byte is not blank per C++'s \" \\t\\r\\n\" set, so the \
         reader must hand it to the JSON parser and surface the parse failure, \
         not silently treat it as an empty file"
    );
    let message = result.unwrap_err();
    assert!(
        message.contains("could not read"),
        "expected a JSON-parse-failure message, got: {message}"
    );
}

#[test]
fn space_and_tab_only_profile_file_is_still_treated_as_blank() {
    let _lock = env_lock();
    let home = TempHome::new();
    let mut guard = EnvGuard::new();
    guard.set("HOME", home.path());

    let meta_path = home.join("Library/Application Support/Claude-3p/configLibrary/_meta.json");
    std::fs::create_dir_all(meta_path.parent().unwrap()).expect("create configLibrary dir");
    std::fs::write(&meta_path, " \t\r\n").expect("write meta file");

    let result = wally::desktop::restore_gateway();

    assert!(
        result.is_ok(),
        "space/tab/CR/LF must still be treated as blank, matching C++: {:?}",
        result.err()
    );
}
