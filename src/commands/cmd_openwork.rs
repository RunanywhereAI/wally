//! `wally openwork`: the OpenWork desktop app with every RunAnywhere model in
//! its picker, on a private profile of wally's (see harness::openwork).

use std::path::Path;

use crate::account::{self, ConsoleClient, IdentityResult};
use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::commands::cmd_editors::write_private;
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::harness::{
    self, account_models_with, build_openwork_config, catalog_models_for, CatalogModel, Endpoint,
    OpenWorkApp, Profile, Providers,
};
use crate::io::output as out;

const COMMAND: &str = "openwork";
const CONFIG_VARIABLE: &str = "OPENCODE_CONFIG_CONTENT";

pub fn register_openwork(app: &mut App) {
    let command = app.add_subcommand(COMMAND, "Open OpenWork with a local or cloud model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally openwork -m qwen3-4b-instruct-2507",
            "The certified local model, with your hosted models beside it",
        ),
        Example::new(
            "wally openwork -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
    ]));
    command.add_option(
        "-m,--model",
        ValueType::Text,
        "A model on this machine, or a hosted one from your account",
    );
    command.callback(|p, g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        // Before resolving a model: a person without the app should see only
        // that it is missing and how to get it.
        let Some(app) = installed_app() else {
            return 127;
        };
        run(
            &app,
            &resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color),
            g,
        )
    });
}

/// The installed OpenWork, or None after saying how to get it (and, on macOS
/// at a terminal with Homebrew, offering to install it).
fn installed_app() -> Option<OpenWorkApp> {
    if let Some(app) = harness::locate_app() {
        return Some(app);
    }
    if let Some(path) =
        crate::util::getenv(harness::APP_OVERRIDE_VARIABLE).filter(|p| !p.is_empty())
    {
        out::error_line(&format!(
            "{} names {path}, which is not an OpenWork app",
            harness::APP_OVERRIDE_VARIABLE
        ));
        return None;
    }
    out::error_line("OpenWork is not installed on this machine");
    if cfg!(target_os = "macos")
        && harness::on_path("brew")
        && crate::util::term::stdin_is_tty()
        && crate::util::term::stderr_is_tty()
        && harness::confirm(&format!(
            "Install it now with `{}`? [y/N] ",
            harness::BREW_INSTALL
        ))
    {
        let installed = std::process::Command::new("bash")
            .args(["-c", harness::BREW_INSTALL])
            .status()
            .is_ok_and(|status| status.success());
        if !installed {
            out::error_line("the OpenWork install did not finish");
            return None;
        }
        return harness::locate_app().or_else(|| {
            out::error_line("OpenWork installed, but wally could not find OpenWork.app");
            None
        });
    }
    if cfg!(target_os = "macos") {
        out::status_line(&format!(
            "install it with `{}`, or from {}, then run this again",
            harness::BREW_INSTALL,
            harness::DOWNLOAD
        ));
    } else {
        out::status_line(&format!(
            "download it from {}, then run this again",
            harness::DOWNLOAD
        ));
    }
    None
}

/// The account's hosted catalog for a local launch, so it shows beside the
/// local model, behind the same image gate a hosted launch gets. An expired
/// token is refreshed first, as for a hosted launch. None when signed out,
/// when the session is refused, or when the list cannot be read: a local
/// launch never fails for want of the network.
fn hosted_catalog() -> Option<(String, String, Vec<CatalogModel>)> {
    let mut credentials = account::load().ok().filter(|c| c.signed_in())?;
    let console = ConsoleClient::default();
    if let Err(error) = harness::verify_cloud_session(&console, &mut credentials) {
        if !error.unverified {
            return None;
        }
    }
    let (result, models, _) = account_models_with(
        &console,
        &credentials.console_url,
        &credentials.access_token,
    );
    if !matches!(result, IdentityResult::Ok) || models.is_empty() {
        return None;
    }
    let base_url = harness::image_gate::start(&credentials.console_url, &credentials.access_token)
        .unwrap_or_else(|| format!("{}/v1", credentials.console_url));
    Some((base_url, credentials.access_token, models))
}

/// The provider config for a launch on `endpoint`: everything the person can
/// use, with `model` selected.
fn openwork_config(endpoint: &Endpoint, model: &str) -> String {
    let launched = catalog_models_for(endpoint, model);
    if endpoint.serving {
        let hosted = hosted_catalog();
        let providers = Providers {
            local: Some((&endpoint.base_url, &launched)),
            hosted: hosted
                .as_ref()
                .map(|(url, key, models)| (url.as_str(), key.as_str(), models.as_slice())),
        };
        if let Some((_, _, models)) = &hosted {
            out::status_line(&format!(
                "your {} hosted models are listed too",
                models.len()
            ));
        }
        return build_openwork_config(model, &providers);
    }
    out::status_line(&format!("{} models in the picker", launched.len()));
    build_openwork_config(
        model,
        &Providers {
            local: None,
            hosted: Some((&endpoint.base_url, &endpoint.api_key, &launched)),
        },
    )
}

/// Readies the private profile and, before its first launch, its workspace.
fn prepare(profile: &Profile) -> Result<(), String> {
    profile.create()?;
    let list = profile.workspace_list();
    if !list.exists() {
        write_private(&list, &harness::workspace_list(&profile.workspace_path()))
            .map_err(|error| format!("could not write OpenWork's workspace list ({error})"))?;
    }
    Ok(())
}

fn run(app: &OpenWorkApp, model: &str, options: &GlobalOptions) -> i32 {
    let state = cli_paths::state_dir();
    if state.is_empty() {
        out::error_line("could not find a state directory for OpenWork's profile");
        return 1;
    }
    let profile = Profile::under(Path::new(&state));
    if let Err(problem) = prepare(&profile) {
        out::error_line(&problem);
        return 1;
    }
    let mut environment = profile.environment();
    let endpoint = if model.is_empty() {
        // No model named and no default set: the app opens on the profile
        // with no RunAnywhere provider wired.
        None
    } else {
        let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
            return 1;
        };
        environment.push((
            CONFIG_VARIABLE.to_string(),
            openwork_config(&endpoint, model),
        ));
        Some(endpoint)
    };
    out::status_line(&format!(
        "opening OpenWork on wally's profile in {}",
        profile.root.to_string_lossy()
    ));
    let log = profile.root.join("openwork.log");
    let status = start(app, &environment, &log);
    if status != 0 {
        out::status_line(&format!(
            "OpenWork exited with {status}; its output is in {}",
            log.to_string_lossy()
        ));
    }
    if let Some(endpoint) = &endpoint {
        harness::release(endpoint);
    }
    status
}

/// Starts `app` on `environment` and waits for it. Only the child gets the
/// environment; this process's own is left as it was. The app's console
/// output goes to `log`, so it does not flood the terminal and a failed
/// start can still be diagnosed.
fn start(app: &OpenWorkApp, environment: &[(String, String)], log: &Path) -> i32 {
    if matches!(app, OpenWorkApp::Bundle(_)) && !cfg!(target_os = "macos") {
        out::error_line("an OpenWork .app bundle only runs on macOS");
        return 1;
    }
    let output = match std::fs::File::create(log) {
        Ok(file) => file,
        Err(error) => {
            out::error_line(&format!(
                "could not create {} ({error})",
                log.to_string_lossy()
            ));
            return 1;
        }
    };
    let errors = match output.try_clone() {
        Ok(file) => file,
        Err(error) => {
            out::error_line(&format!("could not open OpenWork's log ({error})"));
            return 1;
        }
    };
    // Ctrl-C reaches the app from the terminal and it quits; wally waits for
    // that, so the local server outlives it.
    let _wait = crate::util::interrupt::on_interrupt(|| {});
    match std::process::Command::new(harness::program(app))
        .envs(environment.iter().map(|(k, v)| (k, v)))
        .stdin(std::process::Stdio::null())
        .stdout(output)
        .stderr(errors)
        .status()
    {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            out::error_line(&format!("could not start OpenWork ({error})"));
            127
        }
    }
}
