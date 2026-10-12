//! winhands browser module: read and drive the owner's real Brave over the Chrome DevTools Protocol.
//!
//! No extension, no relay, no header or User-Agent changes. A raw CDP client talks to a debug port on
//! 127.0.0.1 that exists only when Brave was started by [`launcher::launch`]. Pages are read as a
//! compact accessibility tree with short refs ([`ax`]), page text is always returned inside an
//! "untrusted content" wrapper ([`page::untrusted`]), and the [`gate`] refuses anything that submits,
//! pays, sends, deletes, accepts terms or enters a password.
//!
//! This crate knows nothing about winhands itself; the `winhands` binary adapts [`Browser::run`] to
//! the `computer` tool's `browser_*` actions.
//!
//! No `unsafe` in this crate (`unsafe_code = "forbid"` in the workspace lints).

#![warn(missing_docs)]

pub mod ax;
pub mod cdp;
pub mod error;
pub mod gate;
pub mod http;
pub mod launcher;
pub mod log;
pub mod page;

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::runtime::Runtime;

pub use error::{BrowserError, Result};
pub use page::StepOut;

use ax::Refs;
use cdp::Conn;
use page::{ReadMode, host_of};
use wh_core::approval::{Kind, Need};

/// Every action this module answers.
pub const ACTIONS: [&str; 10] = [
    "browser_status",
    "browser_launch",
    "browser_navigate",
    "browser_read",
    "browser_find",
    "browser_click",
    "browser_type",
    "browser_scroll",
    "browser_tabs",
    "browser_screenshot",
];

/// Is `action` one of ours?
pub fn is_browser_action(action: &str) -> bool {
    action.starts_with("browser_")
}

struct Live {
    target: String,
    conn: Conn,
}

struct State {
    rt: Runtime,
    live: Option<Live>,
    refs: Refs,
    /// The tab later actions drive; `None` = the first real page tab.
    current: Option<String>,
    /// Tabs this module opened (the only ones it will close).
    opened: HashSet<String>,
}

/// One browser session: finds Brave's debug port, keeps a connection to the current tab, and runs
/// actions one at a time.
pub struct Browser {
    state: Mutex<Option<State>>,
    fixed_port: Option<u16>,
    log: log::ActionLog,
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}

fn arg_str(step: &Map<String, Value>, key: &str) -> String {
    match step.get(key) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

fn arg_i64(step: &Map<String, Value>, key: &str) -> Option<i64> {
    match step.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn arg_flag(step: &Map<String, Value>, key: &str) -> bool {
    match step.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"),
        _ => false,
    }
}

impl Browser {
    /// A session that finds Brave through the process list. Logs to the default directory.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            fixed_port: None,
            log: log::ActionLog::new(log::default_dir()),
        }
    }

    /// A session on a known debug port (tests, or a port the owner chose), logging to `log_dir`.
    pub fn with_port(port: u16, log_dir: Option<std::path::PathBuf>) -> Self {
        Self {
            state: Mutex::new(None),
            fixed_port: Some(port),
            log: log::ActionLog::new(log_dir),
        }
    }

    /// Run one `browser_*` step. Every gate refusal is final (nobody is asked).
    pub fn run(&self, step: &Map<String, Value>) -> Result<StepOut> {
        self.run_approved(step, None).map_err(BrowserError::hard)
    }

    /// Run one `browser_*` step where a refusal the user could approve comes back as
    /// [`BrowserError::Approval`]. `approved` is the key ([`wh_core::approval::Need::key`]) of an
    /// approval they gave for this step: the one gate stop with that exact key is let through, anything
    /// else (the page changed, a different button) asks again.
    pub fn run_approved(
        &self,
        step: &Map<String, Value>,
        approved: Option<&str>,
    ) -> Result<StepOut> {
        let action = arg_str(step, "action");
        let out = self.dispatch(&action, step, approved);
        let detail = json!({
            "ref": arg_str(step, "ref"),
            "host": host_of(&arg_str(step, "url")),
            "typed_chars": arg_str(step, "text").chars().count(),
            "error": out.as_ref().err().map(|e| e.message().chars().take(160).collect::<String>()),
        });
        self.log.write(&action, detail, out.is_ok());
        out
    }

    fn port(&self) -> Result<u16> {
        match self.fixed_port {
            Some(p) => Ok(p),
            None => launcher::attach_port(),
        }
    }

    fn dispatch(
        &self,
        action: &str,
        step: &Map<String, Value>,
        approved: Option<&str>,
    ) -> Result<StepOut> {
        match action {
            "browser_status" => return self.status(),
            "browser_launch" => return self.launch(),
            "browser_tabs" => return self.tabs(step, approved),
            a if ACTIONS.contains(&a) => {}
            other => {
                return Err(BrowserError::invalid(format!(
                    "unknown browser action {other}; one of: {}",
                    ACTIONS.join(", ")
                )));
            }
        }
        self.with_page(|rt, conn, refs| match action {
            "browser_navigate" => {
                let wait = Duration::from_secs_f64(
                    arg_i64(step, "timeout").unwrap_or(15).clamp(1, 60) as f64,
                );
                let url = arg_str(step, "url");
                let url = if url.is_empty() {
                    arg_str(step, "text")
                } else {
                    url
                };
                rt.block_on(page::navigate(conn, refs, &url, wait, approved))
            }
            "browser_read" => {
                let budget = arg_i64(step, "limit")
                    .filter(|n| *n > 0)
                    .map_or(page::DEFAULT_BUDGET, |n| n as usize);
                let mode = ReadMode::parse(&arg_str(step, "filter"));
                rt.block_on(page::read(conn, refs, budget, mode))
            }
            "browser_find" => {
                let limit = arg_i64(step, "limit")
                    .filter(|n| *n > 0)
                    .map_or(20, |n| n as usize);
                rt.block_on(page::find(conn, refs, &arg_str(step, "text"), limit))
            }
            "browser_click" => {
                let r = arg_str(step, "ref");
                if r.is_empty() {
                    return Err(BrowserError::invalid("browser_click needs ref (like e3)"));
                }
                rt.block_on(page::click(conn, refs, &r, approved))
            }
            "browser_type" => {
                let r = arg_str(step, "ref");
                if r.is_empty() {
                    return Err(BrowserError::invalid("browser_type needs ref and text"));
                }
                // the text is taken as given, not trimmed
                let text = match step.get("text") {
                    Some(Value::String(s)) => s.clone(),
                    _ => return Err(BrowserError::invalid("browser_type needs text")),
                };
                rt.block_on(page::type_text(conn, refs, &r, &text, approved))
            }
            "browser_scroll" => {
                let dir = arg_str(step, "scroll_direction");
                let amount = arg_i64(step, "scroll_amount").unwrap_or(3);
                rt.block_on(page::scroll(
                    conn,
                    refs,
                    &arg_str(step, "ref"),
                    &dir,
                    amount,
                ))
            }
            "browser_screenshot" => rt.block_on(page::screenshot(conn)),
            _ => unreachable!("checked above"),
        })
    }

    /// Build the runtime on first use.
    fn state_guard(&self) -> Result<std::sync::MutexGuard<'_, Option<State>>> {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| {
                    BrowserError::failed(format!("cannot start the async runtime: {e}"))
                })?;
            *g = Some(State {
                rt,
                live: None,
                refs: Refs::default(),
                current: None,
                opened: HashSet::new(),
            });
        }
        Ok(g)
    }

    /// Connect to the current tab (reusing the open connection) and run `f` on it. A failure drops
    /// the connection so the next call reconnects.
    fn with_page<T>(
        &self,
        f: impl FnOnce(&Runtime, &mut Conn, &mut Refs) -> Result<T>,
    ) -> Result<T> {
        let port = self.port()?;
        let mut guard = self.state_guard()?;
        let st = guard.as_mut().expect("state built above");
        let tabs = http::list_tabs(port)?;
        let tab = st
            .current
            .as_ref()
            .and_then(|id| tabs.iter().find(|t| &t.id == id))
            .or_else(|| tabs.first())
            .ok_or_else(|| BrowserError::failed("Brave has no open tab to drive"))?;
        let stale = st.live.as_ref().is_none_or(|l| l.target != tab.id);
        if stale {
            st.refs.clear();
            st.live = None;
            let conn = st.rt.block_on(Conn::connect(&tab.ws_url))?;
            st.live = Some(Live {
                target: tab.id.clone(),
                conn,
            });
            st.current = Some(tab.id.clone());
        }
        let live = st.live.as_mut().expect("connected above");
        let out = f(&st.rt, &mut live.conn, &mut st.refs);
        if let Err(BrowserError::Failed(_)) = &out {
            // the socket may be dead; reconnect next time (refs go with it)
            st.live = None;
            st.refs.clear();
        }
        out
    }

    fn status(&self) -> Result<StepOut> {
        let state = match self.fixed_port {
            Some(port) => launcher::BraveState::Debugging { port },
            None => launcher::state()?,
        };
        let text = match state {
            launcher::BraveState::NotRunning => {
                "Brave is not running. browser_launch starts it in debug mode.".to_string()
            }
            launcher::BraveState::RunningWithoutDebugging => launcher::RELAUNCH_NEEDED.to_string(),
            launcher::BraveState::DebuggingExposed { port, address } => format!(
                "Brave has a debug port {port} bound to {address}, not loopback. Not attaching."
            ),
            launcher::BraveState::Debugging { port } => {
                let tabs = http::list_tabs(port)?;
                format!(
                    "Brave debug mode on 127.0.0.1:{port}, {} tab(s) open.",
                    tabs.len()
                )
            }
        };
        Ok(StepOut {
            text,
            images: Vec::new(),
        })
    }

    fn launch(&self) -> Result<StepOut> {
        let dir = std::env::var("MACHANDS_BRAVE_USER_DATA_DIR").ok();
        let port = match self.fixed_port {
            Some(p) => p,
            None => launcher::launch(dir.as_deref())?,
        };
        Ok(StepOut {
            text: format!(
                "Brave is in debug mode on 127.0.0.1:{port}. Debugging stays on until Brave is quit."
            ),
            images: Vec::new(),
        })
    }

    fn tabs(&self, step: &Map<String, Value>, approved: Option<&str>) -> Result<StepOut> {
        let port = self.port()?;
        let mut guard = self.state_guard()?;
        let st = guard.as_mut().expect("state built above");
        let tabs = http::list_tabs(port)?;
        let op = arg_str(step, "op").to_lowercase();
        let pick = |step: &Map<String, Value>| -> Result<Option<http::Tab>> {
            if let Some(n) = arg_i64(step, "nth") {
                let i =
                    usize::try_from(n - 1).map_err(|_| BrowserError::invalid("nth starts at 1"))?;
                return tabs.get(i).cloned().map(Some).ok_or_else(|| {
                    BrowserError::invalid(format!("no tab {n}; browser_tabs lists them"))
                });
            }
            let q = arg_str(step, "text").to_lowercase();
            if q.is_empty() {
                return Ok(None);
            }
            Ok(tabs
                .iter()
                .find(|t| t.title.to_lowercase().contains(&q) || t.url.to_lowercase().contains(&q))
                .cloned())
        };
        match op.as_str() {
            "" | "list" => {
                let cur = st
                    .current
                    .clone()
                    .or_else(|| tabs.first().map(|t| t.id.clone()));
                let lines: Vec<String> = tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let mark = if Some(&t.id) == cur.as_ref() {
                            "*"
                        } else {
                            " "
                        };
                        let title: String = t.title.chars().take(80).collect();
                        format!(
                            "[{}]{mark} {title} - {}",
                            i + 1,
                            t.url.chars().take(120).collect::<String>()
                        )
                    })
                    .collect();
                Ok(StepOut {
                    text: format!(
                        "{} tab(s); * = the tab browser_* actions drive\n{}",
                        tabs.len(),
                        page::untrusted(&lines.join("\n"))
                    ),
                    images: Vec::new(),
                })
            }
            "switch" => {
                let tab = pick(step)?.ok_or_else(|| {
                    BrowserError::invalid("switch needs nth (1-based, from the list) or text (part of the title or URL)")
                })?;
                st.current = Some(tab.id.clone());
                st.live = None;
                st.refs.clear();
                if arg_flag(step, "wake") {
                    http::request(
                        port,
                        "GET",
                        &format!("/json/activate/{}", tab.id),
                        Duration::from_secs(3),
                    )?;
                }
                Ok(StepOut {
                    text: format!(
                        "now driving tab: {}",
                        page::untrusted(&format!("{} - {}", tab.title, tab.url))
                    ),
                    images: Vec::new(),
                })
            }
            "new" => {
                let url = gate::check_url_with(&arg_str(step, "url"), approved)?;
                let body = http::request(
                    port,
                    "PUT",
                    &format!("/json/new?{}", http::encode_query(&url)),
                    Duration::from_secs(5),
                )?;
                let v: Value = serde_json::from_str(&body)
                    .map_err(|e| BrowserError::failed(format!("bad /json/new answer: {e}")))?;
                let id = v
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if id.is_empty() {
                    return Err(BrowserError::failed(
                        "the browser did not say which tab it opened",
                    ));
                }
                st.opened.insert(id.clone());
                st.current = Some(id);
                st.live = None;
                st.refs.clear();
                Ok(StepOut {
                    text: format!("opened a new tab at {} and now driving it", host_of(&url)),
                    images: Vec::new(),
                })
            }
            "close" => {
                let tab = pick(step)?.or_else(|| {
                    st.current
                        .as_ref()
                        .and_then(|id| tabs.iter().find(|t| &t.id == id).cloned())
                });
                let tab = tab.ok_or_else(|| BrowserError::invalid("close needs nth or text"))?;
                if !st.opened.contains(&tab.id) {
                    // closing the user's own tab: a card, not a flat no
                    let need = Need::new(
                        Kind::BrowserCloseForeignTab,
                        tab.id.clone(),
                        "this tab was not opened by Mac hands",
                        BrowserError::refused(
                            "I only close tabs I opened myself, and this one is the user's",
                        )
                        .message(),
                    );
                    let title: String = tab.title.chars().take(60).collect();
                    let mut need = need.doing(
                        format!("Close the tab \"{title}\""),
                        "Close a tab".to_string(),
                    );
                    let host = host_of(&tab.url);
                    if !host.is_empty() {
                        need.place = format!("on {host}");
                    }
                    if !need.approvable() {
                        return Err(BrowserError::Refused(need.reason));
                    }
                    if approved != Some(need.key().as_str()) {
                        return Err(BrowserError::Approval(Box::new(need)));
                    }
                }
                http::request(
                    port,
                    "GET",
                    &format!("/json/close/{}", tab.id),
                    Duration::from_secs(3),
                )?;
                st.opened.remove(&tab.id);
                if st.current.as_deref() == Some(tab.id.as_str()) {
                    st.current = None;
                    st.live = None;
                    st.refs.clear();
                }
                Ok(StepOut {
                    text: "closed the tab".into(),
                    images: Vec::new(),
                })
            }
            other => Err(BrowserError::invalid(format!(
                "unknown tabs op {other}; one of: list, switch, new, close"
            ))),
        }
    }
}
