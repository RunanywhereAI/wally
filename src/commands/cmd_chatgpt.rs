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

#[cfg(target_os = "macos")]
mod mac {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::bootstrap::GlobalOptions;
    use crate::codex;
    use crate::commands::cmd_editors::{
        cloud_context_window, context_window_for_tool, write_private,
    };
    use crate::config::cli_paths;
    use crate::desktop::instance;
    use crate::harness::chatgpt::{
        chatgpt_auth, chatgpt_config, chatgpt_settings, with_all_efforts_enabled,
        with_onboarding_done, CHATGPT_BUNDLE_ID,
    };
    use crate::harness::codex::{codex_model_catalog, CODEX_KEY_VARIABLE};
    use crate::harness::{self, CatalogModel};
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

    /// Where the app's Codex home and its own app data live: wally's state
    /// directory, kept between runs so threads and project trust carry over.
    struct Folders {
        codex_home: PathBuf,
        app_data: PathBuf,
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
        if let Some(done) = with_onboarding_done(&state, crate::codex::translate::now_unix())
            .and_then(|state| with_all_efforts_enabled(&state))
        {
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

    pub(super) fn run(
        app: &str,
        model: &str,
        context_override: Option<i64>,
        options: &GlobalOptions,
    ) -> i32 {
        let Some(folders) = folders() else {
            out::error_line("could not find a state directory for the app's data");
            return 1;
        };
        let Some(endpoint) = harness::resolve(model, options, super::COMMAND) else {
            return 1;
        };
        let catalog = harness::catalog_models_for(&endpoint, model);
        let Some(mut shim) = codex::start(&endpoint, model, options.verbose, &catalog) else {
            harness::release(&endpoint);
            return 1;
        };
        let real = if endpoint.serving {
            endpoint.context_window
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
        if let Err(problem) = prepare(&folders, model, &shim.base_url, &catalog, context) {
            out::error_line(&problem);
            codex::stop(&mut shim);
            harness::release(&endpoint);
            return 1;
        }
        out::status_line(&format!(
            "chatgpt will talk to {model} through {}",
            shim.base_url
        ));

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
            format!("CODEX_HOME={}", folders.codex_home.to_string_lossy()),
            "--env".to_string(),
            format!("{CODEX_KEY_VARIABLE}={}", shim.auth_token),
            "-a".to_string(),
            app.to_string(),
            "--args".to_string(),
            format!("--user-data-dir={}", folders.app_data.to_string_lossy()),
        ];
        let status = harness::launch("open", "", &args, options);
        finished.store(true, Ordering::SeqCst);
        codex::stop(&mut shim);
        harness::release(&endpoint);
        status
    }
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

#[cfg(not(target_os = "macos"))]
fn run(_model: &str, _context_override: Option<i64>, _options: &GlobalOptions) -> i32 {
    out::error_line("wally chatgpt needs the ChatGPT app for macOS");
    1
}
