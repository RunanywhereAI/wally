//! Regression tests for the wally-rust-migration "vision" audit fixes
//! (/tmp/wally-rust-migration/fixes/vision.json). One file, not folded into
//! tests/test_wally_unit.rs, so the fix-vision worktree never edits a file
//! another port/fix worktree also owns.
//!
//! Covers the two externally-reachable `io::image_io::write_png` findings.
//! The rerank/rag `read_text_file` and DOCUMENT-preview findings are covered
//! by `#[cfg(test)]` unit tests inside src/commands/cmd_rerank.rs and
//! src/commands/cmd_rag.rs instead, since the functions under test are
//! private to those modules and unreachable from an integration test.

use wally::io::image_io::write_png;

/// finding 39: an engine-supplied diagnostic_rgba buffer shorter than
/// width*height*4 must be a clean `Err`, not a Rust slice-index panic. C++
/// has an out-of-bounds heap read here (UB) that this port deliberately does
/// not reproduce; see the comment above the length check in image_io.rs.
#[test]
fn write_png_undersized_buffer_is_err_not_panic() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("undersized.png");

    // width*height*4 = 4*4*4 = 64 bytes needed; supply only 32.
    let undersized = vec![0x11u8; 32];

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        write_png(path.to_str().expect("utf8 path"), &undersized, 4, 4)
    }));

    let err = match result {
        Ok(Err(message)) => message,
        Ok(Ok(())) => panic!("expected an error for an undersized diagnostic buffer"),
        Err(_) => panic!("write_png panicked on an undersized diagnostic buffer"),
    };
    assert_eq!(err, "invalid image dimensions or data");
    assert!(!path.exists(), "no file should be created on failure");
}

/// A buffer sized exactly width*height*4 (the boundary the undersized case
/// above sits just under) must still succeed — the new length check must not
/// be off-by-one.
#[test]
fn write_png_exact_size_buffer_succeeds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("exact.png");
    let exact = vec![0x22u8; 4 * 4 * 4];

    write_png(path.to_str().expect("utf8 path"), &exact, 4, 4).expect("write_png should succeed");
    assert!(path.exists());
}

/// finding 38 (open-failure branch): writing to a path whose parent
/// directory does not exist must still fail with "cannot open ... for
/// writing", the same message as before the fs::write -> File::create +
/// write_all split. The complementary short-write branch (a disk-full /
/// interrupted write after a successful open) is not exercised here: forcing
/// it hermetically requires either OS-level disk quotas or lowering
/// RLIMIT_FSIZE, and the latter raises SIGXFSZ (process-terminating by
/// default) in a shared test binary, which would risk aborting unrelated
/// tests rather than just this one.
#[test]
fn write_png_open_failure_reports_cannot_open() {
    let path = "/no/such/wally-fix-vision-directory/out.png";
    let px = vec![0x33u8; 2 * 2 * 4];

    let err = write_png(path, &px, 2, 2).expect_err("expected an open failure");
    assert_eq!(err, format!("cannot open {path} for writing"));
}
