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
        };
        browser.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": VIEWPORT.0, "height": VIEWPORT.1, "deviceScaleFactor": 1, "mobile": false}),
        )?;
        // Keeps animation frames and menus rendering in a background tab
        // without bringing the person's Chrome window forward.
        browser.call(
            "Emulation.setFocusEmulationEnabled",
            json!({"enabled": true}),
        )?;
        browser.call("Page.navigate", json!({"url": url}))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if let Ok(Value::String(state)) = browser.evaluate("document.readyState") {
                if state == "complete" {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(browser)
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
                    let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
                    self.call(
                        "Input.dispatchKeyEvent",
                        json!({"type": "keyDown", "key": "a", "code": "KeyA", "modifiers": modifiers,
                               "commands": ["selectAll"]}),
                    )?;
                    self.call(
                        "Input.dispatchKeyEvent",
                        json!({"type": "keyUp", "key": "a", "code": "KeyA", "modifiers": modifiers}),
                    )?;
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
}
