//! Lifecycle for the PLS router: write its assets out, build the venv once,
//! spawn it on port 0, read the handshake, and tear it down without leaving an
//! orphan holding the GPU.
//!
//! The router scripts are compiled into wally, so an installed binary carries
//! them. They are written to the profile directory on start; a hand edit there
//! wins until the next launch overwrites it.

// The router is macOS-only (its local lane is MLX), but the module still
// builds everywhere so the command surface is the same on every platform.
// Off Unix wally's socket is a stub, which leaves the decision
// service and the pid probe unreferenced.
#![cfg_attr(not(unix), allow(dead_code, unused_variables, unused_imports))]

use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
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
/// Lowest `p_yes` that answers yes. Privacy asks whether the prompt is
/// private; a yes keeps it on device, so calling it not-private takes a
/// clearer no than a coin flip. Cost stays at an even split.
const PRIVACY_YES_THRESHOLD: f64 = 0.3;
const COST_YES_THRESHOLD: f64 = 0.5;

fn yes_threshold(policy: Policy) -> f64 {
    match policy {
        Policy::Privacy => PRIVACY_YES_THRESHOLD,
        Policy::Cost => COST_YES_THRESHOLD,
    }
}

/// Whether the router can run here. Its local lane is MLX and wally's
/// socket is a Unix socket, so only macOS qualifies.
pub fn supported() -> Result<(), String> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err("the hybrid router needs macOS: its local lane runs on MLX".to_string())
    }
}

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
pub(crate) fn decide_reply(
    policy: Policy,
    response: &contract::DecisionsResponse,
    ms: u128,
) -> serde_json::Value {
    let p_yes = response
        .answers
        .0
        .get("route")
        .and_then(|answer| answer.probabilities.0.get("yes"))
        .copied();
    match p_yes {
        Some(p_yes) => serde_json::json!({
            "answer": if p_yes >= yes_threshold(policy) { "yes" } else { "no" },
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

/// The harness a hybrid launch serves. The router reads it from `config.json`
/// to know the harness's spawn tool, child signal and prompt cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HybridHarness {
    DeepSeek,
    /// opencode 1.x, and OpenWork, whose engine is pinned to 1.x.
    OpenCodeV1,
    OpenCodeV2,
    Hermes,
    OpenClaw,
}

impl HybridHarness {
    /// The profile id `pls.py` knows it by.
    pub fn as_str(self) -> &'static str {
        match self {
            HybridHarness::DeepSeek => "deepseek",
            HybridHarness::OpenCodeV1 => "opencode-v1",
            HybridHarness::OpenCodeV2 => "opencode-v2",
            HybridHarness::Hermes => "hermes",
            HybridHarness::OpenClaw => "openclaw",
        }
    }

    /// The opencode profile for an installed major version. An unknown
    /// version is taken as 2, the version `npm i -g opencode-ai` installs.
    pub fn for_open_code(major_version: Option<u64>) -> HybridHarness {
        match major_version {
            Some(major) if major < 2 => HybridHarness::OpenCodeV1,
            _ => HybridHarness::OpenCodeV2,
        }
    }
}

/// `value` with the harness and the parent's limits a launch serves. A limit
/// of 0 is unknown and is left out, so the router falls back to the local one.
pub(crate) fn harness_value(
    mut value: serde_json::Value,
    harness: HybridHarness,
    cloud_context: i64,
    cloud_output: i64,
) -> serde_json::Value {
    value["harness"] = serde_json::json!(harness.as_str());
    let mut limits = serde_json::json!({});
    if cloud_context > 0 {
        limits["context_length"] = serde_json::json!(cloud_context);
    }
    if cloud_output > 0 {
        limits["max_completion_tokens"] = serde_json::json!(cloud_output);
    }
    value["cloud_limits"] = limits;
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
                    let policy = job.body.policy;
                    let request = build_decide_request(policy, &job.body.text);
                    let started = std::time::Instant::now();
                    let scored = {
                        let _hush = crate::util::hush::HushedStdout::mute();
                        crate::commands::cmd_decisions::score_loaded(component, &request)
                    };
                    let reply = scored.map(|response| {
                        decide_reply(policy, &response, started.elapsed().as_millis())
                    });
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
const MASK_STORE: &str = include_str!("../../pls/mask_store.py");
const PII_MODEL: &str = include_str!("../../pls/pii_model.py");
const LOCK: &str = include_str!("../../pls/requirements.lock");

/// A running router. Dropping it stops the process.
pub struct Router {
    child: Child,
    pub base_url: String,
    /// Per-launch secret; the providers send it as `x-pls-token` so another
    /// local process cannot spend the account's cloud quota.
    pub token: String,
    /// Set once `running` reaps the child, so a later drop does not signal a
    /// recycled pid. `exit_code` reads it after the child has been reaped.
    exit_code: Option<i32>,
    #[allow(dead_code)] // held for its Drop; the router reads it over the socket
    socket: Option<WallySocket>,
}

impl Router {
    /// Whether the router process is still alive.
    pub fn running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exit_code = Some(status.code().unwrap_or(1));
                false
            }
            Err(_) => {
                self.exit_code = Some(1);
                false
            }
        }
    }

    /// The child's exit status. `0` while it is still running.
    pub fn exit_code(&mut self) -> i32 {
        if self.running() {
            0
        } else {
            self.exit_code.unwrap_or(1)
        }
    }
}

/// Block until the router exits and return its status.
pub fn wait_exit(router: &mut Router) -> i32 {
    while router.running() {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    router.exit_code()
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

/// Wally's side of the router, on a unix socket: `POST /decide` runs the
/// decision model and `GET /token` hands out the cloud token. Nothing else
/// is served.
#[cfg(unix)]
pub struct WallySocket {
    stop: Arc<AtomicBool>,
    socket: PathBuf,
    accept: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl WallySocket {
    fn start(decider: Option<ResolvedModelPaths>) -> Option<Self> {
        let decider = decider.and_then(Decider::start).map(Arc::new);
        let socket = socket_path();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).ok()?;
        let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));
        listener.set_nonblocking(true).ok()?;

        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let stop = Arc::clone(&stop);
            let listener = listener.try_clone().ok()?;
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let decider = decider.clone();
                            // A decision can take seconds; it must not hold up
                            // a token request behind it.
                            std::thread::spawn(move || answer(stream, decider));
                        }
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(100)),
                    }
                }
            })
        };
        Some(WallySocket {
            stop,
            socket,
            accept: Some(accept),
        })
    }
}

#[cfg(unix)]
impl Drop for WallySocket {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.socket);
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

/// Off Unix there is no socket: the router never starts there.
#[cfg(not(unix))]
pub struct WallySocket;

#[cfg(not(unix))]
impl WallySocket {
    fn start(_decider: Option<ResolvedModelPaths>) -> Option<Self> {
        None
    }
}

/// One request off the socket: its request line and body.
#[cfg(unix)]
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

#[cfg(unix)]
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

#[cfg(unix)]
fn answer(mut stream: UnixStream, decider: Option<Arc<Decider>>) {
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
    if request_line.starts_with("GET /token") {
        let force = request_line.contains("refresh=1");
        match token_reply(force) {
            Ok(body) => respond(&mut stream, "200 OK", &body),
            Err(error) => respond(
                &mut stream,
                "503 Service Unavailable",
                &serde_json::json!({ "error": error }),
            ),
        }
        return;
    }
    if !request_line.starts_with("POST /decide ") {
        respond(
            &mut stream,
            "404 Not Found",
            &serde_json::json!({ "error": "not found" }),
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

/// Write a refresh grant back to the profile. An empty rotated refresh token
/// keeps the previous one; a non-empty one replaces it. The access token is
/// always the grant's.
fn apply_grant(
    credentials: &mut account::Credentials,
    grant: &account::Grant,
) -> Result<(), String> {
    if grant.access_token.is_empty() {
        return Err("refresh returned no access token".to_string());
    }
    credentials.access_token = grant.access_token.clone();
    if !grant.refresh_token.is_empty() {
        credentials.refresh_token = grant.refresh_token.clone();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    credentials.expires_at = now
        + if grant.expires_in > 0 {
            grant.expires_in
        } else {
            3600
        };
    account::save(credentials)
}

fn refresh_and_store(credentials: &mut account::Credentials) -> Result<account::Grant, String> {
    if credentials.refresh_token.is_empty() {
        return Err("the cloud session cannot be refreshed".to_string());
    }
    let grant = account::refresh(&credentials.console_url, &credentials.refresh_token)?;
    apply_grant(credentials, &grant)?;
    Ok(grant)
}

/// The access token the router should send to the cloud. `force` refreshes
/// even when the stored token has not expired, which is how a 401 is recovered
/// without the router rotating the token itself.
fn token_reply(force: bool) -> Result<serde_json::Value, String> {
    let mut credentials = account::load()?;
    if credentials.console_url.is_empty() || !credentials.signed_in() {
        return Err("not signed in".to_string());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if force || credentials.access_token_expired(now, 60) {
        refresh_and_store(&mut credentials)?;
    }
    Ok(serde_json::json!({
        "access_token": credentials.access_token,
        "console_url": credentials.console_url,
    }))
}

fn config_dir() -> PathBuf {
    PathBuf::from(account::profile_directory()).join("pls")
}

/// Where wally serves the router; handed over as `PLS_SOCKET`.
fn socket_path() -> PathBuf {
    config_dir().join("pls.sock")
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
        fnv1a(
            format!("{SCRIPT}\u{0}{MODEL_SERVER}\u{0}{MASK_STORE}\u{0}{PII_MODEL}\u{0}{LOCK}")
                .as_bytes(),
        )
    );
    let marker = dir.join("assets.sha");
    let script = dir.join("pls.py");
    if std::fs::read_to_string(&marker).unwrap_or_default().trim() == wanted && script.is_file() {
        return Some(script);
    }
    for (name, body) in [
        ("pls.py", SCRIPT),
        ("model_server.py", MODEL_SERVER),
        ("mask_store.py", MASK_STORE),
        ("pii_model.py", PII_MODEL),
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

    let Some(uv) = uv_path() else {
        out::error_line(
            "uv is not on PATH; install it to prepare the hybrid router's Python runtime",
        );
        return None;
    };
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
        .arg("--require-hashes")
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
pub(crate) fn configure(
    model_path: &str,
    cloud_model: &str,
    policy: Policy,
    harness: HybridHarness,
    cloud_limits: (i64, i64),
) {
    let _ = std::fs::create_dir_all(config_dir());
    let path = config_file();
    let existing: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let value = harness_value(
        configure_value(existing, model_path, cloud_model, policy),
        harness,
        cloud_limits.0,
        cloud_limits.1,
    );
    let _ = std::fs::write(&path, value.to_string());
}

/// Start the router for `model_path`, with `decider` answering `/decide`
/// on wally's socket when it is given. Returns `None` after
/// reporting why.
pub fn start(model_path: &str, decider: Option<ResolvedModelPaths>) -> Option<Router> {
    start_with(model_path, decider, Stdio::inherit())
}

/// Where a router behind a harness writes its event log: the harness owns
/// the terminal, and a full-screen TUI is drawn over by every line.
pub fn log_file() -> PathBuf {
    config_dir().join("pls.log")
}

/// `start`, with the router's and model server's stderr in `log_file()`
/// instead of the terminal. For launches that hand the terminal to a harness.
pub fn start_logged(model_path: &str, decider: Option<ResolvedModelPaths>) -> Option<Router> {
    let _ = std::fs::create_dir_all(config_dir());
    let log = match std::fs::File::create(log_file()) {
        Ok(log) => log,
        Err(error) => {
            out::error_line(&format!(
                "could not open the router log {} ({error})",
                log_file().display()
            ));
            return None;
        }
    };
    let router = start_with(model_path, decider, Stdio::from(log))?;
    out::status_line(&format!("router log: {}", log_file().display()));
    Some(router)
}

fn start_with(
    model_path: &str,
    decider: Option<ResolvedModelPaths>,
    stderr: Stdio,
) -> Option<Router> {
    let script = materialise().or_else(|| {
        out::error_line("could not write the router scripts to the profile directory");
        None
    })?;
    let python = ensure_venv(&script)?;

    sweep_stale();
    std::fs::create_dir_all(config_dir()).ok()?;
    let token = match launch_token() {
        Ok(token) => token,
        Err(error) => {
            out::error_line(&error);
            return None;
        }
    };

    let mut bar = crate::progress::progress_bar::Indeterminate::new(true);
    bar.set("starting router");

    let mut child = Command::new(&python)
        .arg(&script)
        .env("PLS_MODEL", model_path)
        .env("PLS_PARENT_PID", std::process::id().to_string())
        .env("PLS_TOKEN", &token)
        .env("PLS_SOCKET", socket_path())
        // The debug console is temporary; forwarded only when set.
        .env("PLS_DEBUG", std::env::var("PLS_DEBUG").unwrap_or_default())
        .env("PYTHONUNBUFFERED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
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
        exit_code: None,
        socket: WallySocket::start(decider),
    })
}

/// A fresh 128-bit hex secret, or an error when the CSPRNG cannot fill it.
/// A predictable fallback would let another local process forge the header.
fn launch_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    // The OS CSPRNG on every platform; `/dev/urandom` does not exist on Windows.
    let filled = getrandom::fill(&mut bytes).is_ok();
    token_or_refuse(filled, bytes)
}

fn token_or_refuse(filled: bool, bytes: [u8; 16]) -> Result<String, String> {
    if !filled {
        return Err("could not generate a launch token".to_string());
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
#[path = "pls_policy_tests.rs"]
mod policy_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_token_is_128_bits_of_hex() {
        let first = launch_token().expect("the test host has a CSPRNG");
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, launch_token().expect("the test host has a CSPRNG"));
    }

    #[test]
    fn a_failed_entropy_read_is_not_a_token() {
        let error =
            token_or_refuse(false, [7; 16]).expect_err("a failed fill must not invent a token");
        assert!(error.contains("launch token"));
    }

    #[test]
    fn a_rotated_refresh_token_is_written_to_the_profile() {
        let _lock = crate::util::env_lock::lock();
        let dir = std::env::temp_dir().join(format!("wally-pls-cred-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("WALLY_PROFILE_DIR", &dir);
        struct ClearProfile;
        impl Drop for ClearProfile {
            fn drop(&mut self) {
                std::env::remove_var("WALLY_PROFILE_DIR");
            }
        }
        let _clear = ClearProfile;
        let mut credentials = account::Credentials {
            console_url: "https://console.example".to_string(),
            email: "a@example.com".to_string(),
            access_token: "old-access".to_string(),
            refresh_token: "old-refresh".to_string(),
            expires_at: 1,
        };
        account::save(&credentials).unwrap();
        let grant = account::Grant {
            access_token: "new-access".to_string(),
            refresh_token: "new-refresh".to_string(),
            email: String::new(),
            plan: "pro".to_string(),
            expires_in: 3600,
        };
        apply_grant(&mut credentials, &grant).unwrap();
        let loaded = account::load().unwrap();
        assert_eq!(loaded.access_token, "new-access");
        assert_eq!(loaded.refresh_token, "new-refresh");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_router_that_exits_nonzero_reports_that_status() {
        let mut command = if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit 7"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 7"]);
            command
        };
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn exit probe");
        let mut router = Router {
            child,
            base_url: String::new(),
            token: String::new(),
            exit_code: None,
            socket: None,
        };
        assert_eq!(wait_exit(&mut router), 7);
    }

    /// A socket started in a fresh profile, removed again on drop.
    #[cfg(unix)]
    struct SocketUnderTest {
        dir: PathBuf,
        server: Option<WallySocket>,
    }

    #[cfg(unix)]
    impl SocketUnderTest {
        fn start(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("wally-pls-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("pls")).unwrap();
            std::env::set_var("WALLY_PROFILE_DIR", &dir);
            let server = WallySocket::start(None).expect("the socket starts");
            SocketUnderTest {
                dir,
                server: Some(server),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("pls").join("pls.sock")
        }

        /// The status line of the reply to one raw request.
        fn status_of(&self, request: &str) -> String {
            let mut stream = UnixStream::connect(self.path()).expect("connect to pls.sock");
            stream.write_all(request.as_bytes()).unwrap();
            let mut reply = String::new();
            let _ = stream.read_to_string(&mut reply);
            reply.lines().next().unwrap_or_default().to_string()
        }
    }

    #[cfg(unix)]
    impl Drop for SocketUnderTest {
        fn drop(&mut self) {
            drop(self.server.take());
            std::env::remove_var("WALLY_PROFILE_DIR");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_file_is_pls_sock_at_mode_0600() {
        let _lock = crate::util::env_lock::lock();
        let socket = SocketUnderTest::start("mode");
        let mode = std::fs::metadata(socket.path())
            .expect("pls.sock exists while the socket runs")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_answers_post_decide() {
        let _lock = crate::util::env_lock::lock();
        let socket = SocketUnderTest::start("decide");
        let body = r#"{"policy":"privacy","text":"hello"}"#;
        let status = socket.status_of(&format!(
            "POST /decide HTTP/1.1\r\nHost: local\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ));
        // No decision model in a test, so the answer is 503, not a 404.
        assert!(status.starts_with("HTTP/1.1 503"), "{status}");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_answers_get_token() {
        let _lock = crate::util::env_lock::lock();
        let socket = SocketUnderTest::start("token");
        let status = socket.status_of("GET /token HTTP/1.1\r\nHost: local\r\n\r\n");
        // Nobody is signed in to the test profile, so the answer is 503.
        assert!(status.starts_with("HTTP/1.1 503"), "{status}");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_has_no_entitlement_answer() {
        let _lock = crate::util::env_lock::lock();
        let socket = SocketUnderTest::start("entitlement");
        let status = socket.status_of("GET /entitlement HTTP/1.1\r\nHost: local\r\n\r\n");
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_answers_any_other_request_with_404() {
        let _lock = crate::util::env_lock::lock();
        let socket = SocketUnderTest::start("other");
        let status = socket.status_of("POST /elsewhere HTTP/1.1\r\nHost: local\r\n\r\n");
        assert!(status.starts_with("HTTP/1.1 404"), "{status}");
    }
}
