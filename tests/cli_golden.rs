//! Replays tests/golden (captured from the C++ build, see its README) against
//! the Rust binary: stdout, stderr and exit code must match byte for byte.
//!
//! The corpus was captured on macOS arm64, and several outputs are platform
//! specific (the MLX backend and catalog rows exist only on Apple), so the
//! replay runs on macOS; other platforms are covered by CI's smoke/e2e runs.
//! `WALLY_GOLDEN_FILTER=<substring>` runs only the matching cases.

#![cfg(target_os = "macos")]

mod common;

use std::path::Path;

use common::{run_wally, TempHome};

#[derive(serde::Deserialize)]
struct Case {
    name: String,
    args: Vec<String>,
}

/// The product version moves with every release (versions.toml, mirrored in
/// Cargo.toml), so `--version` and `version --json` name it as `<VERSION>`;
/// scripts/test/golden-capture.py writes the same placeholder.
fn normalize(text: &str, home: &Path) -> String {
    let real = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let version = env!("CARGO_PKG_VERSION");
    text.replace(real.to_str().unwrap(), "$HOME")
        .replace(home.to_str().unwrap(), "$HOME")
        .replace(&format!("wally {version} "), "wally <VERSION> ")
        .replace(&format!("wally {version}\n"), "wally <VERSION>\n")
        .replace(
            &format!("\"wally\":\"{version}\""),
            "\"wally\":\"<VERSION>\"",
        )
}

fn run_case(root: &Path, case: &Case) -> Option<String> {
    let home = TempHome::new();
    let args: Vec<String> = case
        .args
        .iter()
        .map(|a| a.replace("$HOME", home.path().to_str().unwrap()))
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_wally(&home, &args);
    let (stdout, stderr) = (
        normalize(&stdout, home.path()),
        normalize(&stderr, home.path()),
    );
    let expected = |ext: &str| {
        std::fs::read_to_string(root.join(format!("expected/{}.{ext}", case.name))).unwrap()
    };
    let (want_out, want_err, want_code) =
        (expected("stdout"), expected("stderr"), expected("code"));
    let mut problems = Vec::new();
    if format!("{code}\n") != want_code {
        problems.push(format!("exit code {code}, expected {}", want_code.trim()));
    }
    for (label, got, want) in [
        ("stdout", &stdout, &want_out),
        ("stderr", &stderr, &want_err),
    ] {
        if got != want {
            let first = got
                .lines()
                .zip(want.lines())
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| got.lines().count().min(want.lines().count()));
            problems.push(format!(
                "{label} differs at line {}:\n    got:  {:?}\n    want: {:?}",
                first + 1,
                got.lines().nth(first).unwrap_or("<end>"),
                want.lines().nth(first).unwrap_or("<end>")
            ));
        }
    }
    (!problems.is_empty())
        .then(|| format!("{} {:?}\n  {}", case.name, case.args, problems.join("\n  ")))
}

#[test]
fn golden_cli_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let cases: Vec<Case> =
        serde_json::from_str(&std::fs::read_to_string(root.join("cases.json")).unwrap()).unwrap();
    let filter = std::env::var("WALLY_GOLDEN_FILTER").unwrap_or_default();
    let cases: Vec<&Case> = cases.iter().filter(|c| c.name.contains(&filter)).collect();
    let failures: Vec<String> = std::thread::scope(|scope| {
        let chunks: Vec<_> = cases.chunks(cases.len().div_ceil(8).max(1)).collect();
        let handles: Vec<_> = chunks
            .into_iter()
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .filter_map(|c| run_case(&root, c))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    assert!(
        failures.is_empty(),
        "{} of {} golden cases differ from the C++ build:\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n\n")
    );
}
