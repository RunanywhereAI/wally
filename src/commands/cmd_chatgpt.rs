//! `wally chatgpt`: the ChatGPT desktop app's Codex against a local or hosted
//! model, through the same Responses translator as `wally codex`.
//!
//! The app runs as a second instance with its own data and its own Codex
//! home, so the reader's ChatGPT app, its sign-in and `~/.codex` are never
//! touched, and nothing needs restoring however the session ends. The run and
//! the instance end together, the way `wally claude-desktop` does it: the app
//! quitting ends the run, and Ctrl-C quits the app.

use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::cmd_editors::context_window_problem;
use crate::commands::resolve_default_model;
use crate::harness;
use crate::io::output as out;

const COMMAND: &str = "chatgpt";

/// What a run needs on either platform: wally's folders for the app, the
/// translator, and the settings the app's Codex reads from them.
#[cfg(any(target_os = "macos", windows))]
mod session {
    use std::path::{Path, PathBuf};

    use crate::bootstrap::GlobalOptions;
    use crate::codex::{self, CodexShim};
    use crate::commands::cmd_editors::{
        cloud_context_window, context_window_for_tool, write_private,
    };
    use crate::config::cli_paths;
    use crate::harness::chatgpt::{
        chatgpt_auth, chatgpt_config, chatgpt_settings, with_onboarding_done,
    };
    use crate::harness::codex::codex_model_catalog;
    use crate::harness::{self, CatalogModel, Endpoint};
    use crate::io::output as out;

    /// Where the app's Codex home and its own app data live: wally's state
    /// directory, kept between runs so threads and project trust carry over.
    pub(super) struct Folders {
        pub(super) codex_home: PathBuf,
        pub(super) app_data: PathBuf,
        catalog: PathBuf,
    }

    fn folders() -> Option<Folders> {
        let state = cli_paths::state_dir();
        if state.is_empty() {
            return None;
        }
        let root = Path::new(&state).join("chatgpt");
        let folders = Folders {
            codex_home: root.join("codex-home"),
            app_data: root.join("app-data"),
            catalog: root.join("models.json"),
        };
        std::fs::create_dir_all(&folders.codex_home).ok()?;
        std::fs::create_dir_all(&folders.app_data).ok()?;
        Some(folders)
    }

    /// Writes the home's settings block, its model catalog and, the first
    /// time, its sign-in-skip `auth.json`. The error is for the person.
    fn prepare(
        folders: &Folders,
        model: &str,
        base_url: &str,
        catalog: &[CatalogModel],
        context: i64,
    ) -> Result<(), String> {
        let catalog_path = folders.catalog.to_string_lossy().into_owned();
        write_private(&folders.catalog, &codex_model_catalog(catalog))
            .map_err(|error| format!("could not write the model catalog ({error})"))?;
        let config_path = folders.codex_home.join("config.toml");
        let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
        let settings = chatgpt_settings(model, base_url, Some(&catalog_path), context);
        write_private(&config_path, &chatgpt_config(&existing, &settings))
            .map_err(|error| format!("could not write the app's Codex settings ({error})"))?;
        let state_path = folders.codex_home.join(".codex-global-state.json");
        let state = std::fs::read_to_string(&state_path).unwrap_or_default();
        if let Some(done) = with_onboarding_done(&state, crate::codex::translate::now_unix()) {
            write_private(&state_path, &done)
                .map_err(|error| format!("could not write the app's state ({error})"))?;
        }
        let auth = folders.codex_home.join("auth.json");
        if !auth.exists() {
            write_private(&auth, chatgpt_auth())
                .map_err(|error| format!("could not write the app's sign-in ({error})"))?;
        }
        Ok(())
    }

    /// A run's translator and the folders the app is pointed at.
    pub(super) struct Session {
        pub(super) folders: Folders,
        pub(super) shim: CodexShim,
        endpoint: Endpoint,
    }

    impl Session {
        /// Stops the translator and lets go of the model.
        pub(super) fn end(mut self) {
            codex::stop(&mut self.shim);
            harness::release(&self.endpoint);
        }
    }

    /// Starts the translator for `model` and writes the app's settings. None,
    /// with the reason already printed, when any of it fails.
    pub(super) fn start(
        model: &str,
        context_override: Option<i64>,
        options: &GlobalOptions,
    ) -> Option<Session> {
        let Some(folders) = folders() else {
            out::error_line("could not find a state directory for the app's data");
            return None;
        };
        let endpoint = harness::resolve(model, options, super::COMMAND)?;
        let catalog = harness::catalog_models_for(&endpoint, model);
        let Some(shim) = codex::start(&endpoint, model, options.verbose, &catalog) else {
            harness::release(&endpoint);
            return None;
        };
        let session = Session {
            folders,
            shim,
            endpoint,
        };
        let real = if session.endpoint.serving {
            session.endpoint.context_window
        } else if catalog[0].context_window > 0 {
            catalog[0].context_window
        } else {
            cloud_context_window(model)
        };
        let context = context_window_for_tool(real, context_override);
        let mut catalog = catalog;
        if context > 0 {
            catalog[0].context_window = context;
            out::status_line(&format!("context window: {context} tokens"));
        }
        if let Err(problem) = prepare(
            &session.folders,
            model,
            &session.shim.base_url,
            &catalog,
            context,
        ) {
            out::error_line(&problem);
            session.end();
            return None;
        }
        out::status_line(&format!(
            "chatgpt will talk to {model} through {}",
            session.shim.base_url
        ));
        Some(session)
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::bootstrap::GlobalOptions;
    use crate::desktop::instance;
    use crate::harness;
    use crate::harness::chatgpt::CHATGPT_BUNDLE_ID;
    use crate::harness::codex::CODEX_KEY_VARIABLE;
    use crate::io::output as out;
    use crate::util::getenv;

    pub(super) const DOWNLOAD: &str = "https://chatgpt.com/download";

    /// The installed ChatGPT app that carries Codex, or None. An app with
    /// another bundle id (the older chat-only one) is not it.
    pub(super) fn app_path() -> Option<String> {
        let mut roots = vec!["/Applications".to_string()];
        if let Some(home) = getenv("HOME").filter(|h| !h.is_empty()) {
            roots.push(format!("{home}/Applications"));
        }
        roots
            .iter()
            .map(|root| format!("{root}/ChatGPT.app"))
            .find(|path| bundle_id(path).as_deref() == Some(CHATGPT_BUNDLE_ID))
    }

    fn bundle_id(app: &str) -> Option<String> {
        let output = std::process::Command::new("/usr/libexec/PlistBuddy")
            .args(["-c", "Print :CFBundleIdentifier"])
            .arg(format!("{app}/Contents/Info.plist"))
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    pub(super) fn run(
        app: &str,
        model: &str,
        context_override: Option<i64>,
        options: &GlobalOptions,
    ) -> i32 {
        let Some(session) = super::session::start(model, context_override, options) else {
            return 1;
        };

        // The instance this run starts, found by pid since launchd starts it.
        // Ctrl-C quits it; its exit ends the run even when `open -W` misses it.
        let instance = Arc::new(AtomicI32::new(0));
        let instance_for_signal = Arc::clone(&instance);
        let _quit_on_signal = crate::util::interrupt::on_interrupt(move || {
            let pid = instance_for_signal.load(Ordering::SeqCst);
            if pid > 0 && !instance::quit(pid, Duration::from_secs(5)) {
                out::error_line("chatgpt did not quit; close it by hand");
            }
            // SAFETY: _exit has no preconditions and does not return; it
            // skips destructors while the translator's thread may be mid-call.
            unsafe { libc::_exit(130) };
        });
        let finished = Arc::new(AtomicBool::new(false));
        {
            let before = instance::main_pids(app);
            let app = app.to_string();
            let instance = Arc::clone(&instance);
            let finished = Arc::clone(&finished);
            std::thread::spawn(move || {
                let Some(pid) = instance::wait_for_new(&app, &before, Duration::from_secs(60))
                else {
                    return;
                };
                instance.store(pid, Ordering::SeqCst);
                instance::wait_for_exit(pid);
                std::thread::sleep(Duration::from_secs(1));
                if !finished.load(Ordering::SeqCst) {
                    out::status_line("chatgpt quit; wally is done");
                    // SAFETY: as above.
                    unsafe { libc::_exit(0) };
                }
            });
        }
        let args = vec![
            "-n".to_string(),
            "-W".to_string(),
            "--env".to_string(),
            format!(
                "CODEX_HOME={}",
                session.folders.codex_home.to_string_lossy()
            ),
            "--env".to_string(),
            format!("{CODEX_KEY_VARIABLE}={}", session.shim.auth_token),
            "-a".to_string(),
            app.to_string(),
            "--args".to_string(),
            format!(
                "--user-data-dir={}",
                session.folders.app_data.to_string_lossy()
            ),
        ];
        let status = harness::launch("open", "", &args, options);
        finished.store(true, Ordering::SeqCst);
        session.end();
        status
    }
}

/// The Windows app is a Store package (`OpenAI.Codex`) whose main program
/// has no command-line alias, and starting it through the shell carries no
/// environment or arguments. Started from its package folder with them it
/// runs as a second instance on wally's own folders, the same as on macOS.
///
/// Windows 11 lets wally start that program directly. Windows 10 refuses to
/// start a packaged app's program from outside its package, so there it is
/// started inside the package (`Invoke-CommandInDesktopPackage`), through a
/// launcher file that sets the same environment, and then found by the
/// app-data folder on its command line.
#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::bootstrap::GlobalOptions;
    use crate::commands::cmd_editors::write_private;
    use crate::harness::codex::CODEX_KEY_VARIABLE;
    use crate::io::output as out;

    pub(super) const DOWNLOAD: &str = "https://chatgpt.com/download";

    /// Where Windows records each installed package for the signed-in user,
    /// one subkey per package full name, with its folder in `PackageRootFolder`.
    const PACKAGES: &str = r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// The installed packages' full names, from the registry.
    fn package_names() -> Vec<String> {
        use windows_sys::Win32::System::Registry::{
            RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
        };
        let path = wide(PACKAGES);
        let mut key: HKEY = std::ptr::null_mut();
        // SAFETY: `path` is a NUL-terminated UTF-16 string and `key` a valid
        // out-pointer; the key is closed below.
        if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
            return Vec::new();
        }
        let mut names = Vec::new();
        for index in 0.. {
            let mut buffer = [0u16; 512];
            let mut length = buffer.len() as u32;
            // SAFETY: `key` is open; `buffer` and `length` describe a
            // writable buffer of that many UTF-16 units.
            let status = unsafe {
                RegEnumKeyExW(
                    key,
                    index,
                    buffer.as_mut_ptr(),
                    &mut length,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                break;
            }
            names.push(String::from_utf16_lossy(&buffer[..length as usize]));
        }
        // SAFETY: `key` was opened above and is closed once.
        unsafe { RegCloseKey(key) };
        names
    }

    /// `name`'s install folder, from its `PackageRootFolder` value.
    fn package_root(name: &str) -> Option<PathBuf> {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
        };
        let subkey = wide(&format!("{PACKAGES}\\{name}"));
        let value = wide("PackageRootFolder");
        let mut buffer = [0u16; 1024];
        let mut size = (buffer.len() * 2) as u32;
        // SAFETY: both names are NUL-terminated UTF-16 strings; `buffer` and
        // `size` (in bytes) describe a writable buffer.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut size,
            )
        };
        if status != 0 {
            return None;
        }
        let units = (size as usize / 2).saturating_sub(1);
        Some(PathBuf::from(String::from_utf16_lossy(&buffer[..units])))
    }

    /// The installed app: its program, and the package it belongs to.
    pub(super) struct Installed {
        pub(super) program: PathBuf,
        package: String,
    }

    /// The installed app, or None.
    pub(super) fn app_path() -> Option<Installed> {
        let names = package_names();
        let name = super::newest_package(&names)?;
        let program = package_root(name)?.join("app").join("ChatGPT.exe");
        program.is_file().then(|| Installed {
            program,
            package: name.clone(),
        })
    }

    /// Starts the program directly. Its process id, or the error Windows gave.
    fn start_directly(
        program: &Path,
        app_data: &str,
        codex_home: &Path,
        token: &str,
    ) -> std::io::Result<u32> {
        Command::new(program)
            .arg(format!("--user-data-dir={app_data}"))
            .env("CODEX_HOME", codex_home)
            .env(CODEX_KEY_VARIABLE, token)
            // The app logs to its standard streams; wally's are the person's.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|child| child.id())
    }

    /// Starts the program inside its package, for a Windows that refuses the
    /// direct start. The environment goes through a launcher file wally owns
    /// rather than a command line, which any local process can read; the file
    /// is gone once the app is up. The process id of the app's main process,
    /// or why it could not be started.
    fn start_in_package(
        installed: &Installed,
        app_data: &str,
        codex_home: &Path,
        token: &str,
    ) -> Result<u32, String> {
        let launcher = codex_home.join("wally-launch.cmd");
        let script = super::launcher_script(
            &installed.program.to_string_lossy(),
            app_data,
            &codex_home.to_string_lossy(),
            CODEX_KEY_VARIABLE,
            token,
        )?;
        write_private(&launcher, &script)
            .map_err(|error| format!("could not write the app's launcher ({error})"))?;
        let started = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command"])
            .arg(super::in_package_command(
                &super::package_family(&installed.package),
                &launcher.to_string_lossy(),
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output();
        let found = match started {
            Ok(output) if output.status.success() => {
                find_instance(app_data, Duration::from_secs(30))
            }
            Ok(output) => {
                let _ = std::fs::remove_file(&launcher);
                // PowerShell follows the reason with its own position and
                // category lines; the first line is the one worth reading.
                let reason = String::from_utf8_lossy(&output.stderr);
                return Err(reason.lines().next().unwrap_or("").trim().to_string());
            }
            Err(error) => {
                let _ = std::fs::remove_file(&launcher);
                return Err(error.to_string());
            }
        };
        let _ = std::fs::remove_file(&launcher);
        found.ok_or_else(|| "the app did not come up".to_string())
    }

    /// The app's main process whose command line names `app_data`, waiting up
    /// to `within` for it to appear.
    fn find_instance(app_data: &str, within: Duration) -> Option<u32> {
        let query = format!(
            "Get-CimInstance Win32_Process -Filter \"Name = 'ChatGPT.exe'\" | ForEach-Object {{ \"$($_.ProcessId)`t$($_.CommandLine)\" }}"
        );
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let listing = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &query])
                .stderr(Stdio::null())
                .output()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                .unwrap_or_default();
            if let Some(pid) = super::instance_in_listing(&listing, app_data) {
                return Some(pid);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        None
    }

    /// Whether process `pid` is still running.
    fn is_running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: OpenProcess takes a plain pid; a null handle means it is gone.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: `handle` was just opened and is closed once.
        let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
        unsafe { CloseHandle(handle) };
        running
    }

    /// Closes the instance and every process it started (Electron runs its
    /// windows and the app's Codex as children): first asking, then forcing
    /// what is still there after `grace`. True once it is gone.
    fn quit(pid: u32, grace: Duration) -> bool {
        let pid_text = pid.to_string();
        let taskkill = |force: bool| {
            let mut command = Command::new("taskkill");
            command.args(["/PID", &pid_text, "/T"]);
            if force {
                command.arg("/F");
            }
            let _ = command.output();
        };
        taskkill(false);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if !is_running(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        taskkill(true);
        std::thread::sleep(Duration::from_millis(500));
        !is_running(pid)
    }

    pub(super) fn run(
        installed: &Installed,
        model: &str,
        context_override: Option<i64>,
        options: &GlobalOptions,
    ) -> i32 {
        let Some(session) = super::session::start(model, context_override, options) else {
            return 1;
        };
        let app_data = session.folders.app_data.to_string_lossy().into_owned();
        let codex_home = session.folders.codex_home.clone();
        let token = session.shim.auth_token.clone();
        let started = match start_directly(&installed.program, &app_data, &codex_home, &token) {
            Ok(pid) => Ok(pid),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                start_in_package(installed, &app_data, &codex_home, &token)
            }
            Err(error) => Err(error.to_string()),
        };
        let pid = match started {
            Ok(pid) => pid,
            Err(problem) => {
                out::error_line(&format!("could not start chatgpt ({problem})"));
                session.end();
                return 1;
            }
        };
        // Ctrl-C quits the instance this run started; its exit ends the run.
        let _quit_on_signal = crate::util::interrupt::on_interrupt(move || {
            if !quit(pid, Duration::from_secs(5)) {
                out::error_line("chatgpt did not quit; close it by hand");
            }
            std::process::exit(130);
        });
        while is_running(pid) {
            std::thread::sleep(Duration::from_millis(500));
        }
        out::status_line("chatgpt quit; wally is done");
        session.end();
        0
    }
}

/// The package family name of a full package name: its name and publisher id
/// (`OpenAI.Codex_26.930.3930.0_arm64__2p2nqsd0c76g0` -> `OpenAI.Codex_2p2nqsd0c76g0`).
#[cfg_attr(not(windows), allow(dead_code))]
fn package_family(full_name: &str) -> String {
    let name = full_name.split('_').next().unwrap_or(full_name);
    match full_name.rsplit_once("__") {
        Some((_, publisher)) => format!("{name}_{publisher}"),
        None => full_name.to_string(),
    }
}

/// The PowerShell command that runs `launcher` inside the app's package.
#[cfg_attr(not(windows), allow(dead_code))]
fn in_package_command(family: &str, launcher: &str) -> String {
    let quoted = |text: &str| format!("'{}'", text.replace('\'', "''"));
    format!(
        "Invoke-CommandInDesktopPackage -PackageFamilyName {} -AppId App -Command 'cmd.exe' -Args {}",
        quoted(family),
        quoted(&format!("/d /c \"{launcher}\""))
    )
}

/// The launcher file that starts the app with wally's environment. Refused
/// for a value the command processor would read as syntax: a double quote,
/// a percent sign or a line break.
#[cfg_attr(not(windows), allow(dead_code))]
fn launcher_script(
    program: &str,
    app_data: &str,
    codex_home: &str,
    key_variable: &str,
    token: &str,
) -> Result<String, String> {
    for value in [program, app_data, codex_home, token] {
        if value.contains(['"', '%', '\r', '\n']) {
            return Err(format!("a path cannot be passed to the app: {value}"));
        }
    }
    Ok(format!(
        "@echo off\r\nset \"CODEX_HOME={codex_home}\"\r\nset \"{key_variable}={token}\"\r\nstart \"\" \"{program}\" --user-data-dir=\"{app_data}\"\r\n"
    ))
}

/// The process id in a `<pid>\t<command line>` listing of the app's
/// processes whose command line names `app_data` and is the main process,
/// not one of its helpers (those carry `--type=`).
#[cfg_attr(not(windows), allow(dead_code))]
fn instance_in_listing(listing: &str, app_data: &str) -> Option<u32> {
    let wanted = app_data.replace('/', "\\").to_lowercase();
    listing.lines().find_map(|line| {
        let (pid, command) = line.split_once('\t')?;
        let command = command.replace('/', "\\").to_lowercase();
        (command.contains(&wanted) && !command.contains("--type="))
            .then(|| pid.trim().parse().ok())?
    })
}

/// The package full name of the newest installed ChatGPT app among `names`
/// (`OpenAI.Codex_<version>_<arch>__<publisher>`), by version. Kept free of
/// Windows APIs so it can be tested on any platform.
#[cfg_attr(not(windows), allow(dead_code))]
fn newest_package(names: &[String]) -> Option<&String> {
    let version = |name: &str| -> Option<Vec<u64>> {
        let version = name.strip_prefix("OpenAI.Codex_")?.split('_').next()?;
        version.split('.').map(|part| part.parse().ok()).collect()
    };
    names
        .iter()
        .filter_map(|name| Some((version(name)?, name)))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, name)| name)
}

pub fn register_chatgpt(app: &mut App) {
    let command = app.add_subcommand(COMMAND, "Open ChatGPT's Codex with a model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally chatgpt -m qwen3-4b-instruct-2507",
            "The certified local coding model",
        ),
        Example::new(
            "wally chatgpt -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
    ]));
    command.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    command.add_option(
        "--context-window",
        ValueType::Int64,
        "Tell the app the model holds this many tokens, so it compacts sooner",
    );
    command.callback(|p, g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        if let Some(problem) = context_window_problem(p.get_i64("--context-window")) {
            out::error_line(&problem);
            return 2;
        }
        run(
            &resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color),
            p.get_i64("--context-window"),
            g,
        )
    });
}

#[cfg(target_os = "macos")]
fn run(model: &str, context_override: Option<i64>, options: &GlobalOptions) -> i32 {
    let Some(app) = mac::app_path() else {
        out::error_line("the ChatGPT app is not installed on this machine");
        out::status_line(&format!(
            "download it from {}, then run this again",
            mac::DOWNLOAD
        ));
        return 1;
    };
    if model.is_empty() {
        // No model named means no wiring: the app opens as the reader has it.
        return harness::launch("open", "", &["-a".to_string(), app], options);
    }
    mac::run(&app, model, context_override, options)
}

#[cfg(windows)]
fn run(model: &str, context_override: Option<i64>, options: &GlobalOptions) -> i32 {
    let Some(app) = windows::app_path() else {
        out::error_line("the ChatGPT app is not installed on this machine");
        out::status_line(&format!(
            "download it from {}, then run this again",
            windows::DOWNLOAD
        ));
        return 1;
    };
    if model.is_empty() {
        // No model named means no wiring: the app opens as the reader has it.
        return match std::process::Command::new(&app.program).spawn() {
            Ok(_) => 0,
            Err(error) => {
                out::error_line(&format!("could not start chatgpt ({error})"));
                1
            }
        };
    }
    windows::run(&app, model, context_override, options)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn run(_model: &str, _context_override: Option<i64>, _options: &GlobalOptions) -> i32 {
    out::error_line("wally chatgpt needs the ChatGPT app for macOS or Windows");
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_newest_chatgpt_package_wins_by_version_not_by_text() {
        let installed = names(&[
            "Microsoft.WindowsCalculator_11.2.0.0_arm64__8wekyb3d8bbwe",
            "OpenAI.Codex_26.930.3930.0_arm64__2p2nqsd0c76g0",
            "OpenAI.Codex_26.1001.100.0_arm64__2p2nqsd0c76g0",
        ]);
        assert_eq!(
            newest_package(&installed).map(String::as_str),
            Some("OpenAI.Codex_26.1001.100.0_arm64__2p2nqsd0c76g0")
        );
    }

    #[test]
    fn the_family_name_is_the_name_and_publisher() {
        assert_eq!(
            package_family("OpenAI.Codex_26.930.3930.0_arm64__2p2nqsd0c76g0"),
            "OpenAI.Codex_2p2nqsd0c76g0"
        );
    }

    #[test]
    fn the_in_package_command_quotes_for_powershell() {
        assert_eq!(
            in_package_command("OpenAI.Codex_2p2nqsd0c76g0", r"C:\Users\o'brien\l.cmd"),
            r#"Invoke-CommandInDesktopPackage -PackageFamilyName 'OpenAI.Codex_2p2nqsd0c76g0' -AppId App -Command 'cmd.exe' -Args '/d /c "C:\Users\o''brien\l.cmd"'"#
        );
    }

    #[test]
    fn the_launcher_sets_the_environment_and_starts_the_app() {
        let script = launcher_script(
            r"C:\Apps\ChatGPT.exe",
            r"C:\state\app-data",
            r"C:\state\codex-home",
            "WALLY_CODEX_API_KEY",
            "abc123",
        )
        .unwrap();
        assert!(script.contains("set \"CODEX_HOME=C:\\state\\codex-home\""));
        assert!(script.contains("set \"WALLY_CODEX_API_KEY=abc123\""));
        assert!(script
            .contains(r#"start "" "C:\Apps\ChatGPT.exe" --user-data-dir="C:\state\app-data""#));
        assert!(launcher_script("C:\\a%b", "x", "y", "K", "t").is_err());
    }

    #[test]
    fn the_instance_is_the_main_process_on_our_folder() {
        let listing =
            "10\t\"C:\\Apps\\ChatGPT.exe\" --type=renderer --user-data-dir=C:/state\\app-data\n\
             11\t\"C:\\Apps\\ChatGPT.exe\" --user-data-dir=C:\\other\n\
             12\t\"C:\\Apps\\ChatGPT.exe\" --user-data-dir=C:/state\\app-data\n";
        assert_eq!(instance_in_listing(listing, r"C:\state\app-data"), Some(12));
        assert_eq!(instance_in_listing(listing, r"C:\nowhere"), None);
    }

    #[test]
    fn no_chatgpt_package_means_none() {
        assert_eq!(
            newest_package(&names(&["OpenAI.CodexHelper", "Other_1.0.0.0_x64__x"])),
            None
        );
    }
}
