//! `wally command-code`: Command Code (npm `command-code`) against a local or
//! hosted model.
//!
//! Command Code reads custom providers only from
//! `~/.commandcode/providers.json`. No variable or flag moves that file, and
//! Command Code reloads it live while it runs. So a launch merges a
//! `runanywhere-wally` provider into the person's own file and takes it out
//! again when the tool exits, holding a lock for the whole run so two
//! launches cannot swap providers under each other.

use std::fs::{File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::bootstrap::GlobalOptions;
use crate::io::json::dump_pretty;
use crate::io::output as out;

use super::agents::{key_or_placeholder, os_home, Agent, ScopedEnv};
use super::catalog_models::{catalog_models_for, CatalogModel};
use super::harness::{launch as launch_tool, release, resolve};

/// Our own provider id. Not `runanywhere`: a provider of that name may be the
/// person's own, added through `/connect`, and is never touched.
pub const PROVIDER_ID: &str = "runanywhere-wally";

/// The variable `providers.json` names for the key. Command Code refuses a
/// raw secret in that file and resolves `$VAR` from its own environment.
/// Private to wally, so a person's own provider that reads
/// `RUNANYWHERE_API_KEY` keeps its key during the run.
pub const KEY_VARIABLE: &str = "WALLY_COMMAND_CODE_API_KEY";

/// What Command Code reads as its own sign-in (`getCommandAuthKey`).
const AUTH_VARIABLE: &str = "COMMAND_CODE_API_KEY";

/// Stands in for a Command Code sign-in the person does not have. It only
/// gets past the startup check: `CMD_LOCAL_ONLY` goes with it, so it is never
/// sent anywhere.
const AUTH_PLACEHOLDER: &str = "wally-local";

const LOCK_FILE: &str = "wally-command-code.lock";

/// The reasoning levels Command Code takes in `reasoningEfforts`; it drops
/// any other name.
const LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Command Code's own subcommands (`cmd --help`, 1.87.0). Run as given, with
/// no provider and no injected flags, so `-- status` reports the person's real
/// sign-in. `acp` is not here: it is an agent session and gets the provider.
const OWN_SUBCOMMANDS: &[&str] = &[
    "info",
    "status",
    "help",
    "whoami",
    "update",
    "feedback",
    "issue",
    "taste",
    "learn-taste",
    "mcp",
    "cloud",
    "skills",
    "mods",
    "login",
    "logout",
];

/// Whether `args` asks for one of Command Code's own subcommands. Only the
/// first token counts: anything else would mean guessing which flags take a
/// value.
pub fn is_own_subcommand(args: &[String]) -> bool {
    args.first()
        .is_some_and(|first| OWN_SUBCOMMANDS.contains(&first.as_str()))
}

/// Command Code's home, `<home>/.commandcode`. It reads `HOME` (then
/// `USERPROFILE`) and takes no override.
pub fn command_code_dir() -> Option<PathBuf> {
    os_home().map(|home| PathBuf::from(home).join(".commandcode"))
}

/// Whether Command Code would count the person as signed in, decided the
/// way its `getCommandAuthKey` does: a non-empty `COMMAND_CODE_API_KEY`, or
/// else a non-empty top-level `apiKey` in `auth.json`. Only presence is
/// looked at; the value is never kept.
pub fn signed_in(environment_key: Option<&str>, auth_json: Option<&str>) -> bool {
    if environment_key.is_some_and(|key| !key.trim().is_empty()) {
        return true;
    }
    auth_json
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|auth| {
            auth.get("apiKey")
                .and_then(Value::as_str)
                .map(|key| !key.is_empty())
        })
        .unwrap_or(false)
}

/// The person's `providers.json`, ready to merge into: the parsed document
/// with any `runanywhere-wally` left by a run that never cleaned up dropped,
/// and whether a `provider` object was there to begin with.
#[derive(Debug, PartialEq)]
pub struct ExistingProviders {
    pub document: Value,
    pub had_provider: bool,
    pub dropped_leftover: bool,
}

pub fn parse_existing(existing: Option<&str>) -> Result<ExistingProviders, String> {
    let mut document: Value = match existing {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(text)
            .map_err(|err| format!("command-code providers.json: {err}"))?,
        _ => json!({}),
    };
    if !document.is_object() {
        return Err("command-code providers.json is not an object".to_string());
    }
    let had_provider = match document.get("provider") {
        None => false,
        Some(provider) if provider.is_object() => true,
        Some(_) => {
            return Err("command-code providers.json: \"provider\" is not an object".to_string())
        }
    };
    let dropped_leftover = document
        .get_mut("provider")
        .and_then(Value::as_object_mut)
        .is_some_and(|providers| providers.remove(PROVIDER_ID).is_some());
    Ok(ExistingProviders {
        document,
        had_provider,
        dropped_leftover,
    })
}

/// One catalog model as a Command Code model entry.
fn model_entry(model: &CatalogModel) -> Value {
    let mut entry = json!({ "name": model.id });
    if model.context_window > 0 {
        entry["contextWindow"] = json!(model.context_window);
    }
    if model.max_output > 0 {
        entry["maxOutput"] = json!(model.max_output);
    }
    // No levels Command Code can name means no reasoning field at all:
    // `reasoning: true` would offer low / medium / high, which the endpoint
    // refuses for a model that lacks them.
    if let Some(reasoning) = &model.reasoning {
        let levels: Vec<&str> = reasoning
            .levels
            .iter()
            .copied()
            .filter(|level| LEVELS.contains(level))
            .collect();
        if !levels.is_empty() {
            entry["reasoningEfforts"] = json!(levels);
        }
    }
    if model.input_per_mtok > 0 || model.output_per_mtok > 0 {
        // Divided, not multiplied by 1e-6: 400000 * 1e-6 is 0.39999999999999997.
        let per_million = |micro: i64| micro as f64 / 1_000_000.0;
        entry["cost"] = json!({
            "input": per_million(model.input_per_mtok),
            "output": per_million(model.output_per_mtok),
            "cacheRead": per_million(model.cached_input_per_mtok),
        });
    }
    entry
}

/// `document` with our provider added.
pub fn with_our_provider(mut document: Value, base_url: &str, models: &[CatalogModel]) -> Value {
    let entries: serde_json::Map<String, Value> = models
        .iter()
        .map(|model| (model.id.clone(), model_entry(model)))
        .collect();
    if document.get("provider").is_none() {
        document["provider"] = json!({});
    }
    document["provider"][PROVIDER_ID] = json!({
        "name": "RunAnywhere",
        "api": "openai-completions",
        "baseURL": base_url,
        "apiKey": format!("${KEY_VARIABLE}"),
        "models": entries,
    });
    document
}

/// What the file looked like before the run, and what the run wrote.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// None when there was no file.
    pub original: Option<Vec<u8>>,
    pub had_provider: bool,
    pub written: Vec<u8>,
}

#[derive(Debug, PartialEq)]
pub enum Cleanup {
    Write(Vec<u8>),
    Remove,
    Nothing,
    /// The file no longer parses; it is the person's to fix.
    Unparseable,
}

/// How to put the file back, given what is in it now (None: gone).
///
/// Untouched since the run wrote it: the original bytes go back verbatim, or
/// the file goes when there was none. Edited during the run (Command Code's
/// `/connect` writes it): only our provider comes out, and the `provider`
/// object too if we created it and it is now empty, so their edits survive.
pub fn plan_cleanup(snapshot: &Snapshot, current: Option<&[u8]>) -> Cleanup {
    let Some(current) = current else {
        return Cleanup::Nothing;
    };
    if current == snapshot.written.as_slice() {
        return match &snapshot.original {
            Some(original) => Cleanup::Write(original.clone()),
            None => Cleanup::Remove,
        };
    }
    let Ok(mut document) = serde_json::from_slice::<Value>(current) else {
        return Cleanup::Unparseable;
    };
    let Some(providers) = document.get_mut("provider").and_then(Value::as_object_mut) else {
        return Cleanup::Nothing;
    };
    if providers.remove(PROVIDER_ID).is_none() {
        return Cleanup::Nothing;
    }
    let provider_now_empty = providers.is_empty();
    if provider_now_empty && !snapshot.had_provider {
        if let Some(root) = document.as_object_mut() {
            root.remove("provider");
        }
    }
    if snapshot.original.is_none() && document.as_object().is_some_and(|root| root.is_empty()) {
        return Cleanup::Remove;
    }
    Cleanup::Write(dump_pretty(&document, 2).into_bytes())
}

/// Puts `providers.json` back when the launch ends, however it ends short of
/// the process being killed outright.
struct ProvidersGuard {
    path: PathBuf,
    snapshot: Snapshot,
}

impl Drop for ProvidersGuard {
    fn drop(&mut self) {
        let current = match std::fs::read(&self.path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(_) => {
                report_manual_cleanup(&self.path);
                return;
            }
        };
        let done = match plan_cleanup(&self.snapshot, current.as_deref()) {
            Cleanup::Write(bytes) => std::fs::write(&self.path, bytes).is_ok(),
            Cleanup::Remove => std::fs::remove_file(&self.path).is_ok(),
            Cleanup::Nothing => true,
            Cleanup::Unparseable => false,
        };
        if !done {
            report_manual_cleanup(&self.path);
        }
    }
}

fn report_manual_cleanup(path: &Path) {
    out::error_line(&format!(
        "could not take the \"{PROVIDER_ID}\" provider out of {}; remove it by hand",
        path.display()
    ));
}

/// Holds `~/.commandcode/wally-command-code.lock` for the run. Command Code
/// never reads that file, so even Windows' mandatory lock on it blocks
/// nothing; the OS drops the lock when the process ends, however it ends.
fn take_run_lock(dir: &Path) -> Result<File, String> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(LOCK_FILE))
        .map_err(|err| format!("could not open the command-code run lock: {err}"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            Err("another `wally command-code` is running; one at a time".to_string())
        }
        Err(TryLockError::Error(err)) => {
            Err(format!("could not lock the command-code run lock: {err}"))
        }
    }
}

/// `--skip-onboarding --no-auto-update --model runanywhere-wally/<model>`,
/// then their own arguments. No auto-update, so the binary is not swapped
/// out from under a run.
pub fn command_code_argv(model: &str, child_args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "--skip-onboarding".to_string(),
        "--no-auto-update".to_string(),
        "--model".to_string(),
        format!("{PROVIDER_ID}/{model}"),
    ];
    argv.extend(child_args.iter().cloned());
    argv
}

pub fn launch(agent: &Agent, model: &str, args: &[String], options: &GlobalOptions) -> i32 {
    if model.is_empty() || is_own_subcommand(args) {
        return launch_tool(agent.command, "", args, options);
    }
    let Some(dir) = command_code_dir() else {
        out::error_line("could not find a home directory for command-code");
        return 1;
    };
    if let Err(err) = std::fs::create_dir_all(&dir) {
        out::error_line(&format!("could not create {}: {err}", dir.display()));
        return 1;
    }
    // Before resolving, so a refused run never loads a model.
    let _lock = match take_run_lock(&dir) {
        Ok(lock) => lock,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };

    let Some(endpoint) = resolve(model, options, agent.id) else {
        return 1;
    };
    let status = launch_with_endpoint(agent, model, args, options, &dir, &endpoint);
    release(&endpoint);
    status
}

fn launch_with_endpoint(
    agent: &Agent,
    model: &str,
    args: &[String],
    options: &GlobalOptions,
    dir: &Path,
    endpoint: &super::harness::Endpoint,
) -> i32 {
    // After resolve: a model pull there installs and then clears its own
    // interrupt action, which would take an earlier one with it. The action
    // only notes the interrupt. The child gets the same signal from the
    // terminal and wally waits for it to exit, then cleans up.
    let interrupted = Arc::new(AtomicBool::new(false));
    let noted = Arc::clone(&interrupted);
    let _interrupt = crate::util::interrupt::on_interrupt(move || {
        noted.store(true, Ordering::SeqCst);
    });

    let catalog = catalog_models_for(endpoint, model);
    if catalog[0].context_window > 0 {
        out::status_line(&format!(
            "context window: {} tokens",
            catalog[0].context_window
        ));
    }

    let path = dir.join("providers.json");
    let raw = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            out::error_line(&format!("could not read {}: {err}", path.display()));
            return 1;
        }
    };
    let text = match raw.as_deref().map(std::str::from_utf8).transpose() {
        Ok(text) => text,
        Err(_) => {
            out::error_line(&format!("{} is not UTF-8", path.display()));
            return 1;
        }
    };
    let existing = match parse_existing(text) {
        Ok(existing) => existing,
        Err(failure) => {
            out::error_line(&failure);
            return 1;
        }
    };
    // A leftover from a run that never cleaned up is not put back.
    let original = match (&raw, existing.dropped_leftover) {
        (Some(_), true) => Some(dump_pretty(&existing.document, 2).into_bytes()),
        _ => raw.clone(),
    };
    let written = dump_pretty(
        &with_our_provider(existing.document, &endpoint.base_url, &catalog),
        2,
    )
    .into_bytes();
    // Before the write, so a write that fails halfway is still undone.
    let _restore = ProvidersGuard {
        path: path.clone(),
        snapshot: Snapshot {
            original,
            had_provider: existing.had_provider,
            written: written.clone(),
        },
    };
    if let Err(err) = std::fs::write(&path, &written) {
        out::error_line(&format!("could not write {}: {err}", path.display()));
        return 1;
    }

    let key = ScopedEnv::new(KEY_VARIABLE, key_or_placeholder(&endpoint.api_key));
    let auth_json = std::fs::read_to_string(dir.join("auth.json")).ok();
    let environment_key = std::env::var(AUTH_VARIABLE).ok();
    let is_signed_in = signed_in(environment_key.as_deref(), auth_json.as_deref());
    // Local-only for a model on this machine, and whenever the placeholder
    // sign-in is in play, so it never reaches Command Code's backend.
    let placeholder = (!is_signed_in).then(|| ScopedEnv::new(AUTH_VARIABLE, AUTH_PLACEHOLDER));
    let local_only =
        (endpoint.serving || !is_signed_in).then(|| ScopedEnv::new("CMD_LOCAL_ONLY", "1"));
    let all_applied = key.applied()
        && placeholder.as_ref().is_none_or(ScopedEnv::applied)
        && local_only.as_ref().is_none_or(ScopedEnv::applied);
    if !all_applied {
        out::error_line(&format!("could not set the endpoint for {}", agent.id));
        return 1;
    }

    if interrupted.load(Ordering::SeqCst) {
        return 130;
    }
    out::status_line(&format!(
        "{} will talk to {model} through {}",
        agent.id, endpoint.base_url
    ));
    launch_tool(agent.command, "", &command_code_argv(model, args), options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::ReasoningSupport;

    fn priced(id: &str) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            context_window: 200_000,
            max_output: 32_000,
            input_per_mtok: 400_000,
            output_per_mtok: 1_600_000,
            cached_input_per_mtok: 40_000,
            ..Default::default()
        }
    }

    fn reasoning(id: &str, levels: &[&'static str]) -> CatalogModel {
        CatalogModel {
            id: id.to_string(),
            reasoning: Some(ReasoningSupport {
                levels: levels.to_vec(),
                default_level: levels.first().copied(),
                thinking_off: true,
            }),
            ..Default::default()
        }
    }

    fn snapshot(original: Option<&str>, had_provider: bool, written: &str) -> Snapshot {
        Snapshot {
            original: original.map(|text| text.as_bytes().to_vec()),
            had_provider,
            written: written.as_bytes().to_vec(),
        }
    }

    #[test]
    fn our_provider_names_the_key_variable_and_keeps_theirs() {
        let existing = parse_existing(Some(
            r#"{"provider":{"runanywhere":{"baseURL":"http://mine/v1","apiKey":"$RUNANYWHERE_API_KEY","models":{}}}}"#,
        ))
        .unwrap();
        let built = with_our_provider(
            existing.document,
            "https://api.example/v1",
            &[priced("glm-5.3-flash")],
        );
        assert_eq!(
            built["provider"]["runanywhere"]["baseURL"],
            "http://mine/v1"
        );
        let ours = &built["provider"][PROVIDER_ID];
        assert_eq!(ours["api"], "openai-completions");
        assert_eq!(ours["baseURL"], "https://api.example/v1");
        assert_eq!(ours["apiKey"], "$WALLY_COMMAND_CODE_API_KEY");
    }

    #[test]
    fn model_entries_carry_exact_limits_and_prices() {
        let built = with_our_provider(json!({}), "http://x/v1", &[priced("glm-5.3-flash")]);
        let model = &built["provider"][PROVIDER_ID]["models"]["glm-5.3-flash"];
        assert_eq!(model["contextWindow"], 200_000);
        assert_eq!(model["maxOutput"], 32_000);
        assert_eq!(model["cost"]["input"], json!(0.4));
        assert_eq!(model["cost"]["output"], json!(1.6));
        assert_eq!(model["cost"]["cacheRead"], json!(0.04));
    }

    #[test]
    fn only_levels_command_code_names_are_offered() {
        let built = with_our_provider(
            json!({}),
            "http://x/v1",
            &[
                reasoning("mimo", &["minimal", "low", "high", "max"]),
                reasoning("minimal-only", &["minimal"]),
                reasoning("switch", &[]),
            ],
        );
        let models = &built["provider"][PROVIDER_ID]["models"];
        assert_eq!(
            models["mimo"]["reasoningEfforts"],
            json!(["low", "high", "max"])
        );
        for id in ["minimal-only", "switch"] {
            assert!(models[id].get("reasoningEfforts").is_none(), "{id}");
            assert!(models[id].get("reasoning").is_none(), "{id}");
        }
    }

    #[test]
    fn a_leftover_from_an_earlier_run_is_dropped() {
        let existing = parse_existing(Some(
            r#"{"provider":{"runanywhere-wally":{"baseURL":"http://old/v1","models":{}},"ollama":{"baseURL":"http://localhost:11434/v1","models":{}}}}"#,
        ))
        .unwrap();
        assert!(existing.dropped_leftover);
        assert!(existing.document["provider"].get(PROVIDER_ID).is_none());
        assert!(existing.document["provider"].get("ollama").is_some());
    }

    #[test]
    fn documents_that_cannot_be_merged_into_are_refused() {
        for text in ["[]", r#"{"provider":[]}"#, "{not json"] {
            assert!(parse_existing(Some(text)).is_err(), "{text}");
        }
    }

    #[test]
    fn a_missing_or_empty_file_merges_into_an_empty_document() {
        for text in [None, Some(""), Some("  \n")] {
            let existing = parse_existing(text).unwrap();
            assert_eq!(existing.document, json!({}));
            assert!(!existing.had_provider);
        }
    }

    #[test]
    fn an_untouched_file_gets_its_original_bytes_back() {
        let snap = snapshot(Some("{ \"theirs\" : 1 }\n"), false, "WRITTEN");
        assert_eq!(
            plan_cleanup(&snap, Some(b"WRITTEN")),
            Cleanup::Write(b"{ \"theirs\" : 1 }\n".to_vec())
        );
    }

    #[test]
    fn an_untouched_file_that_was_absent_is_removed() {
        let snap = snapshot(None, false, "WRITTEN");
        assert_eq!(plan_cleanup(&snap, Some(b"WRITTEN")), Cleanup::Remove);
    }

    #[test]
    fn an_edited_file_loses_only_our_provider() {
        let snap = snapshot(Some("{}"), false, "WRITTEN");
        let edited = br#"{"provider":{"runanywhere-wally":{},"groq":{"baseURL":"https://api.groq.com/openai/v1"}}}"#;
        let Cleanup::Write(bytes) = plan_cleanup(&snap, Some(edited)) else {
            panic!("expected a write");
        };
        let after: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(after["provider"].get(PROVIDER_ID).is_none());
        assert!(after["provider"].get("groq").is_some());
    }

    #[test]
    fn an_edited_file_drops_a_provider_object_we_created_once_empty() {
        let snap = snapshot(Some(r#"{"theme":"dark"}"#), false, "WRITTEN");
        let edited = br#"{"theme":"light","provider":{"runanywhere-wally":{}}}"#;
        let Cleanup::Write(bytes) = plan_cleanup(&snap, Some(edited)) else {
            panic!("expected a write");
        };
        let after: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(after, json!({"theme": "light"}));
    }

    #[test]
    fn an_edited_file_that_was_absent_and_ends_empty_is_removed() {
        let snap = snapshot(None, false, "WRITTEN");
        assert_eq!(
            plan_cleanup(&snap, Some(br#"{"provider":{"runanywhere-wally":{}}}"#)),
            Cleanup::Remove
        );
    }

    #[test]
    fn a_file_that_no_longer_parses_is_left_alone() {
        let snap = snapshot(Some("{}"), false, "WRITTEN");
        assert_eq!(plan_cleanup(&snap, Some(b"{broken")), Cleanup::Unparseable);
    }

    #[test]
    fn a_file_deleted_during_the_run_stays_deleted() {
        let snap = snapshot(Some("{}"), false, "WRITTEN");
        assert_eq!(plan_cleanup(&snap, None), Cleanup::Nothing);
    }

    #[test]
    fn a_second_run_lock_is_refused_while_the_first_is_held() {
        let dir = std::env::temp_dir().join(format!("wally-cc-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = take_run_lock(&dir).unwrap();
        let second = take_run_lock(&dir).unwrap_err();
        assert!(second.contains("one at a time"), "{second}");
        drop(first);
        assert!(take_run_lock(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn signed_in_matches_command_codes_own_check() {
        assert!(signed_in(Some("ck_live"), None));
        assert!(!signed_in(Some("  "), None));
        assert!(signed_in(
            None,
            Some(r#"{"apiKey":"ck_live","userName":"x"}"#)
        ));
        assert!(!signed_in(None, Some(r#"{"apiKey":""}"#)));
        assert!(!signed_in(None, Some(r#"{"openrouter":{"apiKey":"sk"}}"#)));
        assert!(!signed_in(None, Some("not json")));
        assert!(!signed_in(None, None));
    }

    #[test]
    fn own_subcommands_run_untouched_and_acp_does_not() {
        let args = |tokens: &[&str]| tokens.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert!(is_own_subcommand(&args(&["status"])));
        assert!(is_own_subcommand(&args(&["login", "openai"])));
        assert!(!is_own_subcommand(&args(&["acp"])));
        assert!(!is_own_subcommand(&args(&["-p", "status"])));
        assert!(!is_own_subcommand(&args(&[])));
    }

    #[test]
    fn argv_skips_setup_and_names_the_model_before_theirs() {
        assert_eq!(
            command_code_argv("qwen3-4b", &["-p".to_string(), "hi".to_string()]),
            [
                "--skip-onboarding",
                "--no-auto-update",
                "--model",
                "runanywhere-wally/qwen3-4b",
                "-p",
                "hi"
            ]
        );
    }
}
