//! The DevTools HTTP endpoints (`/json/version`, `/json/list`, ...), over a plain blocking socket to
//! 127.0.0.1. No HTTP client crate: the endpoint is local, tiny and always HTTP/1.1 with a body.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

use crate::error::{BrowserError, Result};

/// One request to the debug port on loopback. Returns the body of a 200 answer.
pub fn request(port: u16, method: &str, path: &str, timeout: Duration) -> Result<String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| BrowserError::failed(format!("cannot reach 127.0.0.1:{port}: {e}")))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| BrowserError::failed(format!("debug port write failed: {e}")))?;
    // Chromium keeps the socket open despite Connection: close, so stop at Content-Length instead
    // of waiting out the read timeout (that wait was 3 s on every call)
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    while raw.len() < 8 * 1024 * 1024 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
        if answer_complete(&raw) {
            break;
        }
    }
    parse_response(&String::from_utf8_lossy(&raw))
}

/// The headers are in and the body has reached its Content-Length.
fn answer_complete(raw: &[u8]) -> bool {
    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
    head.lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .is_some_and(|len| raw.len() >= end + 4 + len)
}

/// Split an HTTP answer into status and body; anything but 200 is an error.
pub fn parse_response(raw: &str) -> Result<String> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| BrowserError::failed("the debug port sent no HTTP answer"))?;
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    if status != "200" {
        return Err(BrowserError::failed(format!(
            "the debug port answered HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }
    Ok(body.to_string())
}

/// One browser tab as `/json/list` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// The target id.
    pub id: String,
    /// The page title (page-controlled text).
    pub title: String,
    /// The current URL.
    pub url: String,
    /// The page's own websocket.
    pub ws_url: String,
}

/// The tabs worth driving: real pages, not DevTools, extension pages or workers.
pub fn parse_tabs(body: &str) -> Result<Vec<Tab>> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| BrowserError::failed(format!("bad /json/list answer: {e}")))?;
    let list = v
        .as_array()
        .ok_or_else(|| BrowserError::failed("/json/list is not a list"))?;
    let text = |t: &Value, k: &str| t.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(list
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("page"))
        .map(|t| Tab {
            id: text(t, "id"),
            title: text(t, "title"),
            url: text(t, "url"),
            ws_url: text(t, "webSocketDebuggerUrl"),
        })
        .filter(|t| !t.id.is_empty() && !t.url.starts_with("devtools://"))
        .collect())
}

/// All open page tabs.
pub fn list_tabs(port: u16) -> Result<Vec<Tab>> {
    parse_tabs(&request(port, "GET", "/json/list", Duration::from_secs(3))?)
}

/// The `/json/version` document (browser name, protocol version).
pub fn version(port: u16) -> Result<Value> {
    let body = request(port, "GET", "/json/version", Duration::from_secs(3))?;
    serde_json::from_str(&body).map_err(|e| BrowserError::failed(format!("bad /json/version: {e}")))
}

/// True when something accepts connections on 127.0.0.1:`port` (the acceptance check that the
/// debug port is closed on a normal Brave).
pub fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

/// Percent-encode a URL for the query part of `/json/new?<url>`.
pub fn encode_query(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~:/?=&#%".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_200_answer_gives_its_body() {
        let body = parse_response("HTTP/1.1 200 OK\r\nContent-Type: x\r\n\r\n{\"a\":1}").unwrap();
        assert_eq!(body, "{\"a\":1}");
    }

    #[test]
    fn an_answer_is_complete_at_its_content_length() {
        assert!(!answer_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabc"));
        assert!(answer_complete(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nabcde"));
        assert!(!answer_complete(b"HTTP/1.1 200 OK\r\nContent-Len"));
    }

    #[test]
    fn other_statuses_are_errors() {
        let e = parse_response("HTTP/1.1 404 Not Found\r\n\r\nnope").unwrap_err();
        assert!(e.message().contains("404"), "{e}");
        assert!(parse_response("garbage").is_err());
    }

    #[test]
    fn only_real_pages_are_listed() {
        let body = r#"[
          {"id":"A","type":"page","title":"Home","url":"https://a.test/","webSocketDebuggerUrl":"ws://127.0.0.1:1/devtools/page/A"},
          {"id":"B","type":"service_worker","title":"sw","url":"https://a.test/sw.js","webSocketDebuggerUrl":"ws://x"},
          {"id":"C","type":"page","title":"DT","url":"devtools://devtools/bundled/x.html","webSocketDebuggerUrl":"ws://x"}
        ]"#;
        let tabs = parse_tabs(body).unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "A");
        assert_eq!(tabs[0].title, "Home");
    }

    #[test]
    fn query_encoding_keeps_url_punctuation_and_escapes_the_rest() {
        assert_eq!(
            encode_query("https://a.test/x y?q=1"),
            "https://a.test/x%20y?q=1"
        );
    }
}
