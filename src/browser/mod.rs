//! `wally browser-use`: a decision model chooses each step of a web task in
//! Chrome, code executes it, and the run stops before any payment.
//!
//! The loop is Jev Ultrafast's (github.com/browser-use/jev-ultrafast), ported
//! from its Python: observe the page, ask one `/systemone` question set for the
//! next operation and its target, check the guards, act, observe again. The
//! model only ever picks among controls the snapshot observed, and a decision
//! made on a page that has since changed is thrown away rather than executed.

pub mod cdp;
pub mod chrome;
pub mod decide;
pub mod gateway;
pub mod guards;
pub mod json;
pub mod page;
pub mod prompts;
pub mod text;
pub mod words;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use self::chrome::ChromeMode;
use self::decide::{
    chosen_target, decision_request, final_choice, final_name, finalist_question, option_cap,
    validate_choice, TargetPick,
};
use self::gateway::Gateway;
use self::json::Json;
use self::page::{Action, ActionKind, Browser, CdpBrowser, Observation};
use self::text::{
    chat_request, field_context, goal_string_question, goal_strings, parse_field_value,
    resolve_field_value, FieldValue, Resolved,
};
use crate::io::output as out;

/// Why a step did not complete. `Stale` is retried; the rest end the run.
#[derive(Debug, Clone, PartialEq)]
pub enum StepError {
    Stale,
    Browser(String),
    Model(String),
    Budget(String),
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Stale => f.write_str("the page kept changing; no action executed"),
            StepError::Browser(message)
            | StepError::Model(message)
            | StepError::Budget(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ready,
    Predicted,
    Done,
    Blocked,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::Predicted => "predicted",
            Status::Done => "done",
            Status::Blocked => "blocked",
        }
    }
}

/// One executed action, as later questions and the step line see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Step {
    pub action: String,
    pub kind: ActionKind,
    pub node: Option<u64>,
    pub text: Option<String>,
    pub text_model: Option<String>,
    pub text_latency_ms: u128,
    pub latency_ms: u128,
    pub page_changed: Option<bool>,
    pub url: String,
}

impl Step {
    pub fn kind_name(&self) -> &'static str {
        match self.kind {
            ActionKind::Click => "click",
            ActionKind::Fill => "fill",
            ActionKind::Select => "select",
            ActionKind::Scroll => "scroll",
            ActionKind::Wait => "wait",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// An observed action id, or DONE / BLOCKED.
    pub choice: String,
    pub operation: String,
    pub target: Option<String>,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub latency_ms: u128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextHelper {
    pub model: String,
    pub latency_ms: u128,
}

/// The two model calls. Hosted in a run, scripted in tests.
pub trait Models {
    fn choose(
        &mut self,
        page: &Observation,
        goal: &str,
        history: &[Step],
    ) -> Result<Decision, String>;
    fn field_text(&mut self, context: &Json) -> Result<(String, TextHelper), String>;
}

/// Drops a target already used twice in the last six actions. Google Flights
/// redraws its suggestion list on every click, so the page fingerprint changes
/// and the unchanged-page stop never fires. Scrolling and waiting are meant to
/// repeat (a long page takes many scrolls), so they are never dropped; a
/// scroll that goes nowhere leaves the page unchanged and that stop ends it.
pub fn suppress_repeated(actions: &[Action], history: &[Step]) -> Vec<Action> {
    let repeats = |kind: ActionKind| !matches!(kind, ActionKind::Scroll | ActionKind::Wait);
    let recent = &history[history.len().saturating_sub(6)..];
    let mut banned: Vec<&str> = Vec::new();
    for step in recent.iter().filter(|step| repeats(step.kind)) {
        let count = recent
            .iter()
            .filter(|other| other.action == step.action && other.text == step.text)
            .count();
        if count >= 2 && !banned.contains(&step.action.as_str()) {
            banned.push(&step.action);
        }
    }
    let kept: Vec<Action> = actions
        .iter()
        .filter(|action| !repeats(action.kind) || !banned.contains(&action.label.as_str()))
        .cloned()
        .collect();
    if kept.is_empty() {
        actions.to_vec()
    } else {
        kept
    }
}

pub struct Agent<B: Browser, M: Models> {
    pub browser: B,
    models: M,
    goal: String,
    pub page: Observation,
    pub history: Vec<Step>,
    pub status: Status,
    decision: Option<Decision>,
    decisions: usize,
    max_steps: usize,
    pending_text: Option<(String, String, TextHelper)>,
    started: Option<Instant>,
}

impl<B: Browser, M: Models> Agent<B, M> {
    pub fn new(mut browser: B, models: M, goal: &str, max_steps: usize) -> Result<Self, StepError> {
        let page = match browser.observe() {
            Ok(page) => page,
            Err(error) => {
                browser.close();
                return Err(error);
            }
        };
        Ok(Self {
            browser,
            models,
            goal: goal.trim().to_string(),
            page,
            history: Vec::new(),
            status: Status::Ready,
            decision: None,
            decisions: 0,
            max_steps,
            pending_text: None,
            started: None,
        })
    }

    pub fn elapsed_ms(&self) -> u128 {
        self.started
            .map_or(0, |started| started.elapsed().as_millis())
    }

    pub fn finished(&self) -> bool {
        matches!(self.status, Status::Done | Status::Blocked)
    }

    /// One decision and, unless it was stale, one action.
    pub fn tick(&mut self) -> Result<(), StepError> {
        match self.predict().and_then(|()| self.act()) {
            Err(StepError::Stale) => {
                self.decision = None;
                self.status = Status::Ready;
                self.page = self.browser.observe()?;
                Ok(())
            }
            other => other,
        }
    }

    fn predict(&mut self) -> Result<(), StepError> {
        self.started.get_or_insert_with(Instant::now);
        if !self.browser.fresh(&self.page, None)? {
            self.page = self.browser.observe()?;
        }
        self.decision = None;
        if self.decisions >= self.max_steps * 2 {
            return Err(StepError::Budget(format!(
                "Reached the model-call budget ({} decisions)",
                self.max_steps * 2
            )));
        }
        let offered = Observation {
            actions: suppress_repeated(&self.page.actions, &self.history),
            ..self.page.clone()
        };
        let decision = self
            .models
            .choose(&offered, &self.goal, &self.history)
            .map_err(StepError::Model)?;
        self.decisions += 1;
        self.decision = Some(decision);
        self.status = Status::Predicted;
        Ok(())
    }

    fn act(&mut self) -> Result<(), StepError> {
        // Consumed before anything else runs, so a retry cannot click twice.
        let Some(decision) = self.decision.take() else {
            return Err(StepError::Browser("act without a decision".into()));
        };
        let page = self.page.clone();
        if decision.choice == "DONE" || decision.choice == "BLOCKED" {
            if !self.browser.fresh(&page, None)? {
                self.status = Status::Ready;
                return Err(StepError::Stale);
            }
            self.status = if decision.choice == "DONE" {
                Status::Done
            } else {
                Status::Blocked
            };
            return Ok(());
        }
        let action = page.action(&decision.choice).cloned().ok_or_else(|| {
            StepError::Model(format!(
                "the decision named no observed action: {}",
                decision.choice
            ))
        })?;
        if guards::refuses(&action, &page.text) {
            self.status = Status::Blocked;
            return Ok(());
        }
        if self.history.len() >= self.max_steps {
            self.status = Status::Blocked;
            return Err(StepError::Budget(format!(
                "Stopped at the {}-action budget",
                self.max_steps
            )));
        }
        let mut text = None;
        let mut helper = None;
        if action.kind == ActionKind::Fill {
            if !self.browser.fresh(&page, None)? {
                return Err(StepError::Stale);
            }
            let context = field_context(&self.goal, &action, &page, &self.history);
            let key = context.to_python();
            let (value, used) = match &self.pending_text {
                Some((pending, value, used)) if *pending == key => (value.clone(), used.clone()),
                _ => {
                    let (value, used) =
                        self.models.field_text(&context).map_err(StepError::Model)?;
                    self.pending_text = Some((key, value.clone(), used.clone()));
                    (value, used)
                }
            };
            text = Some(value);
            helper = Some(used);
        }
        // `act` checks the page again right before input, after any text call.
        self.browser.act(&action, &page, text.as_deref())?;
        self.pending_text = None;
        // Recorded before observing: a stale observation must not erase the action.
        self.history.push(Step {
            action: action.label.clone(),
            kind: action.kind,
            node: action.node,
            text,
            text_model: helper.as_ref().map(|h| h.model.clone()),
            text_latency_ms: helper.as_ref().map_or(0, |h| h.latency_ms),
            latency_ms: decision.latency_ms,
            page_changed: None,
            url: page.url.clone(),
        });
        self.page = self.browser.observe()?;
        let last = self.history.last_mut().expect("just pushed");
        last.page_changed = Some(self.page.fingerprint != page.fingerprint);
        last.url = self.page.url.clone();
        let recent = &self.history[self.history.len().saturating_sub(3)..];
        let stuck = recent.len() == 3
            && recent
                .iter()
                .all(|step| step.page_changed == Some(false) && step.kind != ActionKind::Wait);
        self.status = if stuck {
            Status::Blocked
        } else {
            Status::Ready
        };
        Ok(())
    }
}

/// The models on a gateway: `/systemone` for decisions, chat completions for text.
pub struct HostedModels {
    pub gateway: Gateway,
    pub decision_model: String,
    pub text_model: String,
}

impl HostedModels {
    fn systemone(&mut self, body: &Json) -> Result<serde_json::Value, String> {
        self.gateway.post("/systemone", &body.to_bytes())
    }

    /// The decision model picks one of the goal's own strings for the field.
    fn text_from_goal(&mut self, context: &Json, today: &str) -> Result<Option<String>, String> {
        let goal = match context.get("goal") {
            Some(Json::String(goal)) => goal.clone(),
            _ => String::new(),
        };
        let options = goal_strings(&goal, today);
        if options.len() < 2 {
            return Ok(None);
        }
        let raw = self.systemone(&goal_string_question(
            &self.decision_model,
            context,
            &options,
        ))?;
        Ok(Some(validate_choice(&raw, "text", &options)?.choice))
    }
}

impl Models for HostedModels {
    fn choose(
        &mut self,
        page: &Observation,
        goal: &str,
        history: &[Step],
    ) -> Result<Decision, String> {
        let today = local_today();
        let (body, space, operations) =
            decision_request(&self.decision_model, page, goal, history, &today);
        let started = Instant::now();
        let raw = self.systemone(&body)?;
        let operation_ids: Vec<String> = match &operations {
            Json::Object(members) => members.iter().map(|(name, _)| name.clone()).collect(),
            _ => Vec::new(),
        };
        let operation = validate_choice(&raw, "operation", &operation_ids)?;
        let mut probabilities = BTreeMap::new();
        let (choice, target) = match space.targets_for(&operation.choice) {
            Some(candidates) => {
                // Unused target heads cannot cause an action; only the chosen one is checked.
                let pick = chosen_target(
                    &raw,
                    &operation.choice,
                    candidates,
                    option_cap(&self.decision_model),
                )?;
                let target = match pick {
                    TargetPick::Chosen(target) => target,
                    TargetPick::Finalists(finalists) => {
                        let question = finalist_question(
                            goal,
                            &operation.choice,
                            candidates,
                            &finalists,
                            &today,
                        );
                        let body = Json::object()
                            .with("model", &self.decision_model)
                            .with("state", body.get("state").cloned().unwrap_or(Json::Null))
                            .with(
                                "questions",
                                Json::object().with(&final_name(&operation.choice), question),
                            );
                        let raw = self.systemone(&body)?;
                        final_choice(&raw, &operation.choice, &finalists)?
                    }
                };
                for (index, action) in candidates {
                    probabilities.insert(
                        action.id.clone(),
                        target.probabilities.get(index).copied().unwrap_or(0.0),
                    );
                }
                let chosen = candidates
                    .iter()
                    .find(|(index, _)| *index == target.choice)
                    .map(|(_, action)| action.id.clone())
                    .unwrap_or_default();
                (chosen, Some(target.choice))
            }
            None => {
                let choice = space
                    .controls
                    .iter()
                    .find(|(name, _)| *name == operation.choice)
                    .map_or(operation.choice.clone(), |(_, action)| action.id.clone());
                probabilities.insert(choice.clone(), operation.probabilities[&operation.choice]);
                (choice, None)
            }
        };
        Ok(Decision {
            choice,
            operation: operation.choice,
            target,
            confidence: operation.confidence,
            probabilities,
            latency_ms: started.elapsed().as_millis(),
        })
    }

    fn field_text(&mut self, context: &Json) -> Result<(String, TextHelper), String> {
        let today = local_today();
        let started = Instant::now();
        let request = chat_request(&self.text_model, context, &today, prompts::TEXT_VALUE);
        let body = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        let raw = self.gateway.post("/chat/completions", &body)?;
        let reply: text::ChatResponse = serde_json::from_value(raw).unwrap_or_default();
        let content = reply
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .unwrap_or_default();
        let parsed = parse_field_value(&content);
        let refused = parsed == FieldValue::Refused;
        let (value, model) = match resolve_field_value(parsed, || {
            self.text_from_goal(context, &today)
        })? {
            Some(Resolved::Typed(value)) => (value, self.text_model.clone()),
            Some(Resolved::FromGoal(value)) => (value, self.decision_model.clone()),
            None if refused => {
                return Err(
                    "Text helper refused the field and the goal names no value for it; \
                                nothing typed."
                        .into(),
                )
            }
            None => {
                let field = context
                    .get("field")
                    .map(Json::to_python)
                    .unwrap_or_default();
                let reply: String = content.chars().take(240).collect();
                return Err(format!(
                        "Text helper returned no valid field value; nothing typed. field={field} reply={reply:?}"
                    ));
            }
        };
        Ok((
            value,
            TextHelper {
                model,
                latency_ms: started.elapsed().as_millis(),
            },
        ))
    }
}

/// Today's local date, `YYYY-MM-DD`: the questions resolve "today" against it.
pub fn local_today() -> String {
    #[cfg(unix)]
    {
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut parts: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { libc::localtime_r(&now, &mut parts) }.is_null() {
            return String::new();
        }
        format!(
            "{:04}-{:02}-{:02}",
            parts.tm_year + 1900,
            parts.tm_mon + 1,
            parts.tm_mday
        )
    }
    #[cfg(windows)]
    {
        let mut parts: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut parts) };
        format!("{:04}-{:02}-{:02}", parts.wYear, parts.wMonth, parts.wDay)
    }
}

/// Python's `repr()` of a short string, as the step line has always shown it.
fn quoted(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The line printed after a tick, or none when a stale decision was retried.
pub fn step_line<B: Browser, M: Models>(
    agent: &Agent<B, M>,
    shown: usize,
    decider: &str,
) -> Option<String> {
    let count = agent.history.len();
    if count == shown && agent.status == Status::Ready {
        return None;
    }
    let mut line = format!(
        "{:>5} ms  {count} actions  {}",
        agent.elapsed_ms(),
        agent.status.name()
    );
    if agent.finished() && count == shown {
        line.push_str(&format!("  stopped  {}", agent.page.url));
    } else if let Some(step) = agent.history.last() {
        line.push_str(&format!("  {}  {}", step.kind_name(), step.action));
        if let Some(text) = &step.text {
            line.push_str(&format!(
                "  {}  text {} ms",
                quoted(text),
                step.text_latency_ms
            ));
        }
        line.push_str(&format!("  {decider} {} ms", step.latency_ms));
    }
    Some(line)
}

pub struct Options {
    pub goal: String,
    pub start_url: String,
    pub decision_model: String,
    pub text_model: String,
    pub max_steps: usize,
    pub chrome: ChromeMode,
}

/// Runs the task to done, blocked, or an error, and returns the exit code.
pub fn run(options: Options, gateway: Gateway) -> i32 {
    let socket = match chrome::devtools_socket(&options.chrome)
        .and_then(|url| cdp::Socket::connect(&url).map_err(|error| error.to_string()))
    {
        Ok(socket) => socket,
        Err(message) => {
            out::error_line(&format!("browser-use stopped: {message}"));
            return 1;
        }
    };
    let models = HostedModels {
        gateway,
        decision_model: options.decision_model.clone(),
        text_model: options.text_model.clone(),
    };
    let agent = CdpBrowser::open(cdp::Cdp::new(socket), &options.start_url)
        .and_then(|browser| Agent::new(browser, models, &options.goal, options.max_steps));
    let mut agent = match agent {
        Ok(agent) => agent,
        Err(error) => {
            out::error_line(&format!("browser-use stopped: {error}"));
            return 1;
        }
    };
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    let _interrupt =
        crate::util::interrupt::on_interrupt(move || flag.store(true, Ordering::SeqCst));
    let mut shown = 0;
    let outcome = loop {
        if interrupted.load(Ordering::SeqCst) {
            break Err(None);
        }
        if agent.finished() {
            break Ok(());
        }
        if let Err(error) = agent.tick() {
            break Err(Some(error));
        }
        if let Some(line) = step_line(&agent, shown, &options.decision_model) {
            println!("{line}");
        }
        shown = agent.history.len();
    };
    agent.browser.close();
    match outcome {
        Err(None) => {
            out::error_line("\nstopped");
            130
        }
        Err(Some(error)) => {
            out::error_line(&format!("browser-use stopped: {error}"));
            1
        }
        Ok(()) if agent.status == Status::Done => {
            println!("Done. The page stays open. Continue from here.");
            println!("{}", agent.page.url);
            0
        }
        Ok(()) => {
            println!("Stopped. The page stays open.");
            println!("{}", agent.page.url);
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn page(fingerprint: &str, actions: Vec<Action>) -> Observation {
        Observation {
            url: "http://127.0.0.1/page".into(),
            title: "Page".into(),
            text: "Search flights".into(),
            actions,
            marker: serde_json::Value::Null,
            page_key: serde_json::Value::Null,
            guards: serde_json::Value::Null,
            fingerprint: fingerprint.into(),
        }
    }

    fn click(id: &str, label: &str) -> Action {
        Action {
            id: id.into(),
            kind: ActionKind::Click,
            node: Some(1),
            label: label.into(),
            ..Action::default()
        }
    }

    #[derive(Default)]
    struct FakeBrowser {
        pages: VecDeque<Observation>,
        stale_reads: usize,
        acted: Vec<String>,
    }

    impl Browser for FakeBrowser {
        fn observe(&mut self) -> Result<Observation, StepError> {
            let next = if self.pages.len() > 1 {
                self.pages.pop_front()
            } else {
                self.pages.front().cloned()
            };
            next.ok_or(StepError::Stale)
        }
        fn fresh(
            &mut self,
            _page: &Observation,
            _action: Option<&Action>,
        ) -> Result<bool, StepError> {
            if self.stale_reads > 0 {
                self.stale_reads -= 1;
                return Ok(false);
            }
            Ok(true)
        }
        fn act(
            &mut self,
            action: &Action,
            page: &Observation,
            _text: Option<&str>,
        ) -> Result<(), StepError> {
            // The real browser checks the page again right before input.
            if !self.fresh(page, Some(action))? {
                return Err(StepError::Stale);
            }
            self.acted.push(action.id.clone());
            Ok(())
        }
        fn close(&mut self) {}
    }

    struct FakeModels {
        choices: VecDeque<&'static str>,
        asked: usize,
    }

    impl FakeModels {
        fn choosing(choices: &[&'static str]) -> Self {
            Self {
                choices: choices.iter().copied().collect(),
                asked: 0,
            }
        }
    }

    impl Models for FakeModels {
        fn choose(
            &mut self,
            _page: &Observation,
            _goal: &str,
            _history: &[Step],
        ) -> Result<Decision, String> {
            self.asked += 1;
            let choice = self.choices.pop_front().unwrap_or("DONE").to_string();
            Ok(Decision {
                choice: choice.clone(),
                operation: "CLICK".into(),
                target: None,
                confidence: 1.0,
                probabilities: BTreeMap::from([(choice, 1.0)]),
                latency_ms: 1,
            })
        }
        fn field_text(&mut self, _context: &Json) -> Result<(String, TextHelper), String> {
            Ok((
                "value".into(),
                TextHelper {
                    model: "text".into(),
                    latency_ms: 1,
                },
            ))
        }
    }

    fn agent(
        pages: Vec<Observation>,
        choices: &[&'static str],
        max_steps: usize,
    ) -> Agent<FakeBrowser, FakeModels> {
        let browser = FakeBrowser {
            pages: pages.into(),
            ..FakeBrowser::default()
        };
        Agent::new(browser, FakeModels::choosing(choices), "goal", max_steps).unwrap()
    }

    fn step(label: &str) -> Step {
        Step {
            action: label.into(),
            page_changed: Some(true),
            ..Step::default()
        }
    }

    #[test]
    fn a_pay_click_is_refused_before_the_browser_acts() {
        let mut agent = agent(
            vec![page("a", vec![click("e1", "Pay ₹4,532")])],
            &["e1"],
            60,
        );
        agent.tick().unwrap();
        assert_eq!(agent.status, Status::Blocked);
        assert!(agent.browser.acted.is_empty());
    }

    #[test]
    fn a_recent_repeat_is_dropped() {
        let history = vec![step("Mumbai"), step("Mumbai")];
        let kept = suppress_repeated(&[click("e1", "Mumbai"), click("e2", "Search")], &history);
        assert_eq!(
            kept.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(),
            ["Search"]
        );
    }

    #[test]
    fn scrolling_is_never_dropped_as_a_repeat() {
        let scroll = Action {
            id: "scroll_down".into(),
            kind: ActionKind::Scroll,
            label: "Scroll down".into(),
            ..Action::default()
        };
        let history: Vec<Step> = (0..3)
            .map(|_| Step {
                kind: ActionKind::Scroll,
                ..step("Scroll down")
            })
            .collect();
        assert_eq!(suppress_repeated(&[scroll], &history).len(), 1);
    }

    #[test]
    fn a_repeat_is_forgotten_after_six_actions() {
        let mut history = vec![step("Mumbai"), step("Mumbai")];
        history.extend((0..6).map(|i| step(&format!("step {i}"))));
        let kept = suppress_repeated(&[click("e1", "Mumbai"), click("e2", "Search")], &history);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn three_unchanged_pages_in_a_row_block_the_run() {
        let same = page("same", vec![click("e1", "Next")]);
        let mut agent = agent(vec![same], &["e1", "e1", "e1"], 60);
        for _ in 0..3 {
            agent.tick().unwrap();
        }
        assert_eq!(agent.status, Status::Blocked);
        assert_eq!(agent.browser.acted.len(), 3);
    }

    #[test]
    fn the_action_budget_stops_the_run() {
        let pages: Vec<Observation> = (0..10)
            .map(|i| page(&format!("p{i}"), vec![click("e1", &format!("Next {i}"))]))
            .collect();
        let mut agent = agent(pages, &["e1"; 10], 3);
        let mut error = None;
        for _ in 0..10 {
            if let Err(e) = agent.tick() {
                error = Some(e);
                break;
            }
        }
        assert_eq!(agent.browser.acted.len(), 3);
        assert!(matches!(error, Some(StepError::Budget(_))), "{error:?}");
    }

    #[test]
    fn the_decision_budget_is_twice_the_action_budget() {
        let mut agent = agent(vec![page("a", vec![click("e1", "Next")])], &[], 3);
        agent.decisions = 6;
        assert!(matches!(agent.tick(), Err(StepError::Budget(_))));
        assert_eq!(agent.models.asked, 0);
    }

    #[test]
    fn a_stale_page_is_observed_again_without_acting() {
        let mut agent = agent(vec![page("a", vec![click("e1", "Next")])], &["e1"], 60);
        agent.browser.stale_reads = 2;
        agent.tick().unwrap();
        assert!(agent.browser.acted.is_empty());
        assert_eq!(agent.status, Status::Ready);
    }

    #[test]
    fn done_on_a_page_that_changed_is_not_accepted() {
        let mut agent = agent(vec![page("a", vec![click("e1", "Next")])], &["DONE"], 60);
        agent.predict().unwrap();
        agent.browser.stale_reads = 1;
        assert_eq!(agent.act(), Err(StepError::Stale));
        assert_eq!(agent.status, Status::Ready);
    }

    #[test]
    fn a_fill_types_the_text_models_value() {
        let mut fill = click("e1", "Where to?");
        fill.kind = ActionKind::Fill;
        let mut agent = agent(
            vec![page("a", vec![fill]), page("b", Vec::new())],
            &["e1"],
            60,
        );
        agent.tick().unwrap();
        assert_eq!(agent.history[0].text.as_deref(), Some("value"));
        assert_eq!(agent.history[0].page_changed, Some(true));
    }

    #[test]
    fn the_step_line_quotes_text_like_python() {
        assert_eq!(quoted("Bengaluru"), "'Bengaluru'");
        assert_eq!(quoted("O'Hare"), "\"O'Hare\"");
    }
}
