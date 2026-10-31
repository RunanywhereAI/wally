//! `wally browser-use [eve | -m eve] --start-url URL "<goal>"`: a browser agent
//! that does a web task and stops at the payment page.
//!
//! This command reads the flags, picks the gateway and its key, and hands both
//! to `crate::browser::run`. Everything that decides what the agent may click
//! or type lives in `crate::browser`'s guards, in code. The key is never put on
//! a command line and never printed.

use crate::account::{self, ConsoleClient};
use crate::browser::chrome::{loopback_devtools_url, ChromeMode};
use crate::browser::gateway::Gateway;
use crate::browser::{self, Options};
use crate::cli::{App, ValueType};
use crate::cli_formatter::{examples_footer, Example};
use crate::harness;
use crate::io::output as out;

const DEFAULT_TEXT_MODEL: &str = "deepseek-v4.1-flash-fast";
const DEFAULT_MAX_STEPS: u64 = 60;
/// TypeSafe's decision model. No RunAnywhere route serves it; it answers on a
/// gateway named by `WALLY_BROWSER_API_BASE`.
const JEV_MODEL: &str = "jev-latest";

fn is_browser_decision_model(id: &str) -> bool {
    harness::is_decisions_model(id) || id == JEV_MODEL
}

/// The flags as given, before any session, network or Chrome check.
#[derive(Debug, Default)]
pub struct Flags<'a> {
    pub words: &'a [String],
    pub model: Option<&'a str>,
    pub text_model: Option<&'a str>,
    pub chrome: Option<&'a str>,
    pub cdp_url: Option<&'a str>,
    pub start_url: Option<&'a str>,
    pub max_steps: Option<u64>,
}

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub decision_model: String,
    pub text_model: String,
    pub goal: String,
    pub chrome: ChromeMode,
    pub start_url: String,
    pub max_steps: u64,
}

/// Reads the flags into a Request. A leading word that is a decision model id
/// (`wally browser-use eve "book …"`) is the model; every other word is the goal.
pub fn parse_request(flags: &Flags) -> Result<Request, String> {
    let mut words: Vec<String> = flags.words.to_vec();
    let mut decision_model = flags.model.map(str::to_string);
    if words.len() > 1 && is_browser_decision_model(&words[0]) {
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
    if !is_browser_decision_model(&decision_model) {
        return Err(format!(
            "{decision_model} is not a decision model; browser-use needs one to choose each step \
             (default {})",
            harness::DEFAULT_DECISIONS_MODEL
        ));
    }
    let text_model = flags.text_model.unwrap_or(DEFAULT_TEXT_MODEL).to_string();
    if is_browser_decision_model(&text_model) {
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
            "say what to do, e.g. wally browser-use --start-url https://www.google.com/travel/flights \
             eve \"find a flight to Bangalore\""
                .into(),
        );
    }
    let start_url = match flags.start_url {
        Some(url) if url.starts_with("https://") || url.starts_with("http://") => url.to_string(),
        Some(url) => return Err(format!("--start-url must be an http(s) URL, not {url}")),
        None => return Err("--start-url is required: the page the task starts on".into()),
    };
    let chrome = match (flags.chrome.unwrap_or("dedicated"), flags.cdp_url) {
        ("dedicated", None) => ChromeMode::Dedicated,
        ("dedicated", Some(_)) => return Err("--cdp-url goes with --chrome attach".into()),
        ("attach", Some(url)) => ChromeMode::Attach(loopback_devtools_url(url)?),
        ("attach", None) => ChromeMode::Personal,
        (other, _) => return Err(format!("--chrome must be dedicated or attach, not {other}")),
    };
    Ok(Request {
        decision_model,
        text_model,
        goal,
        chrome,
        start_url,
        max_steps: flags.max_steps.unwrap_or(DEFAULT_MAX_STEPS),
    })
}

/// Another gateway, when `WALLY_BROWSER_API_BASE` and `WALLY_BROWSER_API_KEY`
/// are both set: its own key, so no Wally session is needed or checked.
fn gateway_override() -> Option<Gateway> {
    let base = crate::util::getenv("WALLY_BROWSER_API_BASE")?;
    let key = crate::util::getenv("WALLY_BROWSER_API_KEY")?;
    Some(Gateway::fixed(&base, key))
}

/// The signed-in Wally session, checked once before the run starts.
fn hosted_session(request: &Request) -> Option<Gateway> {
    let mut credentials = match account::load() {
        Ok(credentials) => credentials,
        Err(message) => {
            out::error_line(&message);
            return None;
        }
    };
    if !credentials.signed_in() {
        harness::report_not_signed_in();
        return None;
    }
    if let Err(error) = harness::verify_cloud_session(&ConsoleClient::default(), &mut credentials) {
        if !error.unverified {
            harness::report_cloud_session_invalid(&request.decision_model);
            return None;
        }
        // The console could not be asked right now; the gateway will answer for the key.
    }
    Some(Gateway::session(credentials))
}

fn run(request: Request) -> i32 {
    let gateway = match gateway_override() {
        Some(gateway) => gateway,
        None => match hosted_session(&request) {
            Some(gateway) => gateway,
            None => return 1,
        },
    };
    out::status_line(&format!(
        "browser-use: {} chooses each step, {} writes text; the run stops at the payment page",
        request.decision_model, request.text_model
    ));
    browser::run(
        Options {
            goal: request.goal,
            start_url: request.start_url,
            decision_model: request.decision_model,
            text_model: request.text_model,
            max_steps: request.max_steps as usize,
            chrome: request.chrome,
        },
        gateway,
    )
}

pub fn register_browser_use(app: &mut App) {
    let cmd = app.add_subcommand(
        "browser-use",
        "Do a web task in a browser with eve, stopping at the payment page",
    );
    cmd.footer(&examples_footer(&[
        Example::new(
            "wally browser-use --start-url https://en.wikipedia.org eve \"Open the Wikipedia article about Bengaluru\"",
            "no payment page involved",
        ),
        Example::new(
            "wally browser-use --start-url https://www.google.com/travel/flights eve \"Find one-way flights from Bengaluru to Delhi on 2026-11-20, nonstop only\"",
            "a search with filters; it stops when the results show",
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
        "Chat model that writes field text (default deepseek-v4.1-flash-fast)",
    );
    cmd.add_option(
        "--start-url",
        ValueType::Text,
        "The page the task starts on (required)",
    );
    cmd.add_option(
        "--chrome",
        ValueType::Text,
        "dedicated (a Wally browser profile, default) or attach (your own Chrome, with remote debugging on at chrome://inspect)",
    );
    cmd.add_option(
        "--cdp-url",
        ValueType::Text,
        "With --chrome attach: a Chrome started with --remote-debugging-port, e.g. http://127.0.0.1:9222",
    );
    cmd.add_option(
        "--max-steps",
        ValueType::UInt,
        "Most actions to take (default 60)",
    )
    .int_bounds(1, 500);
    cmd.add_option("words", ValueType::Text, "[eve] and what to do")
        .multi();
    cmd.callback(|p, _options| {
        let words = p.get_strs("words");
        let model = p.get_str("--model");
        let text_model = p.get_str("--text-model");
        let chrome = p.get_str("--chrome");
        let cdp_url = p.get_str("--cdp-url");
        let start_url = p.get_str("--start-url");
        let request = match parse_request(&Flags {
            words: &words,
            model: model.as_deref(),
            text_model: text_model.as_deref(),
            chrome: chrome.as_deref(),
            cdp_url: cdp_url.as_deref(),
            start_url: start_url.as_deref(),
            max_steps: p.get_u64("--max-steps"),
        }) {
            Ok(request) => request,
            Err(message) => {
                out::error_line(&message);
                return 2;
            }
        };
        run(request)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: Option<&str> = Some("https://en.wikipedia.org");

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| w.to_string()).collect()
    }

    fn parse(list: &[&str], adjust: impl FnOnce(&mut Flags)) -> Result<Request, String> {
        let words = words(list);
        let mut flags = Flags {
            words: &words,
            start_url: URL,
            ..Flags::default()
        };
        adjust(&mut flags);
        parse_request(&flags)
    }

    #[test]
    fn a_leading_decision_model_is_the_model_and_the_rest_is_the_goal() {
        let request = parse(&["eve", "find", "a", "flight"], |_| {}).unwrap();
        assert_eq!(request.decision_model, "eve");
        assert_eq!(request.goal, "find a flight");
        assert_eq!(request.text_model, "deepseek-v4.1-flash-fast");
        assert_eq!(request.chrome, ChromeMode::Dedicated);
        assert_eq!(request.max_steps, 60);
    }

    #[test]
    fn jev_latest_is_accepted_as_the_decision_model() {
        let request = parse(&["jev-latest", "book"], |_| {}).unwrap();
        assert_eq!(request.decision_model, "jev-latest");
        assert_eq!(request.goal, "book");
    }

    #[test]
    fn without_a_model_eve_is_the_default_and_every_word_is_the_goal() {
        let request = parse(&["book a flight"], |_| {}).unwrap();
        assert_eq!(request.decision_model, "eve");
        assert_eq!(request.goal, "book a flight");
    }

    #[test]
    fn a_single_word_goal_named_like_a_model_stays_the_goal() {
        // "eve" alone has nothing after it to be a goal, so it is the goal.
        assert_eq!(parse(&["eve"], |_| {}).unwrap().goal, "eve");
    }

    #[test]
    fn a_chat_model_cannot_choose_the_steps() {
        let error = parse(&["book"], |f| f.model = Some("glm-5.3-flash")).unwrap_err();
        assert!(error.contains("is not a decision model"), "{error}");
    }

    #[test]
    fn a_decision_model_cannot_write_text() {
        let error = parse(&["book"], |f| f.text_model = Some("eve")).unwrap_err();
        assert!(error.contains("needs a chat model"), "{error}");
    }

    #[test]
    fn two_different_decision_models_are_refused() {
        let error = parse(&["eve", "book"], |f| f.model = Some("qwev")).unwrap_err();
        assert!(error.contains("two decision models"), "{error}");
    }

    #[test]
    fn an_empty_goal_is_refused() {
        assert!(parse(&[], |_| {}).is_err());
    }

    #[test]
    fn an_unknown_chrome_mode_is_refused() {
        assert!(parse(&["book"], |f| f.chrome = Some("default-profile")).is_err());
    }

    #[test]
    fn the_start_url_is_required() {
        let error = parse(&["book"], |f| f.start_url = None).unwrap_err();
        assert!(error.contains("--start-url is required"), "{error}");
    }

    #[test]
    fn a_start_url_that_is_not_http_is_refused() {
        assert!(parse(&["book"], |f| f.start_url = Some("file:///etc/passwd")).is_err());
    }

    #[test]
    fn a_remote_cdp_url_is_a_usage_error() {
        let error = parse(&["book"], |f| {
            f.chrome = Some("attach");
            f.cdp_url = Some("http://example.com:9222");
        })
        .unwrap_err();
        assert!(error.contains("on this machine"), "{error}");
    }

    #[test]
    fn a_loopback_cdp_url_attaches() {
        let request = parse(&["book"], |f| {
            f.chrome = Some("attach");
            f.cdp_url = Some("http://127.0.0.1:9222");
        })
        .unwrap();
        assert_eq!(
            request.chrome,
            ChromeMode::Attach("http://127.0.0.1:9222".into())
        );
    }

    #[test]
    fn a_cdp_url_without_attach_is_a_usage_error() {
        let error = parse(&["book"], |f| f.cdp_url = Some("http://127.0.0.1:9222")).unwrap_err();
        assert!(error.contains("--chrome attach"), "{error}");
    }

    #[test]
    fn attach_without_a_cdp_url_uses_the_persons_own_chrome() {
        let request = parse(&["book"], |f| f.chrome = Some("attach")).unwrap();
        assert_eq!(request.chrome, ChromeMode::Personal);
    }

    #[test]
    fn max_steps_is_carried() {
        assert_eq!(
            parse(&["book"], |f| f.max_steps = Some(3))
                .unwrap()
                .max_steps,
            3
        );
    }
}
