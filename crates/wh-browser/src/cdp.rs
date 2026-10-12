//! A minimal Chrome DevTools Protocol client: one websocket to one page, request and answer by id,
//! events buffered. No background task: the caller drives it from `block_on`, so an idle session
//! costs a socket and nothing else.
//!
//! Only loopback websockets are accepted. The client sends no `Origin`, no custom User-Agent and no
//! extra headers of its own; whatever the page itself sends is Brave's stock behaviour.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::error::{BrowserError, Result};

/// How long one command may take before the step fails.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Events kept while waiting for an answer; older ones are dropped.
const EVENT_CAP: usize = 256;

/// A protocol event (`Page.loadEventFired` and friends).
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The event name.
    pub method: String,
    /// Its parameters.
    pub params: Value,
}

/// One open session on one page.
pub struct Conn {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: VecDeque<Event>,
}

/// Is `url` a websocket on this machine?
pub fn is_loopback_ws(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("ws://") else {
        return false;
    };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

impl Conn {
    /// Open the websocket. Refuses anything that is not loopback.
    pub async fn connect(url: &str) -> Result<Self> {
        if !is_loopback_ws(url) {
            return Err(BrowserError::failed(format!(
                "refusing to connect to a non-loopback websocket: {url}"
            )));
        }
        let (ws, _) = timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async(url),
        )
        .await
        .map_err(|_| BrowserError::failed("timed out opening the page websocket"))?
        .map_err(|e| BrowserError::failed(format!("cannot open the page websocket: {e}")))?;
        Ok(Self {
            ws,
            next_id: 1,
            events: VecDeque::new(),
        })
    }

    /// Send one command and wait for its answer. Events that arrive meanwhile are kept.
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::text(msg.to_string()))
            .await
            .map_err(|e| BrowserError::failed(format!("{method}: send failed: {e}")))?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(value) = self.next_message(left).await? else {
                return Err(BrowserError::failed(format!("{method}: timed out")));
            };
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    let m = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("error");
                    return Err(BrowserError::failed(format!("{method}: {m}")));
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
        }
    }

    /// Wait up to `dur` for an event named `method`. True when it arrived. Events seen earlier count.
    pub async fn wait_event(&mut self, method: &str, dur: Duration) -> bool {
        if let Some(pos) = self.events.iter().position(|e| e.method == method) {
            self.events.remove(pos);
            return true;
        }
        let deadline = Instant::now() + dur;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.next_message(left).await {
                Ok(Some(_)) => {
                    if let Some(pos) = self.events.iter().position(|e| e.method == method) {
                        self.events.remove(pos);
                        return true;
                    }
                }
                _ => return false,
            }
        }
    }

    /// Forget buffered events (before an action whose events we want to wait for).
    pub fn clear_events(&mut self) {
        self.events.clear();
    }

    /// Read until one JSON message arrives; events are stored and the message is also returned.
    /// `None` on timeout.
    async fn next_message(&mut self, dur: Duration) -> Result<Option<Value>> {
        let Ok(next) = timeout(dur, self.ws.next()).await else {
            return Ok(None);
        };
        let msg = match next {
            None => return Err(BrowserError::failed("the browser closed the connection")),
            Some(Err(e)) => return Err(BrowserError::failed(format!("websocket error: {e}"))),
            Some(Ok(m)) => m,
        };
        let Message::Text(text) = msg else {
            return Ok(Some(Value::Null));
        };
        let value: Value = serde_json::from_str(text.as_str())
            .map_err(|e| BrowserError::failed(format!("bad CDP message: {e}")))?;
        if let Some(method) = value.get("method").and_then(Value::as_str) {
            if self.events.len() >= EVENT_CAP {
                self.events.pop_front();
            }
            self.events.push_back(Event {
                method: method.to_string(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            });
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_websockets_pass() {
        assert!(is_loopback_ws("ws://127.0.0.1:9222/devtools/page/AB"));
        assert!(is_loopback_ws("ws://localhost:9222/x"));
        assert!(!is_loopback_ws("ws://192.168.1.5:9222/x"));
        assert!(!is_loopback_ws("ws://127.0.0.1.evil.test:9222/x"));
        assert!(!is_loopback_ws("wss://127.0.0.1:9222/x"));
        assert!(!is_loopback_ws("http://127.0.0.1:9222/x"));
    }
}
