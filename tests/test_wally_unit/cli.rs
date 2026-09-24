//! test_wally_unit.cpp cases owned by the CLI port. Port each C++ case below as a
//! `#[test] fn <same name>()` in this file, then delete its line from this list.
//! Test bodies are in explore-main/tests/test_wally_unit.cpp (grep the name).

#[allow(unused_imports)]
use super::common;

/// Port of test_passthrough_argv_split (explore-main/tests/test_wally_unit.cpp).
#[test]
fn passthrough_argv_split() {
    fn v(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    struct Case {
        input: Vec<String>,
        want: Vec<String>,
    }
    let cases = [
        // A leading tool flag is separated so CLI11 forwards it.
        Case {
            input: v(&["wally", "claude-code", "--dangerously-skip-permissions"]),
            want: v(&[
                "wally",
                "claude-code",
                "--",
                "--dangerously-skip-permissions",
            ]),
        },
        // wally's own -m is consumed first; the tool flag after is separated.
        Case {
            input: v(&["wally", "claude-code", "-m", "glm-5.3-flash", "-p", "hi"]),
            want: v(&[
                "wally",
                "claude-code",
                "-m",
                "glm-5.3-flash",
                "--",
                "-p",
                "hi",
            ]),
        },
        // --model=... inline form is a wally flag too.
        Case {
            input: v(&[
                "wally",
                "opencode",
                "--cloud",
                "--model=glm-5.3-flash",
                "run",
            ]),
            want: v(&[
                "wally",
                "opencode",
                "--cloud",
                "--model=glm-5.3-flash",
                "--",
                "run",
            ]),
        },
        // An explicit -- is left exactly as the reader wrote it.
        Case {
            input: v(&["wally", "claude-code", "--", "-p", "hi"]),
            want: v(&["wally", "claude-code", "--", "-p", "hi"]),
        },
        // Only wally flags: nothing to forward, no -- added.
        Case {
            input: v(&["wally", "claude-code", "-m", "glm-5.3-flash"]),
            want: v(&["wally", "claude-code", "-m", "glm-5.3-flash"]),
        },
        // Not a passthrough command: untouched.
        Case {
            input: v(&["wally", "run", "qwen3-0.6b", "hi"]),
            want: v(&["wally", "run", "qwen3-0.6b", "hi"]),
        },
    ];

    for c in &cases {
        let got = wally::app::split_passthrough_argv(&c.input);
        assert_eq!(
            got,
            c.want,
            "for [{} ...] got: {:?}",
            c.input.get(1).map(String::as_str).unwrap_or(""),
            got
        );
    }
}
