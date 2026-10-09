//! `wally browser-use [eve | -m eve] "<goal>"`: a browser agent that does a web
//! task and stops at the payment page.
//!
//! Wally's part is small: pick the models, check the hosted session, and run
//! the locked Python package in `browser/` (`wally_browser`) with `uv run
//! --frozen`. Everything that decides what the agent may click or type lives in
//! that package's guards, in code. The session key goes to the child in its
//! environment, never on its command line, and is never printed.

use std::path::{Path, PathBuf};

use crate::account::{self, ConsoleClient};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::harness;
use crate::io::output as out;

const DEFAULT_TEXT_MODEL: &str = "glm-5.3-flash";
const DEFAULT_MAX_STEPS: u64 = 60;
/// Where an install keeps the locked Python project, relative to the binary's
/// directory. A checkout points `WALLY_BROWSER_PROJECT` at its `browser/`.
const PROJECT_RELATIVE: &[&str] = &["../share/wally/browser", "../browser"];

/// What the command line asked for, before any session or filesystem check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub decision_model: String,
    pub text_model: String,
    pub goal: String,
    pub profile: Option<String>,
    pub chrome: String,
    pub cdp_url: Option<String>,
    pub start_url: Option<String>,
    pub max_steps: u64,
    pub clean_output: bool,
}

/// Reads the words and options into a Request. A leading word that is a
/// decision model id (`wally browser-use eve "book …"`) is the model; every
/// other word is the goal.
pub fn parse_request(
    words: &[String],
    model: Option<&str>,
    text_model: Option<&str>,
    chrome: Option<&str>,
) -> Result<Request, String> {
    let mut words: Vec<String> = words.to_vec();
    let mut decision_model = model.map(str::to_string);
    if words.len() > 1 && harness::is_decisions_model(&words[0]) {
        let named = words.remove(0);
        if let Some(flag) = &decision_model {
            if flag != &named {
                return Err(format!("two decision models given: {named} and -m {flag}"));
            }
        }
        decision_model = Some(named);
    }
    let decision_model =
        decision_model.unwrap_or_else(|| harness::DEFAULT_DECISIONS_MODEL.to_string());
    if !harness::is_decisions_model(&decision_model) {
        return Err(format!(
            "{decision_model} is not a decision model; browser-use needs one to choose each step \
             (default {})",
            harness::DEFAULT_DECISIONS_MODEL
        ));
    }
    let text_model = text_model.unwrap_or(DEFAULT_TEXT_MODEL).to_string();
    if harness::is_decisions_model(&text_model) {
        return Err(format!(
            "{text_model} is a decision model; --text-model needs a chat model (default {DEFAULT_TEXT_MODEL})"
        ));
    }
    for id in [&decision_model, &text_model] {
        if !harness::model_id_is_safe(id) {
            return Err(format!("'{id}' is not a valid model id"));
        }
    }
    let goal = words.join(" ").trim().to_string();
    if goal.is_empty() {
        return Err(
            "say what to do, e.g. wally browser-use eve \"find a flight to Bangalore\"".into(),
        );
    }
    let chrome = chrome.unwrap_or("dedicated").to_string();
    if chrome != "dedicated" && chrome != "attach" {
        return Err(format!(
            "--chrome must be dedicated or attach, not {chrome}"
        ));
    }
    Ok(Request {
        decision_model,
        text_model,
        goal,
        profile: None,
        chrome,
        cdp_url: None,
        start_url: None,
        max_steps: DEFAULT_MAX_STEPS,
        clean_output: false,
    })
}

/// The child's environment: the hosted endpoint and key, the models, and the
/// options. The key is here and only here.
pub fn child_environment(
    request: &Request,
    api_base: &str,
    api_key: &str,
    deadline: i64,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("WALLY_BROWSER_API_BASE".to_string(), api_base.to_string()),
        ("WALLY_BROWSER_API_KEY".to_string(), api_key.to_string()),
        (
            "WALLY_BROWSER_DECISION_MODEL".to_string(),
            request.decision_model.clone(),
        ),
        (
            "WALLY_BROWSER_TEXT_MODEL".to_string(),
            request.text_model.clone(),
        ),
        ("WALLY_BROWSER_CHROME".to_string(), request.chrome.clone()),
        (
            "WALLY_BROWSER_MAX_STEPS".to_string(),
            request.max_steps.to_string(),
        ),
        ("WALLY_BROWSER_DEADLINE".to_string(), deadline.to_string()),
        // browser-use reads these at import; the package forces them too.
        ("ANONYMIZED_TELEMETRY".to_string(), "false".to_string()),
        ("BROWSER_USE_CLOUD_SYNC".to_string(), "false".to_string()),
    ];
    if request.clean_output {
        env.push(("WALLY_BROWSER_CLEAN".to_string(), "1".to_string()));
    }
    if let Some(profile) = &request.profile {
        env.push(("WALLY_BROWSER_PROFILE".to_string(), profile.clone()));
    }
    if let Some(cdp_url) = &request.cdp_url {
        env.push(("WALLY_BROWSER_CDP_URL".to_string(), cdp_url.clone()));
    }
    env
}

/// The child's argv after the `uv` program: the goal is the only free text.
pub fn child_arguments(request: &Request, project: &Path) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--frozen".to_string(),
        "--quiet".to_string(),
        "--project".to_string(),
        project.to_string_lossy().into_owned(),
        "python".to_string(),
        "-m".to_string(),
        "wally_browser".to_string(),
    ];
    if let Some(url) = &request.start_url {
        args.push("--start-url".to_string());
        args.push(url.clone());
    }
    args.push("--".to_string());
    args.push(request.goal.clone());
    args
}

/// The locked project: `WALLY_BROWSER_PROJECT`, else next to the binary.
fn find_project() -> Option<PathBuf> {
    if let Some(dir) = crate::util::getenv("WALLY_BROWSER_PROJECT") {
        let dir = PathBuf::from(dir);
        return dir.join("pyproject.toml").is_file().then_some(dir);
    }
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let bin = exe.parent()?;
    PROJECT_RELATIVE
        .iter()
        .map(|relative| bin.join(relative))
        .find(|dir| dir.join("pyproject.toml").is_file() && dir.join("uv.lock").is_file())
}

fn find_on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{tool}.exe"), tool.to_string()]
    } else {
        vec![tool.to_string()]
    };
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn run(request: Request) -> i32 {
    let Some(project) = find_project() else {
        out::error_line(
            "the browser agent is not in this install; set WALLY_BROWSER_PROJECT to a wally \
             checkout's browser/ directory",
        );
        return 1;
    };
    let Some(uv) = find_on_path("uv") else {
        out::error_line(
            "browser-use needs uv (https://docs.astral.sh/uv/) to run its locked Python package; \
             install it, then run this again",
        );
        return 127;
    };

    let mut credentials = match account::load() {
        Ok(credentials) => credentials,
        Err(message) => {
            out::error_line(&message);
            return 1;
        }
    };
    if !credentials.signed_in() {
        harness::report_not_signed_in();
        return 1;
    }
    if let Err(error) = harness::verify_cloud_session(&ConsoleClient::default(), &mut credentials) {
        if !error.unverified {
            harness::report_cloud_session_invalid(&request.decision_model);
            return 1;
        }
        // The console could not be asked right now; the gateway will answer for the key.
    }
    let api_base = format!("{}/v1", credentials.console_url);
    out::status_line(&format!(
        "browser-use: {} chooses each step, {} writes text; the run stops at the payment page",
        request.decision_model, request.text_model
    ));

    let mut command = std::process::Command::new(&uv);
    command
        .args(child_arguments(&request, &project))
        .envs(child_environment(
            &request,
            &api_base,
            &credentials.access_token,
            credentials.expires_at,
        ));
    // Stay in the foreground terminal job. A new process group is stopped by
    // the kernel (SIGTTIN) the moment Python reads a confirmation, and Chrome,
    // in that same group, freezes with it.
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            out::error_line(&format!("could not start uv: {error}"));
            return 1;
        }
    };
    let interrupted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&interrupted);
    // The child receives Ctrl-C in the foreground job and handles its own
    // shutdown, leaving its dedicated browser open.
    let _interrupt = crate::util::interrupt::on_interrupt(move || {
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let status = child.wait();
    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
        return 130;
    }
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(error) => {
            out::error_line(&format!("browser-use ended without a status: {error}"));
            1
        }
    }
}

pub fn register_browser_use(app: &mut App) {
    let cmd = app.add_subcommand(
        "browser-use",
        "Do a web task in a browser with eve, stopping at the payment page",
    );
    cmd.footer(&examples_footer(&[
        Example::new(
            "wally browser-use --start-url \"https://docs.oracle.com/en/java/javase/21/docs/api/index.html\" eve \"Open the Java 21 docs\"",
            "start at a URL; no payment page involved",
        ),
        Example::new(
            "wally browser-use eve \"Open the Wikipedia article about Bengaluru\"",
            "no URL given; the text model picks the site, or asks for one",
        ),
    ]));
    cmd.add_option(
        "-m,--model",
        ValueType::Text,
        "Decision model that chooses each step (default eve)",
    );
    cmd.add_option(
        "--text-model",
        ValueType::Text,
        "Chat model for the plan and field text (default glm-5.3-flash)",
    );
    cmd.add_option(
        "--profile",
        ValueType::Text,
        "Traveller profile file (default ~/.config/wally/traveller.toml)",
    );
    cmd.add_option(
        "--chrome",
        ValueType::Text,
        "dedicated (a Wally browser profile, default) or attach (your running Chrome)",
    );
    cmd.add_option(
        "--cdp-url",
        ValueType::Text,
        "With --chrome attach: the running Chrome's DevTools URL",
    );
    cmd.add_option("--start-url", ValueType::Text, "Open this page first");
    cmd.add_flag(
        "--clean-output",
        "Print the plan and one line per step, without browser-use's own log",
    );
    cmd.add_option(
        "--max-steps",
        ValueType::UInt,
        "Most steps to take (default 60)",
    )
    .int_bounds(1, 500);
    cmd.add_option("words", ValueType::Text, "[eve] and what to do")
        .multi();
    cmd.callback(|p, _options| {
        let mut request = match parse_request(
            &p.get_strs("words"),
            p.get_str("--model").as_deref(),
            p.get_str("--text-model").as_deref(),
            p.get_str("--chrome").as_deref(),
        ) {
            Ok(request) => request,
            Err(message) => {
                out::error_line(&message);
                return 2;
            }
        };
        request.profile = p.get_str("--profile");
        request.cdp_url = p.get_str("--cdp-url");
        request.start_url = p.get_str("--start-url");
        request.clean_output = p.flag("--clean-output");
        if let Some(steps) = p.get_u64("--max-steps") {
            request.max_steps = steps;
        }
        if request.cdp_url.is_some() && request.chrome != "attach" {
            out::error_line("--cdp-url goes with --chrome attach");
            return 2;
        }
        run(request)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_leading_decision_model_is_the_model_and_the_rest_is_the_goal() {
        let request =
            parse_request(&words(&["eve", "find", "a", "flight"]), None, None, None).unwrap();
        assert_eq!(request.decision_model, "eve");
        assert_eq!(request.goal, "find a flight");
        assert_eq!(request.text_model, "glm-5.3-flash");
        assert_eq!(request.chrome, "dedicated");
    }

    #[test]
    fn without_a_model_eve_is_the_default_and_every_word_is_the_goal() {
        let request = parse_request(&words(&["book a flight"]), None, None, None).unwrap();
        assert_eq!(request.decision_model, "eve");
        assert_eq!(request.goal, "book a flight");
    }

    #[test]
    fn a_single_word_goal_named_like_a_model_stays_the_goal() {
        // "eve" alone has nothing after it to be a goal, so it is the goal.
        let request = parse_request(&words(&["eve"]), None, None, None).unwrap();
        assert_eq!(request.goal, "eve");
    }

    #[test]
    fn a_chat_model_cannot_choose_the_steps() {
        let error =
            parse_request(&words(&["book"]), Some("glm-5.3-flash"), None, None).unwrap_err();
        assert!(error.contains("is not a decision model"), "{error}");
    }

    #[test]
    fn a_decision_model_cannot_write_text() {
        let error = parse_request(&words(&["book"]), None, Some("eve"), None).unwrap_err();
        assert!(error.contains("needs a chat model"), "{error}");
    }

    #[test]
    fn two_different_decision_models_are_refused() {
        let error = parse_request(&words(&["eve", "book"]), Some("qwev"), None, None).unwrap_err();
        assert!(error.contains("two decision models"), "{error}");
    }

    #[test]
    fn an_empty_goal_and_an_unknown_chrome_mode_are_refused() {
        assert!(parse_request(&[], None, None, None).is_err());
        assert!(parse_request(&words(&["book"]), None, None, Some("default-profile")).is_err());
    }

    #[test]
    fn the_key_goes_in_the_environment_never_the_arguments() {
        let request = parse_request(&words(&["eve", "book a flight"]), None, None, None).unwrap();
        let env = child_environment(
            &request,
            "https://inference.example/v1",
            "secret-key",
            1_700_000_000,
        );
        let args = child_arguments(&request, Path::new("/opt/wally/browser"));
        assert!(env.contains(&("WALLY_BROWSER_API_KEY".into(), "secret-key".into())));
        assert!(args.iter().all(|a| !a.contains("secret-key")));
        assert!(env.contains(&("ANONYMIZED_TELEMETRY".into(), "false".into())));
        assert_eq!(args[..2], ["run".to_string(), "--frozen".to_string()]);
        assert_eq!(args.last().unwrap(), "book a flight");
        assert_eq!(args[args.len() - 2], "--");
    }
}
