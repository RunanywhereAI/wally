//! Port of src/commands/cmd_account.cpp.

#[cfg(windows)]
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::account::{
    self as account, ConsoleClient, Credentials, Grant, IdentityResult, PollResult,
};
use crate::cli::App;
use crate::cli_formatter::{examples_footer, Example};
use crate::io::output as out;
use crate::util::getenv;

/// Send the approval link to a console of your choosing.
///
/// The control plane builds the approval URL from its own configured console
/// origin, so a locally served console is otherwise unreachable: sign-in keeps
/// opening the deployed one no matter where the CLI is pointed. The origin-match
/// check still runs against what the server sent; this replaces the origin only
/// afterwards, only from the environment, and only with an origin that passes
/// the same rules -- the path and request code stay exactly as sent.
///
/// The baked origin is paired with the baked API and is used only when that API
/// is the one being contacted. It used to apply to any API, so a dev build
/// pointed at another console rewrote that console's approval URL to the baked
/// origin and then refused it as off-origin -- `WALLY_CONSOLE_URL` alone could
/// not sign in to a local console (#91).
fn console_web_origin(console_url: &str) -> String {
    let configured = getenv("WALLY_CONSOLE_WEB_URL")
        // rcli-era override, still honored so it doesn't go silently unread
        // after the wally rename.
        .or_else(|| getenv("RCLI_CONSOLE_WEB_URL"));
    let configured = match configured {
        Some(value) => value,
        None => {
            // A dev build carries its approval console compiled in (see
            // baked_endpoints.h.in) -- empty in production builds, and the env
            // overrides above always win. Pairwise, exactly as
            // account::trusted_browser_origins pairs them.
            let baked_api = account::baked_console_api_url();
            let baked_web = env!("WALLY_BAKED_CONSOLE_WEB_ORIGIN");
            if !baked_api.is_empty() && console_url == baked_api && !baked_web.is_empty() {
                baked_web.to_string()
            } else {
                return String::new();
            }
        }
    };
    account::normalize_console_url(&configured).unwrap_or_default()
}

fn rebase_approval_url(url: &str, console_url: &str) -> String {
    let origin = console_web_origin(console_url);
    if origin.is_empty() {
        return url.to_string();
    }
    let Some(scheme) = url.find("://") else {
        return url.to_string();
    };
    // The first of '/', '?', or '#' -- not just '/' -- so a server-sent URL
    // with a query but no path (an origin-only approval link plus `?code=...`)
    // still keeps its request code instead of being rebased to a bare origin.
    match url[scheme + 3..].find(['/', '?', '#']) {
        Some(offset) => origin + &url[scheme + 3 + offset..],
        None => origin,
    }
}

fn epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn hostname() -> String {
    use windows_sys::Win32::Networking::WinSock::{gethostname, WSAStartup, WSADATA};
    // SAFETY: WSAStartup/gethostname are plain FFI calls with valid, correctly
    // sized out-parameters; nothing here escapes this function.
    unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        if WSAStartup(0x0202, &mut data) != 0 {
            return "unknown".to_string();
        }
        let mut buffer = [0u8; 256];
        let rc = gethostname(buffer.as_mut_ptr(), buffer.len() as i32 - 1);
        if rc == 0 {
            let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
            if end > 0 {
                return String::from_utf8_lossy(&buffer[..end]).into_owned();
            }
        }
    }
    "unknown".to_string()
}

#[cfg(not(windows))]
fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: `buffer` is sized and passed with its exact length; gethostname
    // writes at most that many bytes and we only read what it wrote.
    let rc =
        unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len() - 1) };
    if rc == 0 {
        let end = buffer.iter().position(|&b| b == 0).unwrap_or(0);
        if end > 0 {
            return String::from_utf8_lossy(&buffer[..end]).into_owned();
        }
    }
    "unknown".to_string()
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        let spawned = Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
        if spawned.is_err() {
            out::status_line("could not open a browser; use the URL printed above");
        }
    }
    // Not `std::process::Command`: Rust's `spawn()`/`status()` report a failed
    // exec (e.g. a missing `xdg-open`) as `Err`, indistinguishable from
    // `fork()` itself failing. C++'s OpenBrowser forks, execvp's in the child
    // and _exit(127)s on exec failure, and the parent only waitpid's to absorb
    // EINTR without inspecting the exit status -- so a missing opener binary
    // is silent, and only fork() failing prints the fallback line. Matching
    // that needs the same fork/exec split, not the higher-level `Command` API.
    #[cfg(not(windows))]
    {
        #[cfg(target_os = "macos")]
        let opener = "open";
        #[cfg(not(target_os = "macos"))]
        let opener = "xdg-open";

        let Ok(opener_c) = std::ffi::CString::new(opener) else {
            out::status_line("could not open a browser; use the URL printed above");
            return;
        };
        let Ok(url_c) = std::ffi::CString::new(url) else {
            out::status_line("could not open a browser; use the URL printed above");
            return;
        };
        let argv: [*const libc::c_char; 3] = [opener_c.as_ptr(), url_c.as_ptr(), std::ptr::null()];

        // SAFETY: fork() duplicates this process; both branches below only
        // touch the (already-owned, exec-only-uses) fds and pointers prepared
        // above, matching C++'s OpenBrowser.
        let child = unsafe { libc::fork() };
        if child < 0 {
            out::status_line("could not open a browser; use the URL printed above");
            return;
        }
        if child == 0 {
            // SAFETY: this is the forked child. It only redirects its own
            // stdout/stderr to /dev/null, then either exec's (which replaces
            // this process image and never returns) or _exit(127)s; it never
            // returns to the caller.
            unsafe {
                let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                if devnull >= 0 {
                    libc::dup2(devnull, libc::STDOUT_FILENO);
                    libc::dup2(devnull, libc::STDERR_FILENO);
                    libc::close(devnull);
                }
                libc::execvp(opener_c.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        // SAFETY: `child` is the pid fork() just returned to the parent; this
        // only waits on it, discarding the exit status like C++ does, and
        // loops solely to absorb EINTR.
        unsafe {
            let mut status: libc::c_int = 0;
            while libc::waitpid(child, &mut status, 0) < 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
            {}
        }
    }
}

fn load_credentials() -> Option<Credentials> {
    match account::load() {
        Ok(credentials) => Some(credentials),
        Err(failure) => {
            out::error_line(&failure);
            None
        }
    }
}

fn apply_grant(grant: &Grant, credentials: &mut Credentials) {
    credentials.access_token = grant.access_token.clone();
    if !grant.refresh_token.is_empty() {
        credentials.refresh_token = grant.refresh_token.clone();
    }
    if !grant.email.is_empty() {
        credentials.email = grant.email.clone();
    }
    credentials.expires_at = epoch_seconds()
        + if grant.expires_in > 0 {
            grant.expires_in
        } else {
            3600
        };
}

fn refresh_session(client: &ConsoleClient, credentials: &mut Credentials) -> Result<(), String> {
    if credentials.refresh_token.is_empty() {
        return Err("the cloud session cannot be refreshed; run `wally account login`".to_string());
    }
    let grant = client
        .refresh(&credentials.console_url, &credentials.refresh_token)
        .map_err(|e| e.message)?;
    apply_grant(&grant, credentials);
    account::save(credentials)
}

fn login(requested_console: &str, open: bool) -> i32 {
    let configured = if requested_console.is_empty() {
        account::default_console_url()
    } else {
        requested_console.to_string()
    };
    let console_url = match account::normalize_console_url(&configured) {
        Ok(url) => url,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };

    let client = ConsoleClient::default();
    let host = hostname();
    let authorization = match client.begin_authorization(
        &console_url,
        &host,
        Some(&|| out::status_line("server busy, retrying")),
    ) {
        Ok(authorization) => authorization,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };

    // Rebase first, then check what we will actually open.
    //
    // "Must match the API's own origin" was the wrong test: the control plane
    // runs on Cloud Run and the console it hands you runs on Railway, so the two
    // are never equal and the check refused every real sign-in. Which origin to
    // trust is account::trusted_browser_origins's answer, and it is never
    // empty -- the version of this that fell back to an empty string pinned
    // nothing at all in the shipped configuration, because the environment
    // variable it read is unset unless an operator sets it.
    let trusted = account::trusted_browser_origins(&console_url);
    let approval_url = rebase_approval_url(&authorization.verification_url, &console_url);
    if !account::browser_url_is_trusted(&approval_url, &trusted) {
        out::error_line("console returned an approval URL outside its origin");
        return 1;
    }

    out::status_line("approve this sign-in in your browser");
    out::result_line(&format!("code  {}", authorization.request_code));
    out::result_line(&format!("url   {approval_url}"));
    if open {
        open_browser(&approval_url);
    }
    out::status_line("waiting for approval");

    let deadline = Instant::now() + Duration::from_secs(authorization.expires_in.max(0) as u64);
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let outcome = client.poll(&console_url, &authorization);
        match outcome.result {
            PollResult::Pending => {
                // A console that asked for a delay gets it. Polling at the
                // authorization's own interval through a 30-second backoff is
                // just refusing to hear the answer (#90). The grant's expiry
                // still bounds the wait, so this cannot outlive the login.
                if !outcome.error.is_empty() && outcome.retry_after <= authorization.interval {
                    out::status_line("server busy, retrying");
                }
                let delay =
                    account::next_poll_delay_seconds(authorization.interval, outcome.retry_after);
                let wait = Duration::from_secs(delay.max(0) as u64);
                let now = Instant::now();
                if now >= deadline {
                    // The `while`/`loop` condition re-reads the clock and ends
                    // it on the next turn, where the "expired" message lives.
                    break;
                }
                let remaining = deadline - now;
                if wait > remaining {
                    if outcome.retry_after > 0 {
                        // The console will still be refusing when this
                        // request expires. Sleeping until then would look
                        // like a hang and end in the same failure, so say
                        // the number and stop.
                        out::error_line(&format!("Wally Cloud is busy - try again in {delay}s"));
                        return 1;
                    }
                    // Only our own poll cadence overshoots the deadline: the
                    // request is simply running out. Blaming a busy console
                    // here sent people looking for an outage.
                    std::thread::sleep(remaining);
                    continue;
                }
                // A wait the console asked for is longer than the cadence the
                // person was told about, so it is worth naming.
                if delay > authorization.interval {
                    out::status_line(&format!("console busy, waiting {delay}s as asked"));
                }
                std::thread::sleep(wait);
                continue;
            }
            PollResult::Denied => {
                out::error_line("the request was denied in the browser");
                return 1;
            }
            PollResult::Expired => {
                out::error_line("the request expired before it was approved");
                return 1;
            }
            PollResult::Failed => {
                out::error_line(&outcome.error);
                return 1;
            }
            PollResult::Approved => {
                let Some(grant) = outcome.grant else {
                    out::error_line(&outcome.error);
                    return 1;
                };
                let mut credentials = Credentials {
                    console_url: console_url.clone(),
                    ..Credentials::default()
                };
                apply_grant(&grant, &mut credentials);
                if let Err(failure) = account::save(&credentials) {
                    out::error_line(&failure);
                    return 1;
                }
                // Prime the model catalog cache while the token is freshest, so
                // a later harness launch can validate a -m offline.
                account::refresh_model_cache(&credentials);
                let identity = if credentials.email.is_empty() {
                    "your account".to_string()
                } else {
                    credentials.email.clone()
                };
                out::status_line(&format!("signed in as {identity}"));
                out::status_line(&format!(
                    "cloud session stored in {}",
                    account::profile_directory()
                ));
                return 0;
            }
        }
    }
    out::error_line("timed out waiting for approval");
    1
}

fn logout() -> i32 {
    let Some(credentials) = load_credentials() else {
        return 1;
    };
    if !credentials.signed_in() && credentials.refresh_token.is_empty() {
        out::status_line("not signed in");
        return 0;
    }

    let client = ConsoleClient::default();
    let revoke_result = client.revoke(
        &credentials.console_url,
        &credentials.access_token,
        &credentials.refresh_token,
    );
    if let Err(clear_failure) = account::clear() {
        out::error_line(&clear_failure);
        return 1;
    }
    account::clear_model_cache();
    out::status_line("signed out on this machine");
    match revoke_result {
        Err(revoke_failure) => {
            out::error_line(&format!("{revoke_failure}; the local session was removed"));
            1
        }
        Ok(()) => {
            out::status_line("cloud session revoked");
            0
        }
    }
}

fn who_am_i(as_json: bool) -> i32 {
    let Some(mut credentials) = load_credentials() else {
        return 1;
    };
    if !credentials.signed_in() {
        out::error_line("not signed in — run `wally account login`");
        return 1;
    }

    let client = ConsoleClient::default();
    if credentials.access_token_expired(epoch_seconds(), 60) {
        if let Err(failure) = refresh_session(&client, &mut credentials) {
            out::error_line(&failure);
            return 1;
        }
    }

    let (mut result, mut identity, mut failure) =
        client.who_am_i(&credentials.console_url, &credentials.access_token);
    if result == IdentityResult::Unauthorized {
        if let Err(failure) = refresh_session(&client, &mut credentials) {
            out::error_line(&failure);
            return 1;
        }
        (result, identity, failure) =
            client.who_am_i(&credentials.console_url, &credentials.access_token);
    }
    if result != IdentityResult::Ok {
        out::error_line(&failure);
        return 1;
    }

    // whoami is identity only, by decision: plan, spend and token usage belong
    // to `wally usage`, and an e2e guard (tests/test_account_cli.py) fails the
    // build if any of them leak in here. The README is worded to match.
    if as_json {
        let mut json = out::JsonWriter::new();
        json.begin_object()
            .field_str("email", &identity.email)
            .field_str("session", "active")
            .field_str("console", &credentials.console_url)
            .end_object();
        out::result_line(json.str());
        return 0;
    }
    out::result_line(&format!("{:<14} {}", "email", identity.email));
    out::result_line(&format!("{:<14} {}", "session", "active"));
    // The console endpoint is an internal URL that means nothing to the
    // reader, the same call `wally about` makes; it stays in --json for
    // tooling only.
    0
}

pub fn register_account(app: &mut App) {
    let account_cmd = app.add_subcommand("account", "Manage your RunAnywhere cloud account");
    account_cmd.require_subcommand(1, 1);

    let login_cmd = account_cmd.add_subcommand("login", "Sign in through the browser");
    login_cmd.add_flag(
        "--no-browser",
        "Print the sign-in URL instead of opening it",
    );
    login_cmd.footer(&examples_footer(&[
        Example::new("wally account login", ""),
        Example::new("wally account login --no-browser", ""),
    ]));
    // The console origin is not a user-facing flag: it comes from the baked
    // default, or WALLY_CONSOLE_URL for a dev build (read directly in
    // credentials.rs). login() falls back to that when handed an empty string.
    login_cmd.callback(|p, _g| login("", !p.flag("--no-browser")));

    let logout_cmd = account_cmd.add_subcommand("logout", "Sign out and revoke the session");
    logout_cmd.footer(&examples_footer(&[Example::new(
        "wally account logout",
        "",
    )]));
    logout_cmd.callback(|_p, _g| logout());

    let whoami_cmd = account_cmd.add_subcommand("whoami", "Show the signed-in account");
    whoami_cmd.add_flag("--json", "Print as JSON");
    whoami_cmd.footer(&examples_footer(&[
        Example::new("wally account whoami", ""),
        Example::new("wally --json account whoami", ""),
    ]));
    // `wally --json account whoami` and `... whoami --json` mean the same
    // thing; see the identical fix in register_usage (cmd_usage.rs).
    whoami_cmd.callback(|p, g| who_am_i(p.flag("--json") || g.json));
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::{open_browser, rebase_approval_url};
    use crate::util::env_lock::lock as env_lock;

    // C++'s OpenBrowser only prints the fallback line when fork() itself
    // fails; the parent never inspects the child's exec outcome. A missing
    // opener binary (PATH with nothing in it) must be just as silent here,
    // not surfaced as an extra status line.
    #[test]
    fn open_browser_stays_silent_when_opener_binary_is_missing() {
        let _lock = env_lock();
        let empty_path_dir = tempfile::tempdir().expect("temp dir");
        let saved_path = std::env::var_os("PATH");
        // SAFETY: `_lock` serializes every test in this process that touches
        // PATH or process-wide fds 1/2.
        unsafe { std::env::set_var("PATH", empty_path_dir.path()) };

        let mut pipe_fds = [0i32; 2];
        // SAFETY: `pipe_fds` is a valid 2-element buffer for pipe(2) to fill.
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        let [read_fd, write_fd] = pipe_fds;
        // SAFETY: STDERR_FILENO is always open in a test process; dup()
        // returns a new fd referencing the same open file description.
        let saved_stderr = unsafe { libc::dup(libc::STDERR_FILENO) };
        assert!(saved_stderr >= 0);
        // SAFETY: redirects this process's stderr to the pipe's write end for
        // the duration of the call below, then `write_fd` is redundant and
        // closed; `open_browser`'s own child redirects its inherited copy of
        // fd 2 to /dev/null before it ever execs, so only a `status_line` call
        // in the parent (this process) can land in the pipe.
        unsafe {
            assert_eq!(
                libc::dup2(write_fd, libc::STDERR_FILENO),
                libc::STDERR_FILENO
            );
            libc::close(write_fd);
        }

        open_browser("https://example.test/approve");

        // SAFETY: restores the real stderr and drops the pipe-writing fd, so
        // the read below observes end-of-file once drained.
        unsafe {
            assert_eq!(
                libc::dup2(saved_stderr, libc::STDERR_FILENO),
                libc::STDERR_FILENO
            );
            libc::close(saved_stderr);
        }
        if let Some(path) = saved_path {
            // SAFETY: still under `_lock`.
            unsafe { std::env::set_var("PATH", path) };
        } else {
            // SAFETY: still under `_lock`.
            unsafe { std::env::remove_var("PATH") };
        }

        let mut buffer = Vec::new();
        // SAFETY: `read_fd` is the pipe's read end; every writer was closed
        // above, so this drains whatever was written and then returns 0.
        loop {
            let mut chunk = [0u8; 256];
            let read = unsafe { libc::read(read_fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if read <= 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read as usize]);
        }
        // SAFETY: closes the read end now that draining is done.
        unsafe { libc::close(read_fd) };

        assert!(
            buffer.is_empty(),
            "open_browser must stay silent on a missing opener binary, like C++; got: {:?}",
            String::from_utf8_lossy(&buffer)
        );
    }

    // An origin-level approval URL with a query but no path (e.g. Railway's
    // preview host) must keep its `?code=...` suffix, or the browser opens a
    // console with no sign-in request and login times out waiting for a poll
    // that never arrives.
    #[test]
    fn rebase_approval_url_preserves_a_query_only_suffix() {
        let _lock = env_lock();
        let saved = std::env::var_os("WALLY_CONSOLE_WEB_URL");
        // SAFETY: `_lock` serializes every test in this process that touches
        // WALLY_CONSOLE_WEB_URL.
        unsafe { std::env::set_var("WALLY_CONSOLE_WEB_URL", "https://console.runanywhere.ai") };

        let result = rebase_approval_url(
            "https://runanywhere-frontend-production.up.railway.app?code=abc",
            "https://inference.runanywhere.ai",
        );

        match saved {
            // SAFETY: still under `_lock`.
            Some(value) => unsafe { std::env::set_var("WALLY_CONSOLE_WEB_URL", value) },
            None => unsafe { std::env::remove_var("WALLY_CONSOLE_WEB_URL") },
        }

        assert_eq!(result, "https://console.runanywhere.ai?code=abc");
    }
}
