//! `wally codex`: Codex CLI against a local or hosted model, through the
//! loopback Responses translator (`crate::codex`).
//!
//! Its own command rather than a row in `cmd_editors`' table: that table is the
//! Anthropic-shaped tools, and Codex is wired by command-line overrides, not
//! environment variables.

use std::path::{Path, PathBuf};

use crate::bootstrap::GlobalOptions;
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::codex;
use crate::commands::cmd_editors::{
    cloud_context_window, context_window_for_tool, context_window_problem, text_only_ids,
    write_private, RemovedOnDrop, ScopedEnv,
};
use crate::commands::resolve_default_model;
use crate::config::cli_paths;
use crate::harness::{self, codex as codex_config};
use crate::io::output as out;

const COMMAND: &str = "codex";

/// The model catalog Codex reads at startup, in a file wally owns for this
/// session only. None when there is nowhere to write it; Codex then runs on
/// its fallback metadata, which works but warns.
fn write_model_catalog(catalog: &[harness::CatalogModel]) -> RemovedOnDrop {
    let state_dir = cli_paths::state_dir();
    if state_dir.is_empty() {
        return RemovedOnDrop(None);
    }
    let dir = Path::new(&state_dir).join("codex");
    if let Err(error) = std::fs::create_dir_all(&dir) {
        out::status_line(&format!("could not write Codex's model catalog ({error})"));
        return RemovedOnDrop(None);
    }
    let path = dir.join(format!("wally-models-{}.json", std::process::id()));
    match write_private(&path, &codex_config::codex_model_catalog(catalog)) {
        Ok(()) => RemovedOnDrop(Some(path.to_string_lossy().into_owned())),
        Err(error) => {
            out::status_line(&format!("could not write Codex's model catalog ({error})"));
            RemovedOnDrop(None)
        }
    }
}

/// A Codex home of wally's own: the threads, settings and sign-in of a wally
/// session stay apart from the reader's `~/.codex`, and nothing is copied
/// between the two.
///
/// Codex records every thread, with its provider and model, in its home, and
/// the reader's own Codex (the CLI, the IDE extension, the app) lists them
/// all. A wally thread there names a `wally` provider that exists only while
/// wally runs and a model the reader's Codex cannot serve. Kept apart, there
/// is nothing to undo when a session ends, however it ends -- Ctrl-C, a closed
/// terminal, SIGKILL. Everything wally needs reaches Codex as `-c` overrides,
/// so the home starts empty and Codex fills it (project trust is asked once
/// per folder, as on a first install). None without a usable state directory,
/// in which case the reader's home is used rather than a root-level path.
fn codex_home(state_dir: &str) -> Option<PathBuf> {
    if state_dir.is_empty() {
        return None;
    }
    let home = Path::new(state_dir).join("codex").join("wally-home");
    std::fs::create_dir_all(&home).ok()?;
    Some(home)
}

/// The Codex home the person's own Codex uses: `CODEX_HOME` when they set
/// one, else `~/.codex`. Read before wally sets `CODEX_HOME` for its child.
fn user_codex_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    let profile = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(profile)
        .filter(|h| !h.is_empty())
        .map(|h| Path::new(&h).join(".codex"))
}

/// The person's machine-level Codex settings (`harness::codex::carried_settings`)
/// as `-c` overrides; none when they have no config.
fn carried_overrides(user_home: Option<&Path>) -> Vec<String> {
    let Some(text) =
        user_home.and_then(|home| std::fs::read_to_string(home.join("config.toml")).ok())
    else {
        return Vec::new();
    };
    codex_config::carried_settings(&text)
        .into_iter()
        .flat_map(|setting| ["-c".to_string(), setting])
        .collect()
}

/// The Windows sandbox's one-time setup, which Codex keeps in its home: the
/// setup marker and the sandbox accounts' passwords. The accounts themselves
/// are machine-wide, so a second setup from wally's home would give them new
/// passwords and leave the person's own Codex holding stale ones. wally's home
/// links to the person's setup instead of keeping its own.
#[cfg(windows)]
const SANDBOX_SETUP_DIRS: &[&str] = &[".sandbox", ".sandbox-secrets"];

/// Points wally's home at the person's Windows sandbox setup once they have
/// completed one (its account passwords are on disk). Until then wally's home
/// keeps its own, and a link left from an earlier launch is taken away. What
/// wally's home held there before is its own and is replaced.
#[cfg(windows)]
fn share_sandbox_setup(wally_home: &Path, user_home: &Path) {
    // Run from inside a Codex session wally launched, CODEX_HOME already names
    // wally's home: its setup is then the one in use, and is left as it is.
    let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if same(wally_home, user_home) {
        return;
    }
    let set_up = user_home
        .join(".sandbox-secrets")
        .join("sandbox_users.json")
        .is_file();
    for dir in SANDBOX_SETUP_DIRS {
        let ours = wally_home.join(dir);
        let linked =
            std::fs::symlink_metadata(&ours).is_ok_and(|meta| meta.file_type().is_symlink());
        if !set_up {
            if linked {
                let _ = std::fs::remove_dir(&ours);
            }
            continue;
        }
        let theirs = user_home.join(dir);
        if crate::util::junction::points_to(&ours, &theirs) {
            continue;
        }
        let removed = match std::fs::symlink_metadata(&ours) {
            Err(_) => Ok(()),
            Ok(_) if linked => std::fs::remove_dir(&ours),
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&ours),
            Ok(_) => std::fs::remove_file(&ours),
        };
        if let Err(error) = removed.and_then(|()| crate::util::junction::create(&ours, &theirs)) {
            out::status_line(&format!(
                "could not share your Codex sandbox setup ({dir}: {error}); Codex may ask to set it up again"
            ));
        }
    }
}

/// Holds the translator open and prints what to point at it, for a client
/// that speaks the Responses API but is not Codex.
fn serve(model: &str, options: &GlobalOptions) -> i32 {
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::result_line(&format!("OPENAI_BASE_URL={}", shim.base_url));
    out::result_line(&format!("OPENAI_API_KEY={}", shim.auth_token));
    out::status_line(&format!("serving {model}; press Ctrl-C to stop"));
    // No signal handling: Ctrl-C ends the process, and the OS reclaims the port
    // and the model.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn run(
    model: &str,
    args: &[String],
    context_override: Option<i64>,
    options: &GlobalOptions,
) -> i32 {
    if model.is_empty() {
        // No model named means no wiring to do: Codex runs exactly as the
        // reader has it configured. Same contract as `wally opencode`.
        return harness::launch(COMMAND, "", args, options);
    }
    let Some(endpoint) = harness::resolve(model, options, COMMAND) else {
        return 1;
    };
    // Read once: it names the models Codex lists, their windows, and which of
    // them take no images.
    let catalog = harness::catalog_models_for(&endpoint, model);
    let Some(mut shim) = codex::start(&endpoint, model, options.verbose, &text_only_ids(&catalog))
    else {
        harness::release(&endpoint);
        return 1;
    };
    out::status_line(&format!(
        "codex will talk to {model} through {}",
        shim.base_url
    ));
    let real = if endpoint.serving {
        endpoint.context_window
    } else if catalog[0].context_window > 0 {
        catalog[0].context_window
    } else {
        cloud_context_window(model)
    };
    let context = context_window_for_tool(real, context_override);
    if context > 0 {
        out::status_line(&format!("context window: {context} tokens"));
    }

    // The catalog carries each model's window too, and Codex reads it from
    // there for the launched model; a smaller asked-for window goes in both.
    let mut catalog = catalog;
    if context > 0 {
        catalog[0].context_window = context;
    }
    let catalog_file = write_model_catalog(&catalog);
    let user_home = user_codex_home();
    let mut overrides = carried_overrides(user_home.as_deref());
    overrides.extend(codex_config::codex_overrides(
        &shim.base_url,
        catalog_file.0.as_deref(),
        context,
    ));
    // The loopback token, never the upstream key, and only for this child.
    let codex_home = codex_home(&cli_paths::state_dir());
    #[cfg(windows)]
    if let (Some(ours), Some(theirs)) = (&codex_home, &user_home) {
        share_sandbox_setup(ours, theirs);
    }
    let status = {
        let _key = ScopedEnv::new(codex_config::CODEX_KEY_VARIABLE, &shim.auth_token);
        let _home = codex_home
            .as_ref()
            .map(|home| ScopedEnv::new("CODEX_HOME", &home.to_string_lossy()));
        harness::launch(
            COMMAND,
            "",
            &codex_config::codex_argv(&overrides, model, args),
            options,
        )
    };

    codex::stop(&mut shim);
    harness::release(&endpoint);
    status
}

pub fn register_codex(app: &mut App) {
    let command = app.add_subcommand(COMMAND, "Open Codex with a model");
    command.footer(&examples_footer(&[
        Example::new(
            "wally codex -m qwen3-4b-instruct-2507",
            "The certified local coding model",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash",
            "A hosted model (needs `wally account login`)",
        ),
        Example::new(
            "wally codex -m glm-5.3-flash -- exec \"explain this project\"",
            "Pass arguments through to Codex after --",
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
        "Tell Codex the model holds this many tokens, so it compacts sooner",
    );
    command.add_flag(
        "--serve",
        "Print the endpoint and keep it open instead of launching",
    );
    // Tokens after the wally flags belong to Codex, its own flags included;
    // `run()` inserts the `--` that lets them through (SplitPassthroughArgv).
    command
        .add_option("args", ValueType::Text, "Passed through to codex")
        .multi();
    command.prefix_command(true);
    command.callback(|p, g| {
        if harness::refuse_decisions_model(p.get_str("--model").as_deref()) {
            return 2;
        }
        if let Some(problem) = context_window_problem(p.get_i64("--context-window")) {
            out::error_line(&problem);
            return 2;
        }
        // A missing tool shows only that it is missing and how to get it,
        // before any model resolution. --serve launches nothing.
        if !p.flag("--serve") && !harness::ensure_installed(COMMAND) {
            return 127;
        }
        let effective =
            resolve_default_model(&p.get_str("--model").unwrap_or_default(), g.no_color);
        if p.flag("--serve") {
            if effective.is_empty() {
                out::error_line(
                    "--serve requires --model <id>, and no default is set (wally models default <id>)",
                );
                return 2;
            }
            serve(&effective, g)
        } else {
            run(
                &effective,
                &p.get_strs("args"),
                p.get_i64("--context-window"),
                g,
            )
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_home_is_wallys_own_and_starts_empty() {
        let state = tempfile::tempdir().unwrap();
        let home = codex_home(&state.path().to_string_lossy()).unwrap();
        assert!(home.starts_with(state.path()));
        assert!(home.is_dir());
        assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
    }

    #[cfg(windows)]
    #[test]
    fn a_completed_sandbox_setup_is_shared_and_an_unfinished_one_is_not() {
        let state = tempfile::tempdir().unwrap();
        let ours = codex_home(&state.path().to_string_lossy()).unwrap();
        let theirs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(theirs.path().join(".sandbox")).unwrap();
        std::fs::create_dir_all(ours.join(".sandbox-secrets")).unwrap();
        std::fs::write(ours.join(".sandbox-secrets").join("stale.json"), "{}").unwrap();

        // Not set up yet: wally's home keeps its own.
        share_sandbox_setup(&ours, theirs.path());
        assert!(!crate::util::junction::points_to(
            &ours.join(".sandbox"),
            &theirs.path().join(".sandbox")
        ));

        let secrets = theirs.path().join(".sandbox-secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("sandbox_users.json"), "{\"version\":5}").unwrap();
        share_sandbox_setup(&ours, theirs.path());
        for dir in SANDBOX_SETUP_DIRS {
            assert!(
                crate::util::junction::points_to(&ours.join(dir), &theirs.path().join(dir)),
                "{dir}"
            );
        }
        assert!(ours
            .join(".sandbox-secrets")
            .join("sandbox_users.json")
            .is_file());
        assert!(!secrets.join("stale.json").exists());

        // Launched from inside wally's own Codex: nothing is touched.
        share_sandbox_setup(&ours, &ours);
        assert!(ours
            .join(".sandbox-secrets")
            .join("sandbox_users.json")
            .is_file());

        // Their setup gone: the links go, their folders stay.
        std::fs::remove_file(secrets.join("sandbox_users.json")).unwrap();
        share_sandbox_setup(&ours, theirs.path());
        assert!(!ours.join(".sandbox").exists());
        assert!(theirs.path().join(".sandbox").is_dir());
    }

    #[test]
    fn no_state_directory_means_no_home_of_our_own() {
        assert_eq!(codex_home(""), None);
    }
}
