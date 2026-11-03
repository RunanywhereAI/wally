//! Lifecycle for the PLS router: write its assets out, build the venv once,
//! spawn it on port 0, read the handshake, and tear it down without leaving an
//! orphan holding the GPU.
//!
//! The router scripts are compiled into wally, so an installed binary carries
//! them. They are written to the profile directory on start; a hand edit there
//! wins until the next launch overwrites it.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use crate::account;
use crate::io::output as out;

/// How long the router gets to print its port before the launch is abandoned.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

const SCRIPT: &str = include_str!("../../pls/pls.py");
const MODEL_SERVER: &str = include_str!("../../pls/model_server.py");
const LOCK: &str = include_str!("../../pls/requirements.lock");

/// A running router. Dropping it stops the process.
pub struct Router {
    child: Child,
    pub base_url: String,
    /// Per-launch secret; the providers send it as `x-pls-token` so another
    /// local process cannot spend the account's cloud quota.
    pub token: String,
    #[allow(dead_code)] // held for its Drop; the router reads it over the socket
    entitlement: Option<Entitlement>,
}

impl Router {
    /// Whether the router process is still alive.
    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Router {
    fn drop(&mut self) {
        // SIGTERM first: the router kills the model server in its own exit
        // path, which a SIGKILL would skip.
        #[cfg(unix)]
        {
            // SAFETY: only signalled when the pid is still unreaped by us, so
            // `kill` cannot land on a recycled pid. `try_wait` reaps it.
            if matches!(self.child.try_wait(), Ok(None)) {
                unsafe {
                    libc::kill(self.child.id() as i32, libc::SIGTERM);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        // A normal exit leaves no record, so the next launch has nothing to
        // sweep; only a crashed one leaves a pidfile behind.
        let _ = std::fs::remove_file(pid_path());
    }
}

/// A loopback entitlement server on a unix socket. The router asks it whether
/// the signed-in account may run work locally; wally answers from the last
/// console check so a turn never waits on the network.
pub struct Entitlement {
    #[allow(dead_code)]
    premium: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    socket: PathBuf,
    accept: Option<std::thread::JoinHandle<()>>,
    refresh: Option<std::thread::JoinHandle<()>>,
}

impl Entitlement {
    fn start() -> Option<Self> {
        let socket = config_dir().join("entitlement.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).ok()?;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        listener.set_nonblocking(true).ok()?;

        let premium = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));

        let accept = {
            let premium = Arc::clone(&premium);
            let stop = Arc::clone(&stop);
            let listener = listener.try_clone().ok()?;
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => answer(stream, premium.load(Ordering::SeqCst)),
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                    }
                }
            })
        };
        let refresh = {
            let premium = Arc::clone(&premium);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    premium.store(check_entitlement(), Ordering::SeqCst);
                    for _ in 0..600 {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            })
        };
        Some(Entitlement {
            premium,
            stop,
            socket,
            accept: Some(accept),
            refresh: Some(refresh),
        })
    }
}

impl Drop for Entitlement {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.refresh.take() {
            let _ = handle.join();
        }
    }
}

fn answer(mut stream: UnixStream, premium: bool) {
    let mut buffer = [0u8; 256];
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    let _ = stream.read(&mut buffer);
    let body = format!("{{\"sub\":{premium}}}");
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

/// Whether the signed-in account may run work on the local model.
///
/// A plan the console does not describe counts as premium only when it says
/// so; anything else, including a console that cannot be reached twice, is
/// false so the router falls back to the cloud.
fn check_entitlement() -> bool {
    let Ok(mut credentials) = account::load() else {
        return false;
    };
    if credentials.console_url.is_empty() || !credentials.signed_in() {
        return false;
    }
    if let Ok(identity) = account::who_am_i(&credentials.console_url, &credentials.access_token) {
        return plan_is_premium(&identity.plan);
    }
    if credentials.refresh_token.is_empty() {
        return false;
    }
    let Ok(grant) = account::refresh(&credentials.console_url, &credentials.refresh_token) else {
        return false;
    };
    credentials.access_token = grant.access_token;
    if grant.refresh_token.is_empty() {
        return false;
    }
    credentials.refresh_token = grant.refresh_token;
    plan_is_premium(&grant.plan)
}

fn plan_is_premium(plan: &str) -> bool {
    matches!(
        plan.to_ascii_lowercase().as_str(),
        "pro" | "premium" | "paid" | "beta"
    )
}

fn config_dir() -> PathBuf {
    PathBuf::from(account::profile_directory()).join("pls")
}

fn pid_path() -> PathBuf {
    config_dir().join("pls.pid")
}

/// The mode/cap file, for a status line.
pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// Write the router scripts into the profile directory, keeping the venv
/// marker honest. Returns the script path, or the reason it could not be
/// written.
fn materialise() -> Option<PathBuf> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let wanted = format!(
        "{:x}",
        fnv1a(format!("{SCRIPT}\u{0}{MODEL_SERVER}\u{0}{LOCK}").as_bytes())
    );
    let marker = dir.join("assets.sha");
    let script = dir.join("pls.py");
    if std::fs::read_to_string(&marker).unwrap_or_default().trim() == wanted && script.is_file() {
        return Some(script);
    }
    for (name, body) in [
        ("pls.py", SCRIPT),
        ("model_server.py", MODEL_SERVER),
        ("requirements.lock", LOCK),
    ] {
        std::fs::write(dir.join(name), body).ok()?;
    }
    std::fs::write(&marker, wanted).ok()?;
    Some(script)
}

fn uv_path() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("UV") {
        if !value.is_empty() {
            return Some(PathBuf::from(value));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("uv");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("uv"))
        .find(|candidate| candidate.is_file())
}

fn venv_python() -> PathBuf {
    config_dir().join("venv").join("bin").join("python")
}

/// Build the venv when it is missing or the lockfile changed. Returns the
/// interpreter to run `pls.py` with.
fn ensure_venv(script: &Path) -> Option<PathBuf> {
    let python = venv_python();
    let lock = script.with_file_name("requirements.lock");
    if !lock.is_file() {
        out::error_line(&format!("missing {}", lock.display()));
        return None;
    }
    let marker = config_dir().join("venv.lock.sha");
    let wanted = std::fs::read(&lock)
        .map(|bytes| format!("{:x}", fnv1a(&bytes)))
        .unwrap_or_default();
    let current = std::fs::read_to_string(&marker).unwrap_or_default();
    if python.is_file() && current.trim() == wanted {
        return Some(python);
    }

    let uv = uv_path()?;
    std::fs::create_dir_all(config_dir()).ok()?;
    let mut bar = crate::progress::progress_bar::Indeterminate::new(true);
    bar.set("preparing local runtime");

    let venv = Command::new(&uv)
        .arg("venv")
        .arg("--quiet")
        .arg("--clear")
        .arg("--python")
        .arg("3.12")
        .arg(config_dir().join("venv"))
        .status()
        .ok()?;
    if !venv.success() {
        bar.finish();
        out::error_line("could not create the PLS venv");
        return None;
    }
    let synced = Command::new(&uv)
        .arg("pip")
        .arg("sync")
        .arg("--quiet")
        .arg(&lock)
        .arg("--python")
        .arg(&python)
        .status()
        .ok()?;
    bar.finish();
    if !synced.success() {
        out::error_line("could not install the PLS dependencies");
        return None;
    }
    std::fs::write(&marker, wanted).ok()?;
    Some(python)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Kill a router left behind by a previous wally that did not get to clean up.
///
/// The pid may be recycled by now, so the recorded command line is checked
/// against the router script before the signal is sent.
pub fn sweep_stale() {
    let Ok(text) = std::fs::read_to_string(pid_path()) else {
        return;
    };
    let mut parts = text.split_whitespace();
    let Ok(pid) = parts.next().unwrap_or_default().parse::<i32>() else {
        let _ = std::fs::remove_file(pid_path());
        return;
    };
    let recorded = parts.next().unwrap_or_default();
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only probes for the pid's existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if alive && process_is_router(pid, recorded) {
            // SAFETY: probed alive and identified as a router just above.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            out::status_line(&format!("stopped a leftover router (pid {pid})"));
        }
    }
    let _ = std::fs::remove_file(pid_path());
}

/// Whether `pid` is the recorded router script, not an unrelated process that
/// inherited the number.
fn process_is_router(pid: i32, recorded: &str) -> bool {
    if recorded.is_empty() {
        return false;
    }
    let Ok(out) = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains(recorded)
}

/// Record the model paths in the config file, preserving the person's mode and
/// cap. Called before `start`.
pub fn configure(model_path: &str, cloud_model: &str) {
    let _ = std::fs::create_dir_all(config_dir());
    let path = config_file();
    let mut value: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    value["model_path"] = serde_json::json!(model_path);
    value["cloud_model"] = serde_json::json!(cloud_model);
    let _ = std::fs::write(&path, value.to_string());
}

/// Start the router for `model_path`. Returns `None` after reporting why.
pub fn start(model_path: &str) -> Option<Router> {
    let script = materialise().or_else(|| {
        out::error_line("could not write the router scripts to the profile directory");
        None
    })?;
    let python = ensure_venv(&script)?;

    sweep_stale();
    std::fs::create_dir_all(config_dir()).ok()?;
    let token = launch_token();

    let mut bar = crate::progress::progress_bar::Indeterminate::new(true);
    bar.set("starting router");

    let mut child = Command::new(&python)
        .arg(&script)
        .env("PLS_MODEL", model_path)
        .env("PLS_PARENT_PID", std::process::id().to_string())
        .env("PLS_TOKEN", &token)
        .env(
            "PLS_ENTITLEMENT_SOCKET",
            config_dir().join("entitlement.sock"),
        )
        // The debug console is temporary; forwarded only when set.
        .env("PLS_DEBUG", std::env::var("PLS_DEBUG").unwrap_or_default())
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    // The read must not block the timeout, so the lines arrive over a channel
    // and this side waits with a deadline.
    let (sender, receiver) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if sender.send(Some(line)).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = sender.send(None);
    });

    let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let port = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(Some(line)) => {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
                    if let Some(port) = value.get("port").and_then(|p| p.as_u64()) {
                        break port as u16;
                    }
                }
            }
            Ok(None) => {
                bar.finish();
                out::error_line("the router exited before it reported a port");
                let _ = child.wait();
                return None;
            }
            Err(_) => {
                bar.finish();
                out::error_line("the router did not report a port in time");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    bar.finish();

    let _ = std::fs::write(pid_path(), format!("{} {}", child.id(), script.display()));
    let base_url = format!("http://127.0.0.1:{port}");
    out::status_line(&format!("router ready on {base_url}"));
    if std::env::var("PLS_DEBUG").as_deref() == Ok("1") {
        out::status_line(&format!("debug console: {base_url}/debug"));
    }
    Some(Router {
        child,
        base_url,
        token,
        entitlement: Entitlement::start(),
    })
}

/// A fresh 128-bit hex secret for this launch.
fn launch_token() -> String {
    let mut bytes = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        let seed = format!(
            "{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        return format!("{:x}", fnv1a(seed.as_bytes()));
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_token_is_128_bits_of_hex() {
        let first = launch_token();
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, launch_token());
    }

    #[test]
    fn a_plan_the_console_does_not_describe_is_not_premium() {
        assert!(plan_is_premium("pro"));
        assert!(plan_is_premium("Premium"));
        assert!(plan_is_premium("beta"));
        assert!(!plan_is_premium("free"));
        assert!(!plan_is_premium(""));
    }
}
