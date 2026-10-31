//! What the agent sees of a page and how it touches it: `snapshot.js` reads
//! visible text and controls; a click or a fill goes to a node the snapshot
//! named, at the point a hit test confirms, only if the page still matches.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::cdp::{Cdp, CdpError, Wire};
use super::json::Json;
use super::StepError;

pub const SNAPSHOT_JS: &str = include_str!("js/snapshot.js");
const SETTLE_JS: &str = include_str!("js/settle.js");
const HIT_TEST_JS: &str = include_str!("js/hit_test.js");
const GUARD_READ_JS: &str = include_str!("js/guard_read.js");
const FOCUS_CHECK_JS: &str = include_str!("js/focus_check.js");
const SELECT_TEXT_JS: &str = include_str!("js/select_text.js");
const VIEWPORT: (u32, u32) = (1120, 780);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    #[default]
    Click,
    Fill,
    Select,
    Scroll,
    Wait,
}

/// One control the snapshot offered. `node` is absent for scroll and wait.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct Action {
    #[serde(default)]
    pub id: String,
    pub kind: ActionKind,
    #[serde(default)]
    pub node: Option<u64>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub current_value: Option<String>,
    #[serde(default)]
    pub checked: Option<String>,
    #[serde(default)]
    pub selected: Option<String>,
    #[serde(default)]
    pub expanded: Option<String>,
    #[serde(default)]
    pub autocomplete: Option<String>,
    #[serde(default)]
    pub delta: Option<i64>,
    /// A link aimed at another browsing context (`_blank`, a named target):
    /// its click may open a new tab.
    #[serde(default)]
    pub opens_tab: Option<bool>,
    /// That link's absolute URL, to tell the tab it opens from others.
    #[serde(default)]
    pub link: Option<String>,
}

/// One read of the page. `marker`, `page_key` and `guards` are compared, never
/// interpreted, so they stay JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub url: String,
    pub title: String,
    pub text: String,
    pub actions: Vec<Action>,
    pub marker: Value,
    pub page_key: Value,
    pub guards: Value,
    pub fingerprint: String,
}

#[derive(Deserialize)]
struct Snapshot {
    url: String,
    title: String,
    text: String,
    actions: Vec<Action>,
    marker: Value,
    page_key: Value,
    guards: Value,
}

impl Observation {
    pub fn from_snapshot(raw: &Value) -> Result<Self, String> {
        let snapshot: Snapshot = serde_json::from_value(raw.clone())
            .map_err(|error| format!("the page snapshot did not parse: {error}"))?;
        Ok(Self {
            url: snapshot.url,
            title: snapshot.title,
            text: snapshot.text,
            actions: snapshot.actions,
            marker: snapshot.marker,
            page_key: snapshot.page_key,
            guards: snapshot.guards,
            fingerprint: fingerprint(raw),
        })
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|action| action.id == id)
    }
}

/// SHA-256 of `json.dumps({url, text, actions, scroll}, sort_keys=True)`, the
/// bytes Jev Ultrafast hashed, so a page is "unchanged" by the same rule.
pub fn fingerprint(raw: &Value) -> String {
    let mut content = serde_json::Map::new();
    for key in ["url", "text", "actions", "scroll"] {
        content.insert(
            key.to_string(),
            raw.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    let text = Json::sorted(&Value::Object(content)).to_python();
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// What the loop needs from a browser. `CdpBrowser` in a run, a fake in tests.
pub trait Browser {
    fn observe(&mut self) -> Result<Observation, StepError>;
    /// The page still matches the observation; for a click or select, the
    /// target node too.
    fn fresh(&mut self, page: &Observation, action: Option<&Action>) -> Result<bool, StepError>;
    fn act(
        &mut self,
        action: &Action,
        page: &Observation,
        text: Option<&str>,
    ) -> Result<(), StepError>;
    /// Brings the tab forward and leaves it open for the person.
    fn close(&mut self);
}

pub struct CdpBrowser<W: Wire> {
    cdp: Cdp<W>,
    target: Option<String>,
    session: String,
    after_input: Option<Action>,
    /// Page tabs that existed before the last click; a tab outside this set
    /// that our tab opened is followed.
    known_tabs: Vec<String>,
}

impl<W: Wire> CdpBrowser<W> {
    /// Opens a background tab on `url` and waits up to 15 s for it to load.
    pub fn open(mut cdp: Cdp<W>, url: &str) -> Result<Self, StepError> {
        let created = cdp
            .call(
                "Target.createTarget",
                json!({"url": "about:blank", "background": true}),
                None,
            )
            .map_err(browser_error)?;
        let target = string_field(&created, "targetId")?;
        let attached = cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                None,
            )
            .map_err(browser_error)?;
        let session = string_field(&attached, "sessionId")?;
        let mut browser = Self {
            cdp,
            target: Some(target),
            session,
            after_input: None,
            known_tabs: Vec::new(),
        };
        browser.prepare_tab()?;
        browser.call("Page.navigate", json!({"url": url}))?;
        browser.wait_loaded();
        browser.known_tabs = browser.page_tabs()?.into_iter().map(|tab| tab.0).collect();
        Ok(browser)
    }

    /// The viewport every observation assumes, and focus emulation, which
    /// keeps animation frames and menus rendering in a background tab without
    /// bringing the person's Chrome window forward.
    fn prepare_tab(&mut self) -> Result<(), StepError> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": VIEWPORT.0, "height": VIEWPORT.1, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        self.call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        )?;
        Ok(())
    }

    /// Up to 15 s for the document to finish loading.
    fn wait_loaded(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Value::String(state)) = self.evaluate("document.readyState") {
                if state == "complete" {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Open page tabs as (target id, opener id, url).
    fn page_tabs(&mut self) -> Result<Vec<(String, Option<String>, String)>, StepError> {
        let reply = self
            .cdp
            .call("Target.getTargets", json!({}), None)
            .map_err(browser_error)?;
        Ok(reply
            .get("targetInfos")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|info| info.get("type").and_then(Value::as_str) == Some("page"))
            .filter_map(|info| {
                let id = info.get("targetId")?.as_str()?.to_string();
                let opener = info
                    .get("openerId")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let url = info
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                Some((id, opener, url))
            })
            .collect())
    }

    /// A tab our tab opened since the last click, if any: one whose opener is
    /// our tab, or, after a click on a new-tab link, one on that link's
    /// origin. A `rel=noopener` link (how Amazon opens product pages) leaves no
    /// opener to match, and a tab the person opens elsewhere matches neither.
    fn opened_tab(&mut self, link: Option<&str>) -> Result<Option<String>, StepError> {
        let ours = self.target.clone();
        let known = self.known_tabs.clone();
        let link_origin = link.and_then(origin_of);
        Ok(self
            .page_tabs()?
            .into_iter()
            .find(|(id, opener, url)| {
                !known.contains(id)
                    && ((opener.is_some() && *opener == ours)
                        || (link_origin.is_some() && origin_of(url) == link_origin))
            })
            .map(|(id, _, _)| id))
    }

    /// Moves the run to `target`, a tab our page opened. The old tab stays open.
    fn follow(&mut self, target: String) -> Result<(), StepError> {
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId": target, "flatten": true}),
                None,
            )
            .map_err(browser_error)?;
        self.session = string_field(&attached, "sessionId")?;
        self.target = Some(target);
        self.after_input = None;
        self.prepare_tab()?;
        self.wait_loaded();
        self.known_tabs = self.page_tabs()?.into_iter().map(|(id, _, _)| id).collect();
        Ok(())
    }

    /// After a click that opens a tab, wait up to 2 s for it; on any other
    /// observation, follow one that is already there (window.open from script).
    fn follow_opened_tab(&mut self, link: Option<&str>) -> Result<(), StepError> {
        let wait = if link.is_some() { 2 } else { 0 };
        let deadline = Instant::now() + Duration::from_secs(wait);
        loop {
            if let Some(tab) = self.opened_tab(link)? {
                return self.follow(tab);
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, StepError> {
        self.cdp
            .call(method, params, Some(&self.session))
            .map_err(browser_error)
    }

    /// A script exception means the document moved under the read: stale.
    fn evaluate(&mut self, expression: &str) -> Result<Value, StepError> {
        let response = self.call(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
        )?;
        if response.get("exceptionDetails").is_some() {
            return Err(StepError::Stale);
        }
        Ok(response
            .get("result")
            .and_then(|r| r.get("value"))
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn settle_after_input(&mut self) {
        let Some(action) = self.after_input.take() else {
            return;
        };
        let expression = format!("{}({})", SETTLE_JS.trim_end(), js_action(&action));
        // Read-only and best effort: a navigation can end it early.
        let _ = self.cdp.call(
            "Runtime.evaluate",
            json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
            Some(&self.session),
        );
    }
}

impl<W: Wire> Browser for CdpBrowser<W> {
    fn observe(&mut self) -> Result<Observation, StepError> {
        let link = self
            .after_input
            .as_ref()
            .filter(|a| a.opens_tab == Some(true))
            .and_then(|a| a.link.clone());
        self.follow_opened_tab(link.as_deref())?;
        self.settle_after_input();
        for attempt in 0..10 {
            match self.evaluate(SNAPSHOT_JS) {
                Ok(Value::Null) | Err(StepError::Stale) if attempt < 9 => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(Value::Null) | Err(StepError::Stale) => return Err(StepError::Stale),
                Ok(raw) => return Observation::from_snapshot(&raw).map_err(StepError::Browser),
                Err(error) => return Err(error),
            }
        }
        Err(StepError::Stale)
    }

    fn fresh(&mut self, page: &Observation, action: Option<&Action>) -> Result<bool, StepError> {
        if let Some(action) =
            action.filter(|a| matches!(a.kind, ActionKind::Click | ActionKind::Select))
        {
            let Some(node) = action.node else {
                return Ok(false);
            };
            let current = self.evaluate(&format!("{}({node})", GUARD_READ_JS.trim_end()))?;
            let recorded = page
                .guards
                .get(node.to_string())
                .cloned()
                .unwrap_or(Value::Null);
            return Ok(current == json!([page.page_key, recorded]));
        }
        let marker = format!(
            "(() => {{ const state={}; return state?.marker ?? null; }})()",
            SNAPSHOT_JS.trim_end()
        );
        Ok(self.evaluate(&marker)? == page.marker)
    }

    fn act(
        &mut self,
        action: &Action,
        page: &Observation,
        text: Option<&str>,
    ) -> Result<(), StepError> {
        if !self.fresh(page, Some(action))? {
            return Err(StepError::Stale);
        }
        self.known_tabs = self.page_tabs()?.into_iter().map(|(id, _, _)| id).collect();
        match action.kind {
            ActionKind::Wait => {
                std::thread::sleep(Duration::from_millis(100));
                return Ok(());
            }
            ActionKind::Scroll => {
                self.call(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": 550, "y": 650, "deltaX": 0,
                           "deltaY": action.delta.unwrap_or(0)}),
                )?;
            }
            ActionKind::Click | ActionKind::Fill | ActionKind::Select => {
                if action.node.is_none() {
                    return Err(StepError::Browser(
                        "the action names no observed node".into(),
                    ));
                }
                let target = self.evaluate(&format!(
                    "{}({})",
                    HIT_TEST_JS.trim_end(),
                    js_action(action)
                ))?;
                let point = match (
                    target.get("x").and_then(Value::as_f64),
                    target.get("y").and_then(Value::as_f64),
                ) {
                    (Some(x), Some(y)) => (x, y),
                    _ if action.kind == ActionKind::Select => return Err(StepError::Browser(
                        "the dropdown change was not confirmed; look at the page before retrying"
                            .into(),
                    )),
                    _ => return Err(StepError::Stale),
                };
                if action.kind != ActionKind::Select {
                    for event in ["mousePressed", "mouseReleased"] {
                        self.call(
                            "Input.dispatchMouseEvent",
                            json!({"type": event, "x": point.0, "y": point.1, "button": "left", "clickCount": 1}),
                        )?;
                    }
                }
                if action.kind == ActionKind::Fill {
                    // Select the old value in code, not with a key event: a
                    // page's keydown handler could move focus after the check.
                    let node = action.node.unwrap_or_default();
                    self.evaluate(&format!("{}({node})", SELECT_TEXT_JS.trim_end()))?;
                    // Nothing runs between this check and the text: it goes only
                    // to a plain, safe text field, judged as it is now.
                    let focus = self.evaluate(&format!("{}({node})", FOCUS_CHECK_JS.trim_end()))?;
                    if let Some(reason) = focus_refusal(&action.label, &focus) {
                        return Err(StepError::Refused(reason));
                    }
                    self.call("Input.insertText", json!({"text": text.unwrap_or("")}))?;
                }
            }
        }
        self.after_input = Some(action.clone());
        Ok(())
    }

    fn close(&mut self) {
        if let Some(target) = self.target.take() {
            let _ = self
                .cdp
                .call("Target.activateTarget", json!({"targetId": target}), None);
        }
    }
}

/// Why typing must not go ahead after the click, given what `focus_check.js`
/// found holding focus; `None` when it may.
pub fn focus_refusal(label: &str, focus: &Value) -> Option<String> {
    let field = |key: &str| {
        focus
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let holder = field("focus");
    if holder != "same" && holder != "other" {
        let holder = if holder.is_empty() {
            "an unknown element".to_string()
        } else {
            holder
        };
        return Some(format!(
            "focus moved from {label:?} to {holder}; nothing typed"
        ));
    }
    // The focused field as it is now, judged again even when it is the node
    // the guard passed: a handler can have turned it into a card or password field.
    let focused = Action {
        kind: ActionKind::Fill,
        label: field("label"),
        autocomplete: Some(field("autocomplete")).filter(|a| !a.is_empty()),
        ..Action::default()
    };
    let text = focus.get("text").and_then(Value::as_bool).unwrap_or(false);
    if text && field("type") != "password" && !super::guards::sensitive_fill(&focused) {
        return None;
    }
    Some(if holder == "same" {
        format!("{label:?} is no longer a plain text field; nothing typed")
    } else {
        format!(
            "focus moved from {label:?} to {:?}, which is not a plain text field; nothing typed",
            focused.label
        )
    })
}

/// `scheme://host[:port]` of a URL, the part a redirect keeps.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{}://{}", scheme.to_lowercase(), host.to_lowercase()))
}

/// The three fields the page scripts read off an action.
fn js_action(action: &Action) -> String {
    json!({
        "node": action.node,
        "kind": match action.kind {
            ActionKind::Click => "click",
            ActionKind::Fill => "fill",
            ActionKind::Select => "select",
            ActionKind::Scroll => "scroll",
            ActionKind::Wait => "wait",
        },
        "value": action.value,
    })
    .to_string()
}

fn string_field(value: &Value, key: &str) -> Result<String, StepError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| StepError::Browser(format!("Chrome's reply had no {key}")))
}

fn browser_error(error: CdpError) -> StepError {
    StepError::Browser(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recorded() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/browser/flights-snapshot.json"
        ))
        .unwrap()
    }

    #[test]
    fn the_recorded_snapshot_parses() {
        let page = Observation::from_snapshot(&recorded()).unwrap();
        assert_eq!(
            page.title,
            "Find Cheap Flights Worldwide & Book Your Ticket - Google Flights"
        );
        assert_eq!(page.actions.len(), 31);
    }

    #[test]
    fn scroll_and_wait_actions_have_no_node() {
        let page = Observation::from_snapshot(&recorded()).unwrap();
        for action in page
            .actions
            .iter()
            .filter(|a| matches!(a.kind, ActionKind::Scroll | ActionKind::Wait))
        {
            assert_eq!(action.node, None, "{}", action.id);
        }
        assert!(page.actions.iter().any(|a| a.kind == ActionKind::Wait));
    }

    #[test]
    fn the_fingerprint_matches_the_one_python_computed() {
        let raw = recorded();
        assert_eq!(fingerprint(&raw), raw["fingerprint"].as_str().unwrap());
    }

    #[test]
    fn a_changed_text_changes_the_fingerprint() {
        let mut raw = recorded();
        raw["text"] = Value::String("something else".into());
        assert_ne!(
            fingerprint(&raw),
            recorded()["fingerprint"].as_str().unwrap()
        );
    }

    #[test]
    fn typing_goes_ahead_when_the_field_keeps_focus() {
        let focus = json!({"focus": "same", "text": true, "type": "text", "label": "Where to?", "autocomplete": ""});
        assert_eq!(focus_refusal("Where to?", &focus), None);
    }

    #[test]
    fn typing_stops_when_the_judged_field_turned_into_a_card_field() {
        let focus = json!({"focus": "same", "text": true, "type": "text", "label": "Full name", "autocomplete": "cc-number"});
        assert!(focus_refusal("Full name", &focus).is_some());
        let password = json!({"focus": "same", "text": true, "type": "password", "label": "Full name", "autocomplete": ""});
        assert!(focus_refusal("Full name", &password).is_some());
    }

    #[test]
    fn typing_follows_focus_to_a_plain_text_field() {
        let focus = json!({"focus": "other", "text": true, "type": "text", "label": "Where else?", "autocomplete": ""});
        assert_eq!(focus_refusal("Where to?", &focus), None);
    }

    #[test]
    fn typing_never_follows_focus_to_a_card_field() {
        let focus = json!({"focus": "other", "text": true, "type": "text", "label": "Card number", "autocomplete": "cc-number"});
        assert!(focus_refusal("Full name", &focus).is_some());
    }

    #[test]
    fn typing_never_follows_focus_into_a_cross_origin_frame() {
        assert!(focus_refusal("Full name", &json!({"focus": "cross-origin frame"})).is_some());
    }

    #[test]
    fn typing_never_follows_focus_to_a_password_or_non_text_element() {
        let password = json!({"focus": "other", "text": true, "type": "password", "label": "Name", "autocomplete": ""});
        assert!(focus_refusal("Full name", &password).is_some());
        let button = json!({"focus": "other", "text": false, "type": "submit", "label": "Go", "autocomplete": ""});
        assert!(focus_refusal("Full name", &button).is_some());
    }

    #[test]
    fn a_redirect_keeps_the_links_origin() {
        assert_eq!(
            origin_of("https://www.amazon.in/sspa/click?ie=UTF8&url=%2Fdp%2FB0"),
            origin_of("https://www.amazon.in/SanDisk-Portable/dp/B0")
        );
        assert_ne!(
            origin_of("https://www.amazon.in/x"),
            origin_of("https://mail.google.com/")
        );
        assert_eq!(origin_of("about:blank"), None);
    }
}
