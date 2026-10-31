//! The Chrome the agent drives. Dedicated: Wally's own profile on
//! 127.0.0.1:9334, started when nothing is listening and reused when the
//! listener is that Chrome; logins persist in the profile between runs.
//! Attach: a Chrome the person already runs with remote debugging, on loopback.

use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const DEDICATED_PORT: u16 = 9334;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeMode {
    Dedicated,
    /// A Chrome started with `--remote-debugging-port`, at this origin.
    Attach(String),
    /// The person's everyday Chrome, with remote debugging turned on at
    /// chrome://inspect/#remote-debugging. Since Chrome 136 the port flag is
    /// ignored on the default profile; this toggle is the supported route.
    /// It serves a WebSocket only (no /json/version), named in the profile's
    /// DevToolsActivePort file, and Chrome asks the person to allow each
    /// connection.
    Personal,
}

/// Chrome's default profile folder, where DevToolsActivePort appears.
pub fn personal_profile_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = crate::util::getenv("HOME")?;
        Some(PathBuf::from(home).join("Library/Application Support/Google/Chrome"))
    } else if cfg!(windows) {
        let local = crate::util::getenv("LOCALAPPDATA")?;
        Some(
            PathBuf::from(local)
                .join("Google")
                .join("Chrome")
                .join("User Data"),
        )
    } else {
        let home = crate::util::getenv("HOME")?;
        Some(PathBuf::from(home).join(".config/google-chrome"))
    }
}

/// The browser WebSocket named by a DevToolsActivePort file: the port on the
/// first line, the browser path on the second.
pub fn socket_from_active_port(contents: &str) -> Option<String> {
    let mut lines = contents.lines();
    let port: u16 = lines.next()?.trim().parse().ok()?;
    let path = lines.next()?.trim();
    path.starts_with("/devtools/browser/")
        .then(|| format!("ws://127.0.0.1:{port}{path}"))
}

/// `--cdp-url` accepted only on loopback: DevTools has no authentication, and
/// this is where the person's logged-in browser answers.
pub fn loopback_devtools_url(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("--cdp-url must be http://127.0.0.1:<port>, not {url}"))?;
    let authority = rest.split('/').next().unwrap_or("");
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => {
            return Err(format!(
                "--cdp-url needs a port, as in http://127.0.0.1:9222, not {url}"
            ))
        }
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return Err(format!(
            "--cdp-url must be on this machine (127.0.0.1, localhost or [::1]), not {host}"
        ));
    }
    Ok(format!("http://{authority}"))
}

pub fn profile_dir() -> PathBuf {
    let home = crate::util::getenv("HOME")
        .or_else(|| crate::util::getenv("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home)
        .join(".cache")
        .join("wally-browser")
        .join("chrome")
}

fn chrome_binary() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
    }
    if cfg!(windows) {
        for root in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(dir) = crate::util::getenv(root) {
                candidates.push(PathBuf::from(dir).join("Google/Chrome/Application/chrome.exe"));
            }
        }
    }
    if let Some(found) = candidates.into_iter().find(|path| path.is_file()) {
        return Some(found);
    }
    let names: &[&str] = if cfg!(windows) {
        &["chrome.exe"]
    } else {
        &["google-chrome", "google-chrome-stable", "chromium"]
    };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn listening(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
}

/// GET `{origin}/json/version`.
fn version(origin: &str) -> Option<Value> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let text = agent
        .get(&format!("{origin}/json/version"))
        .call()
        .ok()?
        .into_body()
        .read_to_string()
        .ok()?;
    serde_json::from_str(&text).ok()
}

/// The browser WebSocket for this run's Chrome.
pub fn devtools_socket(mode: &ChromeMode) -> Result<String, String> {
    match mode {
        ChromeMode::Attach(origin) => {
            // A Chrome that has only just started writes its port a moment
            // before it answers on it.
            let deadline = Instant::now() + Duration::from_secs(10);
            let reply = loop {
                if let Some(reply) = version(origin) {
                    break reply;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "no Chrome answered at {origin}; start it with --remote-debugging-port"
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            reply
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("{origin} answered, but not as Chrome DevTools"))
        }
        ChromeMode::Dedicated => dedicated(),
        ChromeMode::Personal => {
            let file = personal_profile_dir()
                .ok_or("could not find Chrome's profile folder")?
                .join("DevToolsActivePort");
            let turn_on = "turn on remote debugging at chrome://inspect/#remote-debugging in your \
                           Chrome, then run this again";
            let contents = std::fs::read_to_string(&file).map_err(|_| turn_on.to_string())?;
            socket_from_active_port(&contents)
                .ok_or_else(|| format!("{} is not a DevTools port file; {turn_on}", file.display()))
        }
    }
}

fn dedicated() -> Result<String, String> {
    let port = DEDICATED_PORT;
    if !listening(port) {
        let binary = chrome_binary().ok_or("Chrome is not installed.")?;
        let profile = profile_dir();
        std::fs::create_dir_all(&profile)
            .map_err(|error| format!("could not create {}: {error}", profile.display()))?;
        let mut command = Command::new(binary);
        command
            .arg(format!("--remote-debugging-port={port}"))
            .arg("--remote-debugging-address=127.0.0.1")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Its own process group, so Ctrl-C on wally leaves the browser and
        // its tabs open for the person.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }
        command
            .spawn()
            .map_err(|error| format!("could not start Chrome: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(20);
        while !listening(port) {
            if Instant::now() > deadline {
                return Err(format!("Chrome did not open port {port}."));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let origin = format!("http://127.0.0.1:{port}");
    let ours = |reply: &Value| {
        let browser = reply.get("Browser").and_then(Value::as_str).unwrap_or("");
        let socket = reply
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .unwrap_or("");
        browser.contains("Chrome") && socket.starts_with(&format!("ws://127.0.0.1:{port}"))
    };
    // A just-started Chrome opens the port a moment before it answers.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match version(&origin) {
            Some(reply) if ours(&reply) => {
                return Ok(reply["webSocketDebuggerUrl"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string())
            }
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                return Err(format!(
                    "Port {port} is open, but it is not this command's Chrome."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_devtools_url_is_refused() {
        assert!(loopback_devtools_url("http://example.com:9222").is_err());
        assert!(loopback_devtools_url("http://10.0.0.5:9222").is_err());
    }

    #[test]
    fn a_loopback_devtools_url_is_accepted() {
        assert_eq!(
            loopback_devtools_url("http://127.0.0.1:9222").unwrap(),
            "http://127.0.0.1:9222"
        );
        assert_eq!(
            loopback_devtools_url("http://localhost:9222/").unwrap(),
            "http://localhost:9222"
        );
    }

    #[test]
    fn a_devtools_url_without_a_port_is_refused() {
        assert!(loopback_devtools_url("http://127.0.0.1").is_err());
        assert!(loopback_devtools_url("ws://127.0.0.1:9222").is_err());
    }

    #[test]
    fn the_dedicated_profile_lives_under_the_cache() {
        assert!(profile_dir().ends_with(".cache/wally-browser/chrome"));
    }

    #[test]
    fn an_active_port_file_names_the_browser_socket() {
        let socket = socket_from_active_port("9222\n/devtools/browser/abc-123\n");
        assert_eq!(
            socket.as_deref(),
            Some("ws://127.0.0.1:9222/devtools/browser/abc-123")
        );
    }

    #[test]
    fn a_malformed_active_port_file_names_nothing() {
        assert_eq!(
            socket_from_active_port("not a port\n/devtools/browser/x"),
            None
        );
        assert_eq!(socket_from_active_port("9222\n/etc/passwd"), None);
        assert_eq!(socket_from_active_port("9222"), None);
    }
}
