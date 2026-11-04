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
use crate::account::decisions_contract as contract;
use crate::commands::model_setup::ResolvedModelPaths;
use crate::io::output as out;

/// The local decision model the policies ask about each new prompt. The MLX
/// row: `clef-flash-9b` resolves to its GGUF file, which the kit's local
/// decision component refuses ("Feature/operation is not supported").
pub const DECISION_MODEL: &str = "mlx-clef-flash-9b-4bit";
/// Longest prompt text the decision model is shown.
const DECIDE_TEXT_MAX: usize = 32 * 1024;
/// Largest `/decide` request body read off the socket.
const DECIDE_BODY_MAX: usize = 256 * 1024;
/// How long one decision may take, load included, before the router is told no.
const DECIDE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// What the router optimises for on the hybrid lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Private work stays on the device; the cloud only orchestrates.
    Privacy,
    /// Large tasks go to the local subagent; the cloud plans.
    Cost,
}

impl Policy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Policy::Privacy => "privacy",
            Policy::Cost => "cost",
        }
    }
}

pub(crate) fn parse_policy(value: &str) -> Result<Policy, String> {
    match value {
        "privacy" => Ok(Policy::Privacy),
        "cost" => Ok(Policy::Cost),
        other => Err(format!("unknown policy '{other}': use privacy or cost")),
    }
}

/// The one yes/no question each policy asks about a prompt.
pub(crate) fn decide_question(policy: Policy) -> contract::YesNoQuestion {
    let question = match policy {
        Policy::Privacy => {
            "Does this request contain or ask to work on private data: proprietary \
             source code, internal documents, personal identifiers or credentials?"
        }
        // Picked on clef-flash 9B over six sample prompts: it scores 0.81 and
        // 0.88 for exploring a codebase into an HTML page and fixing a failing
        // build, 0.19 or less for a rename, a one-liner and small talk. The
        // earlier "Is this a large task" wording scored the first one 0.18.
        Policy::Cost => {
            "Would doing this request take many steps of tool use, such as \
             exploring a codebase, reading several files, writing or editing code, \
             building, running or debugging?"
        }
    };
    contract::YesNoQuestion {
        id: "route".to_string(),
        question: question.to_string(),
        r#type: "yes_no".to_string(),
        yes: None,
        no: None,
    }
}

pub(crate) fn build_decide_request(policy: Policy, text: &str) -> contract::DecisionsRequest {
    contract::DecisionsRequest {
        images: None,
        input: text.to_string(),
        model: DECISION_MODEL.to_string(),
        prompt_format_version: None,
        questions: vec![contract::DecisionQuestion::YesNoQuestion(decide_question(
            policy,
        ))],
        temperature: None,
    }
}

/// A parsed `POST /decide` body.
pub(crate) struct DecideBody {
    pub policy: Policy,
    pub text: String,
}

pub(crate) fn parse_decide_body(bytes: &[u8]) -> Result<DecideBody, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| format!("body is not JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "body is not a JSON object".to_string())?;
    let policy = object
        .get("policy")
        .and_then(|p| p.as_str())
        .ok_or_else(|| "policy is missing".to_string())
        .and_then(parse_policy)?;
    let text = object
        .get("text")
        .and_then(|t| t.as_str())
        .ok_or_else(|| "text is missing".to_string())?;
    if text.trim().is_empty() {
        return Err("text is empty".to_string());
    }
    let mut end = text.len().min(DECIDE_TEXT_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(DecideBody {
        policy,
        text: text[..end].to_string(),
    })
}

/// The `/decide` reply for a scored request. `error` is set when the model
/// gave no yes probability for the `route` question.
pub(crate) fn decide_reply(response: &contract::DecisionsResponse, ms: u128) -> serde_json::Value {
    let p_yes = response
        .answers
        .0
        .get("route")
        .and_then(|answer| answer.probabilities.0.get("yes"))
        .copied();
    match p_yes {
        Some(p_yes) => serde_json::json!({
            "answer": if p_yes >= 0.5 { "yes" } else { "no" },
            "p_yes": p_yes,
            "model": response.model,
            "ms": ms as u64,
        }),
        None => serde_json::json!({ "error": "the decision model gave no yes probability" }),
    }
}

/// `config.json` after a launch: the launch's values over whatever the file held.
pub(crate) fn configure_value(
    existing: serde_json::Value,
    model_path: &str,
    cloud_model: &str,
    policy: Policy,
) -> serde_json::Value {
    let mut value = if existing.is_object() {
        existing
    } else {
        serde_json::json!({})
    };
    value["model_path"] = serde_json::json!(model_path);
    value["cloud_model"] = serde_json::json!(cloud_model);
    value["policy"] = serde_json::json!(policy.as_str());
    value
}

/// Resolve the decision model before the harness starts, so a missing one
/// can be offered for download on the terminal instead of failing mid-session.
pub fn resolve_decider(options: &crate::bootstrap::GlobalOptions) -> Option<ResolvedModelPaths> {
    let offline = crate::bootstrap::GlobalOptions {
        offline: true,
        ..options.clone()
    };
    if crate::bootstrap::bootstrap(&offline).is_err() {
        out::error_line(
            "policy: the SDK did not start, so prompts are not routed by a decision model",
        );
        return None;
    }
    match crate::commands::model_setup::ensure_model_ready(&offline, DECISION_MODEL) {
        Ok(paths) => Some(paths),
        Err(_) => {
            out::error_line(&format!(
                "policy: {DECISION_MODEL} is not available; privacy keeps every prompt local, \
                 cost lets the cloud do everything"
            ));
            None
        }
    }
}

struct DecideJob {
    body: DecideBody,
    reply: mpsc::Sender<Result<serde_json::Value, String>>,
}

/// Owns the loaded decision component on one thread, so its handle never
/// moves between threads. Loads on the first request.
pub(crate) struct Decider {
    jobs: mpsc::Sender<DecideJob>,
}

impl Decider {
    fn start(model: ResolvedModelPaths) -> Option<Self> {
        let (jobs, inbox) = mpsc::channel::<DecideJob>();
        std::thread::Builder::new()
            .name("pls-decider".to_string())
            .spawn(move || {
                let mut loaded = None;
                for job in inbox {
                    if loaded.is_none() {
                        let _hush = crate::util::hush::HushedStdout::mute();
                        match crate::commands::cmd_decisions::load_decisions_component(&model) {
                            Ok(component) => loaded = Some(component),
                            Err(error) => {
                                let _ = job.reply.send(Err(error));
                                continue;
                            }
                        }
                    }
                    let Some(component) = loaded.as_ref() else {
                        continue;
                    };
                    let request = build_decide_request(job.body.policy, &job.body.text);
                    let started = std::time::Instant::now();
                    let scored = {
                        let _hush = crate::util::hush::HushedStdout::mute();
                        crate::commands::cmd_decisions::score_loaded(component, &request)
                    };
                    let reply = scored
                        .map(|response| decide_reply(&response, started.elapsed().as_millis()));
                    let _ = job.reply.send(reply);
                }
            })
            .ok()?;
        Some(Decider { jobs })
    }

    fn decide(&self, body: DecideBody) -> Result<serde_json::Value, String> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(DecideJob { body, reply })
            .map_err(|_| "the decision thread has stopped".to_string())?;
        answer
            .recv_timeout(DECIDE_TIMEOUT)
            .map_err(|_| "the decision model did not answer in time".to_string())?
    }
}

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
    fn start(decider: Option<ResolvedModelPaths>) -> Option<Self> {
        let decider = decider.and_then(Decider::start).map(Arc::new);
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
                        Ok((stream, _)) => {
                            let premium = premium.load(Ordering::SeqCst);
                            let decider = decider.clone();
                            // A decision can take seconds; it must not hold up
                            // the entitlement answers the router polls for.
                            std::thread::spawn(move || answer(stream, premium, decider));
                        }
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

/// One request off the socket: its request line and body.
fn read_request(stream: &mut UnixStream) -> Result<(String, Vec<u8>), String> {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut raw = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if raw.len() > 16 * 1024 {
            return Err("request headers are too large".to_string());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("connection closed before the headers ended".to_string()),
            Ok(n) => raw.extend_from_slice(&buffer[..n]),
            Err(error) => return Err(format!("read failed: {error}")),
        }
    };
    let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let length = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > DECIDE_BODY_MAX {
        return Err("request body is too large".to_string());
    }
    let mut body = raw[header_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buffer) {
            Ok(0) => return Err("connection closed before the body ended".to_string()),
            Ok(n) => body.extend_from_slice(&buffer[..n]),
            Err(error) => return Err(format!("read failed: {error}")),
        }
    }
    body.truncate(length);
    Ok((request_line, body))
}

fn respond(stream: &mut UnixStream, status: &str, body: &serde_json::Value) {
    let body = body.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

fn answer(mut stream: UnixStream, premium: bool, decider: Option<Arc<Decider>>) {
    let (request_line, body) = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            respond(
                &mut stream,
                "400 Bad Request",
                &serde_json::json!({ "error": error }),
            );
            return;
        }
    };
    if !request_line.starts_with("POST /decide ") {
        respond(
            &mut stream,
            "200 OK",
            &serde_json::json!({ "sub": premium }),
        );
        return;
    }
    let parsed = match parse_decide_body(&body) {
        Ok(parsed) => parsed,
        Err(error) => {
            respond(
                &mut stream,
                "400 Bad Request",
                &serde_json::json!({ "error": error }),
            );
            return;
        }
    };
    let Some(decider) = decider else {
        respond(
            &mut stream,
            "503 Service Unavailable",
            &serde_json::json!({ "error": format!("{DECISION_MODEL} is not loaded") }),
        );
        return;
    };
    match decider.decide(parsed) {
        Ok(reply) if reply.get("error").is_none() => respond(&mut stream, "200 OK", &reply),
        Ok(reply) => respond(&mut stream, "503 Service Unavailable", &reply),
        Err(error) => respond(
            &mut stream,
            "503 Service Unavailable",
            &serde_json::json!({ "error": error }),
        ),
    }
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
pub(crate) fn configure(model_path: &str, cloud_model: &str, policy: Policy) {
    let _ = std::fs::create_dir_all(config_dir());
    let path = config_file();
    let existing: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let value = configure_value(existing, model_path, cloud_model, policy);
    let _ = std::fs::write(&path, value.to_string());
}

/// Start the router for `model_path`, with `decider` answering `/decide`
/// on the entitlement socket when it is given. Returns `None` after
/// reporting why.
pub fn start(model_path: &str, decider: Option<ResolvedModelPaths>) -> Option<Router> {
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
        entitlement: Entitlement::start(decider),
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
#[path = "pls_policy_tests.rs"]
mod policy_tests;

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
