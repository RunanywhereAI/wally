//! Port of src/commands/cmd_update.cpp.
//!
//! `wally update` — re-run the installer, telling it our version.
//!
//! The installer already knows how to fetch the latest release; the only thing
//! it cannot know on its own is what the caller is running. So update hands it
//! this build's version, and the script decides: pull the newer release, or
//! report that this machine is already current and download nothing.
//!
//! The same script answers a check with the latest release version alone:
//! `install.sh --check`, or `install.ps1` under `WALLY_INSTALL_CHECK=1` (it is
//! run as `irm | iex`, which passes no arguments). The one-line
//! update-available notice reads that, so the version lookup lives in one
//! place.

use crate::cli::App;
use crate::io::output as out;
use crate::util::getenv;

// The same script the install line in the README pipes to a shell. Kept as one
// constant so the update path and the documented install path cannot drift.
#[cfg(not(windows))]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.sh";
#[cfg(windows)]
const INSTALL_URL: &str = "https://raw.githubusercontent.com/RunanywhereAI/wally/main/install.ps1";

// Points update and the update check at another copy of the installer, for
// release tests and mirrors, the way the installer's own WALLY_INSTALL_BASE_URL
// points it at another copy of the release.
const SCRIPT_URL_ENV: &str = "WALLY_UPDATE_SCRIPT_URL";

// How long a version lookup is trusted before the notice asks again.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

// Bounds the lookup so a dead network cannot hold a version check, or
// leave a background check running, for long.
const CHECK_TIMEOUT_SECS: &str = "10";

// Homebrew's own prefixes, plus whatever `$HOMEBREW_PREFIX` names (set by
// `brew shellenv` and by every formula's build environment). A binary
// resolved (symlinks followed) into one of these came from `brew install`,
// not install.sh -- piping install.sh at it would lay a second copy under
// ~/.local that `brew upgrade`/`brew uninstall` never sees again.
#[cfg(not(windows))]
const HOMEBREW_PREFIXES: [&str; 3] = [
    "/opt/homebrew",
    "/usr/local/Cellar",
    "/home/linuxbrew/.linuxbrew",
];

#[cfg(not(windows))]
pub(crate) fn is_homebrew_managed(exe: &str) -> bool {
    if exe.is_empty() {
        return false;
    }
    // Whole path components, so /opt/homebrew-old is not /opt/homebrew.
    let exe = std::path::Path::new(exe);
    if let Some(prefix) = getenv("HOMEBREW_PREFIX") {
        if !prefix.is_empty() && exe.starts_with(&prefix) {
            return true;
        }
    }
    HOMEBREW_PREFIXES
        .iter()
        .any(|prefix| exe.starts_with(prefix))
}

pub fn register_update(app: &mut App) {
    let cmd = app.add_subcommand("update", "Update wally to the latest release");
    cmd.callback(|_parsed, _options| {
        let code = run_update();
        if code != 0 {
            return 1;
        }
        0
    });
}

/// Shared by `wally update` and the whole-argv `-u/--update` shortcut.
#[cfg(not(windows))]
pub fn run_update() -> i32 {
    let exe = crate::commands::cmd_maintenance::self_executable();
    if is_homebrew_managed(&exe) {
        // Not a failure: the command's job -- get the person to the newest
        // release -- is done by pointing them at the manager that actually
        // owns this binary. install.sh would "succeed" too, but by writing a
        // second, unmanaged copy under ~/.local that outlives every future
        // `brew upgrade`, which is a worse outcome than doing nothing here.
        out::status_line("wally was installed with Homebrew; run `brew upgrade wally` to update.");
        return 0;
    }

    // The script URL and the installer's arguments reach the shell as
    // positional parameters, never spliced into the command string, so an
    // override URL carries nothing a caller could inject.
    let installer_args = [format!("--version={}", env!("WALLY_VERSION"))];

    out::status_line("checking for a newer wally...");
    // A non-zero exit means the installer already explained why on its own
    // stderr; a failure to even spawn `sh` counts the same way.
    let ok = installer_command(&installer_args, None)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        0
    } else {
        1
    }
}

/// Shared by `wally update` and the whole-argv `-u/--update` shortcut.
///
/// install.ps1 replaces the running wally.exe and its DLLs file by file
/// (Windows renames a file in use but will not move or delete it), so the
/// installer runs while this process waits, the same as on macOS and Linux.
#[cfg(windows)]
pub fn run_update() -> i32 {
    let current = env!("WALLY_VERSION");
    out::status_line("checking for a newer wally...");
    let Some(latest) = fetch_latest_version() else {
        out::error_line("could not check for a newer wally; check your connection and try again");
        return 1;
    };
    write_cached_latest(&latest);
    if !is_newer(&latest, current) {
        out::status_line(&format!("wally {current} is the latest"));
        return 0;
    }
    // The URL reaches PowerShell as an environment variable, never spliced
    // into the command, so an override URL carries nothing a caller could
    // inject. A non-zero exit means the installer already said why.
    let script = r#"$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
Invoke-Expression (Invoke-RestMethod -UseBasicParsing $env:WALLY_UPDATE_RESOLVED_URL)"#;
    let ok = powershell(script)
        .env_remove("WALLY_INSTALL_CHECK")
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        0
    } else {
        1
    }
}

/// Deletes the `<name>.old` files an update left beside wally.exe because
/// they were still in use. Best effort: one still in use stays for next time.
#[cfg(windows)]
pub fn sweep_replaced_files() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(".old") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(not(windows))]
pub fn sweep_replaced_files() {}

fn install_script_url() -> String {
    getenv(SCRIPT_URL_ENV)
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| INSTALL_URL.to_string())
}

/// `sh -c 'curl <script> | sh -s -- <args>'`, with the URL and every argument
/// passed as positional parameters. `timeout_secs` bounds the download.
#[cfg(not(windows))]
fn installer_command(args: &[String], timeout_secs: Option<&str>) -> std::process::Command {
    let script = match timeout_secs {
        Some(_) => {
            r#"url="$1"; max="$2"; shift 2; curl -fsSL --max-time "$max" "$url" | sh -s -- "$@""#
        }
        None => r#"url="$1"; shift; curl -fsSL "$url" | sh -s -- "$@""#,
    };
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(script).arg("wally-update");
    command.arg(install_script_url());
    if let Some(max) = timeout_secs {
        command.arg(max);
    }
    command.args(args);
    command
}

/// Windows PowerShell running `script`, with the installer's URL in
/// `WALLY_UPDATE_RESOLVED_URL`. powershell.exe rather than pwsh: it ships with
/// every Windows, and install.ps1's file:// mirror path needs 5.1.
#[cfg(windows)]
fn powershell(script: &str) -> std::process::Command {
    let mut command = std::process::Command::new("powershell.exe");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(script)
        .env("WALLY_UPDATE_RESOLVED_URL", install_script_url());
    command
}

// install.ps1 in check mode, printing only the latest version. Shared by the
// check and the background refresh.
#[cfg(windows)]
const CHECK_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$env:WALLY_INSTALL_CHECK = '1'
$script = Invoke-RestMethod -UseBasicParsing -TimeoutSec $env:WALLY_UPDATE_TIMEOUT $env:WALLY_UPDATE_RESOLVED_URL
$latest = (Invoke-Expression $script | Out-String).Trim()"#;

/// The latest release version from the installer's check mode, or None when
/// the lookup failed or answered with something that is not a version.
fn fetch_latest_version() -> Option<String> {
    #[cfg(not(windows))]
    let mut command = installer_command(&["--check".to_string()], Some(CHECK_TIMEOUT_SECS));
    #[cfg(windows)]
    let mut command = {
        let mut command = powershell(&format!("{CHECK_SCRIPT}\n$latest"));
        command.env("WALLY_UPDATE_TIMEOUT", CHECK_TIMEOUT_SECS);
        command
    };
    let output = command
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let latest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_version(&latest).map(|_| latest)
}

/// `X.Y.Z`, with an optional leading `v`. Anything else (a dev build's
/// `-dev` suffix included) is not comparable and yields None.
pub(crate) fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.strip_prefix('v').unwrap_or(text);
    let mut parts = text.split('.');
    let mut next = || -> Option<u64> {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let version = (next()?, next()?, next()?);
    if parts.next().is_some() {
        return None;
    }
    Some(version)
}

/// True only when both parse and `latest` is strictly ahead, so a dev build
/// or one already past the latest release never hears it is behind.
pub(crate) fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The notice, colored when `color` is set.
pub(crate) fn notice_line(latest: &str, current: &str, color: bool) -> String {
    if color {
        format!(
            "\x1b[1;33mupdate available:\x1b[0m \x1b[1m{latest}\x1b[0m (you have {current}) — run \x1b[36mwally update\x1b[0m"
        )
    } else {
        format!("update available: {latest} (you have {current}) — run `wally update`")
    }
}

fn cache_path() -> Option<std::path::PathBuf> {
    let dir = crate::config::cli_paths::state_dir();
    if dir.is_empty() {
        return None;
    }
    Some(std::path::Path::new(&dir).join("update-check"))
}

fn write_cached_latest(latest: &str) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, format!("{latest}\n"));
}

/// The cached latest version and whether it is still within CHECK_INTERVAL.
/// An empty cache (a lookup that failed) is fresh but names no version.
fn read_cache(path: &std::path::Path) -> Option<(Option<String>, bool)> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let fresh = modified
        .elapsed()
        .map(|age| age < CHECK_INTERVAL)
        .unwrap_or(true);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let latest = text.trim();
    let latest = parse_version(latest).map(|_| latest.to_string());
    Some((latest, fresh))
}

/// Starts a lookup that outlives this process and writes the cache when it
/// finishes, so no command ever waits on the network for the notice. The
/// cache is touched first: one attempt per interval, even offline.
fn refresh_cache_in_background(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let touched = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()));
    if touched.is_err() {
        return;
    }
    let _ = background_refresh_command(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(not(windows))]
fn background_refresh_command(path: &std::path::Path) -> std::process::Command {
    let script = r#"url="$1"; max="$2"; cache="$3"; tmp="$cache.$$"
curl -fsSL --max-time "$max" "$url" | sh -s -- --check > "$tmp" 2>/dev/null \
    && [ -s "$tmp" ] && mv -f "$tmp" "$cache" || rm -f "$tmp""#;
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(script)
        .arg("wally-update-check")
        .arg(install_script_url())
        .arg(CHECK_TIMEOUT_SECS)
        .arg(path);
    command
}

#[cfg(windows)]
fn background_refresh_command(path: &std::path::Path) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    // No console window, and its own process group so the Ctrl+C that ends
    // the command that started it does not end the lookup too.
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = format!(
        r#"{CHECK_SCRIPT}
if ($latest -match '^\d+\.\d+\.\d+$') {{
    $tmp = "$env:WALLY_UPDATE_CACHE.$PID"
    Set-Content -LiteralPath $tmp -Value $latest -Encoding Ascii
    Move-Item -LiteralPath $tmp -Destination $env:WALLY_UPDATE_CACHE -Force
}}"#
    );
    let mut command = powershell(&script);
    command
        .env("WALLY_UPDATE_TIMEOUT", CHECK_TIMEOUT_SECS)
        .env("WALLY_UPDATE_CACHE", path)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    command
}

/// Prints the update-available notice when the cache says a newer release is
/// out, and refreshes a stale cache in the background. Interactive terminals
/// only: never in pipes, in CI, or under `--json`/`--quiet`.
pub fn show_update_notice(json: bool, quiet: bool, no_color: bool) {
    use crate::util::term;
    if json || quiet || getenv("CI").is_some() {
        return;
    }
    if !term::stdin_is_tty() || !term::stderr_is_tty() {
        return;
    }
    let Some(path) = cache_path() else { return };
    let cached = read_cache(&path);
    let fresh = cached.as_ref().is_some_and(|(_, fresh)| *fresh);
    if !fresh {
        refresh_cache_in_background(&path);
    }
    let current = env!("WALLY_VERSION");
    if let Some((Some(latest), _)) = cached {
        if is_newer(&latest, current) {
            out::status_line(&notice_line(
                &latest,
                current,
                !no_color && term::color_enabled(),
            ));
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;
    use crate::util::env_lock::lock as env_lock;

    #[test]
    fn is_homebrew_managed_matches_the_apple_silicon_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(is_homebrew_managed(
            "/opt/homebrew/Cellar/wally/0.5.10/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_matches_the_intel_cellar_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(is_homebrew_managed(
            "/usr/local/Cellar/wally/0.5.10/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_matches_a_custom_homebrew_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body; `set_var`'s
        // only panic case is a name containing '=' or NUL, and this literal
        // has neither.
        unsafe { std::env::set_var("HOMEBREW_PREFIX", "/custom/brew") };
        let result = is_homebrew_managed("/custom/brew/Cellar/wally/0.5.10/bin/wally");
        // SAFETY: still holding env_lock().
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(result);
    }

    #[test]
    fn is_homebrew_managed_rejects_an_install_sh_path() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed(
            "/home/user/.local/lib/wally/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_rejects_a_sibling_of_a_homebrew_prefix() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed("/opt/homebrew-old/bin/wally"));
        assert!(!is_homebrew_managed(
            "/usr/local/Cellar-backup/wally/bin/wally"
        ));
    }

    #[test]
    fn is_homebrew_managed_rejects_an_empty_path() {
        let _lock = env_lock();
        // SAFETY: env_lock() is held for this whole test body.
        unsafe { std::env::remove_var("HOMEBREW_PREFIX") };
        assert!(!is_homebrew_managed(""));
    }

    #[test]
    fn parse_version_reads_plain_and_v_prefixed_releases() {
        assert_eq!(parse_version("0.7.2"), Some((0, 7, 2)));
        assert_eq!(parse_version("v1.10.0"), Some((1, 10, 0)));
    }

    #[test]
    fn parse_version_rejects_anything_but_three_numbers() {
        for text in ["", "0.7", "0.7.2.1", "0.0.0-dev", "a.b.c", "0..2", "<html>"] {
            assert_eq!(parse_version(text), None, "{text}");
        }
    }

    #[test]
    fn is_newer_compares_numerically_not_as_text() {
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("0.7.3", "0.7.2"));
    }

    #[test]
    fn is_newer_is_false_when_equal_behind_or_unparseable() {
        assert!(!is_newer("0.7.2", "0.7.2"));
        assert!(!is_newer("0.7.1", "0.7.2"));
        assert!(!is_newer("0.7.3", "0.0.0-dev"));
        assert!(!is_newer("", "0.7.2"));
    }

    #[test]
    fn notice_line_plain_names_both_versions_and_the_command() {
        assert_eq!(
            notice_line("0.7.3", "0.7.2", false),
            "update available: 0.7.3 (you have 0.7.2) — run `wally update`"
        );
    }

    #[test]
    fn notice_line_colored_carries_no_backticks() {
        let line = notice_line("0.7.3", "0.7.2", true);
        assert!(line.starts_with("\x1b[1;33mupdate available:\x1b[0m"));
        assert!(line.contains("\x1b[36mwally update\x1b[0m"));
        assert!(!line.contains('`'));
    }
}
