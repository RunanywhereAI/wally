//! One DevTools WebSocket to Chrome's browser endpoint. Requests carry an id;
//! a reply is the message with that id, and events in between are dropped
//! (nothing here subscribes to any). Tabs are driven through flat sessions, so
//! one socket serves the whole run.

use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// browser-harness's per-call wait; every call here is a page read or a
/// single input event.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum CdpError {
    /// Chrome answered with a protocol error.
    Protocol(String),
    /// The socket failed, closed, or no reply came in time.
    Transport(String),
}

impl std::fmt::Display for CdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CdpError::Protocol(message) => write!(f, "Chrome refused the call: {message}"),
            CdpError::Transport(message) => write!(f, "lost the connection to Chrome: {message}"),
        }
    }
}

/// Where replies come from. The socket in a run; a script in tests.
pub trait Wire {
    fn send(&mut self, text: String) -> Result<(), CdpError>;
    fn receive(&mut self, deadline: Instant) -> Result<String, CdpError>;
}

pub struct Cdp<W: Wire> {
    wire: W,
    next_id: u64,
}

impl<W: Wire> Cdp<W> {
    pub fn new(wire: W) -> Self {
        Self { wire, next_id: 1 }
    }

    /// Sends one call and returns its `result`, or an empty object when the
    /// reply has none.
    pub fn call(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id;
        self.next_id += 1;
        let mut request = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session_id {
            request["sessionId"] = Value::String(session.to_string());
        }
        self.wire.send(request.to_string())?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let text = self.wire.receive(deadline)?;
            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if message.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(CdpError::Protocol(format!("{method}: {text}")));
            }
            return Ok(message.get("result").cloned().unwrap_or_else(|| json!({})));
        }
    }
}

pub struct Socket(WebSocket<MaybeTlsStream<TcpStream>>);

impl Socket {
    /// `ws_url` is the browser endpoint `/json/version` reported.
    pub fn connect(ws_url: &str) -> Result<Self, CdpError> {
        let (socket, _) = tungstenite::connect(ws_url)
            .map_err(|error| CdpError::Transport(format!("{ws_url}: {error}")))?;
        Ok(Self(socket))
    }

    fn set_read_timeout(&mut self, timeout: Duration) {
        if let MaybeTlsStream::Plain(stream) = self.0.get_mut() {
            let _ = stream.set_read_timeout(Some(timeout.max(Duration::from_millis(1))));
        }
    }
}

impl Wire for Socket {
    fn send(&mut self, text: String) -> Result<(), CdpError> {
        self.0
            .send(Message::text(text))
            .map_err(|error| CdpError::Transport(error.to_string()))
    }

    fn receive(&mut self, deadline: Instant) -> Result<String, CdpError> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CdpError::Transport("no reply within 5 s".into()));
            }
            self.set_read_timeout(left);
            match self.0.read() {
                Ok(Message::Text(text)) => return Ok(text.to_string()),
                Ok(Message::Close(_)) => return Err(CdpError::Transport("Chrome closed".into())),
                Ok(_) => continue,
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(error) => return Err(CdpError::Transport(error.to_string())),
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Replies in order, recording what was sent.
    #[derive(Default)]
    pub struct Script {
        pub sent: Vec<Value>,
        pub replies: VecDeque<String>,
    }

    impl Wire for Script {
        fn send(&mut self, text: String) -> Result<(), CdpError> {
            self.sent.push(serde_json::from_str(&text).unwrap());
            Ok(())
        }

        fn receive(&mut self, _deadline: Instant) -> Result<String, CdpError> {
            self.replies
                .pop_front()
                .ok_or_else(|| CdpError::Transport("script ran out".into()))
        }
    }

    #[test]
    fn a_reply_is_matched_by_id_past_events_and_other_replies() {
        let mut script = Script::default();
        script.replies.extend([
            r#"{"method":"Page.frameNavigated","params":{}}"#.to_string(),
            r#"{"id":99,"result":{"wrong":true}}"#.to_string(),
            r#"{"id":1,"result":{"targetId":"T"}}"#.to_string(),
        ]);
        let mut cdp = Cdp::new(script);
        let result = cdp
            .call("Target.createTarget", json!({"url": "about:blank"}), None)
            .unwrap();
        assert_eq!(result, json!({"targetId": "T"}));
    }

    #[test]
    fn a_protocol_error_names_the_method() {
        let mut script = Script::default();
        script
            .replies
            .push_back(r#"{"id":1,"error":{"code":-32000,"message":"No target"}}"#.to_string());
        let mut cdp = Cdp::new(script);
        let error = cdp
            .call("Target.attachToTarget", json!({}), None)
            .unwrap_err();
        assert_eq!(
            error,
            CdpError::Protocol("Target.attachToTarget: No target".into())
        );
    }

    #[test]
    fn a_session_call_carries_the_session_id() {
        let mut script = Script::default();
        script
            .replies
            .push_back(r#"{"id":1,"result":{}}"#.to_string());
        let mut cdp = Cdp::new(script);
        cdp.call("Runtime.evaluate", json!({}), Some("S1")).unwrap();
        assert_eq!(cdp.wire.sent[0]["sessionId"], "S1");
    }
}
