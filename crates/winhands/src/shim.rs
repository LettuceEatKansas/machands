//! `winhands mcp`: the MCP stdio server (Python `winhands/shim.py`).
//!
//! Thin and non-elevated. It advertises ONE tool, `computer`, exactly as the Python shim does, forwards
//! each call to the daemon over HTTP (which keeps the elevated-daemon split on Windows), and turns the
//! answer back into MCP content. It starts the daemon on demand when nothing answers and retries once.
//! A reaper thread makes it leave when its host is gone or has replaced it.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
    ElicitationAction, Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{ElicitationMode, Peer, RequestContext, ServiceError};
use rmcp::{ErrorData as McpError, RoleServer, ServiceExt};
use serde_json::{Map, Value, json};
use wh_core::approval::{CARD_WAIT, Offer, card_message};
use wh_core::mcp::{
    self, Block, Reply, ShimDefaults, TOOL_DESCRIPTION, TOOL_NAME, build_payload,
    reply_from_daemon, timeout_message,
};

use crate::link::{DaemonLink, Failure};
use crate::registry::{ProcTable, Reaper, Registry, ShimRecord, normcase, spawner_pid};

/// When the shim last handled an MCP request (a long call counts as activity until it ends).
#[derive(Clone)]
pub struct Activity(Arc<Mutex<Instant>>);

impl Default for Activity {
    fn default() -> Self {
        Self::new()
    }
}

impl Activity {
    /// Starts at launch.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// A request arrived or finished.
    pub fn touch(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Time since the last request.
    pub fn idle(&self) -> Duration {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).elapsed()
    }
}

/// The one tool, as `list_tools` serves it.
pub fn tool_definition() -> Tool {
    Tool::new(TOOL_NAME, TOOL_DESCRIPTION, Arc::new(mcp::input_schema()))
}

/// One trip to the daemon: the answer as MCP content, and the approval the daemon offered when a
/// step stopped for a yes from the user.
pub struct Exchange {
    /// The answer, exactly as it has always been (a stopped step reads as its refusal).
    pub reply: Reply,
    /// A single-use token and the card sentence, only when the call said it could show a card.
    pub offer: Option<Offer>,
}

impl Exchange {
    fn plain(reply: Reply) -> Self {
        Self { reply, offer: None }
    }
}

/// The `approval` object of a daemon answer body, if there is one.
fn offer_in(body: &[u8]) -> Option<Offer> {
    let v: Value = serde_json::from_slice(body).ok()?;
    Offer::from_json(v.get("approval")?)
}

/// The daemon, as far as the card loop cares: send one payload, get one [`Exchange`] back.
pub trait Daemon: Send + Sync + 'static {
    /// One request to `/computer` with its start-on-demand and retry, blocking until it is answered.
    fn exchange(&self, payload: &Value) -> Exchange;
}

impl Daemon for DaemonLink {
    fn exchange(&self, payload: &Value) -> Exchange {
        match self.post(TOOL_NAME, payload) {
            Ok(reply) => Exchange {
                offer: if reply.status == 200 {
                    offer_in(&reply.body)
                } else {
                    None
                },
                reply: reply_from_daemon(TOOL_NAME, reply.status, &reply.body),
            },
            Err(Failure::Timeout) => Exchange::plain(Reply::error(timeout_message(TOOL_NAME))),
            Err(Failure::NoDaemon) => Exchange::plain(Reply::error(format!(
                "winhands daemon unreachable and could not be started automatically. Start it with: {}.",
                self.start_hint()
            ))),
            Err(Failure::Io(why)) => {
                Exchange::plain(Reply::error(format!("winhands {TOOL_NAME} failed: {why}")))
            }
        }
    }
}

/// One MCP call to the daemon and back: the payload the Python shim builds, the request with its
/// start-on-demand and one retry, and the answer as MCP content. Never asks for a card.
pub fn forward(link: &DaemonLink, defaults: &ShimDefaults, args: &Map<String, Value>) -> Reply {
    match build_payload(args, defaults) {
        Ok(payload) => link.exchange(&payload).reply,
        Err(e) => Reply::error(format!("Error executing tool {TOOL_NAME}: {e}")),
    }
}

// ------------------------------------------------------------------------------ approval cards

/// How the user answered a card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Accepted the form with "Allow".
    Allow,
    /// Chose "Deny", or declined or cancelled the form.
    Deny,
    /// Nobody answered in [`CARD_WAIT`].
    TimedOut,
    /// The client could not show it (an error, or it went away).
    Unavailable,
}

impl Decision {
    /// The word for the log.
    pub fn word(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::TimedOut => "timeout",
            Decision::Unavailable => "unavailable",
        }
    }
}

/// The card's form: one choice, Deny until the user picks otherwise. MCP form mode allows only flat
/// primitive fields, so this is a single string with two allowed values.
pub fn card_schema() -> Value {
    json!({
        "type": "object",
        "title": "Mac hands needs your OK",
        "properties": {
            "decision": {
                "type": "string",
                "title": "Allow this one action?",
                "description": "Allow runs only this exact action, once.",
                "enum": ["Allow", "Deny"],
                "default": "Deny"
            }
        },
        "required": ["decision"]
    })
}

/// The `elicitation/create` params for a card.
pub fn card_params(offer: &Offer) -> Option<ElicitRequestParams> {
    serde_json::from_value(json!({
        "mode": "form",
        "message": card_message(&offer.summary),
        "requestedSchema": card_schema(),
    }))
    .ok()
}

/// the user's answer from the elicitation result: only an accepted form that says "Allow" is a yes.
pub fn decision_from(action: &ElicitationAction, content: Option<&Value>) -> Decision {
    match action {
        ElicitationAction::Accept => {
            let said = content
                .and_then(|c| c.get("decision"))
                .and_then(Value::as_str);
            if said == Some("Allow") {
                Decision::Allow
            } else {
                Decision::Deny
            }
        }
        _ => Decision::Deny,
    }
}

/// Something that can put a card in front of the user.
pub trait Asker: Send + Sync {
    /// Show `offer` and wait for the answer.
    fn ask(&self, offer: &Offer) -> impl std::future::Future<Output = Decision> + Send;
}

/// Asks through the MCP client that is calling: `elicitation/create`, form mode.
pub struct PeerAsker(pub Peer<RoleServer>);

impl PeerAsker {
    /// Has the client declared the elicitation capability (form mode)?
    pub fn supported(peer: &Peer<RoleServer>) -> bool {
        peer.supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
    }
}

impl Asker for PeerAsker {
    async fn ask(&self, offer: &Offer) -> Decision {
        let Some(params) = card_params(offer) else {
            return Decision::Unavailable;
        };
        match self
            .0
            .create_elicitation_with_timeout(params, Some(CARD_WAIT))
            .await
        {
            Ok(result) => decision_from(&result.action, result.content.as_ref()),
            Err(ServiceError::Timeout { .. }) => Decision::TimedOut,
            Err(_) => Decision::Unavailable,
        }
    }
}

/// Where cards and answers are written: the browser module's local log (one JSON line each, no typed
/// text, nothing leaves the machine).
pub struct CardLog(wh_browser::log::ActionLog);

impl CardLog {
    /// The default directory (`MACHANDS_LOG_DIR` overrides it).
    pub fn standard() -> Self {
        Self(wh_browser::log::ActionLog::new(
            wh_browser::log::default_dir(),
        ))
    }

    /// Log into `dir` (`None` logs nothing).
    pub fn in_dir(dir: Option<std::path::PathBuf>) -> Self {
        Self(wh_browser::log::ActionLog::new(dir))
    }

    fn card(&self, offer: &Offer) {
        self.0.write(
            "approval_card",
            json!({"kind": offer.kind, "step": offer.step, "card": offer.log_summary}),
            true,
        );
    }

    /// the user's answer to a chat code (`winhands approve|deny CODE`). `taken` = the daemon accepted it.
    /// The code itself is logged (single use, ten minutes), never the step.
    pub fn code_answer(&self, code: &str, allow: bool, taken: bool) {
        self.0.write(
            "approval_answer",
            json!({
                "via": "chat code",
                "code": code,
                "answer": if allow { "allow" } else { "deny" },
                "taken": taken,
            }),
            allow && taken,
        );
    }

    fn answer(&self, offer: &Offer, decision: Decision, waited: Duration) {
        self.0.write(
            "approval_answer",
            json!({
                "kind": offer.kind,
                "step": offer.step,
                "answer": decision.word(),
                "waited_ms": u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
            }),
            decision == Decision::Allow,
        );
    }
}

/// Cards one call may show. A batch with this many risky steps is not a batch to wave through.
const MAX_CARDS_PER_CALL: usize = 8;

/// What Claude reads after the user said no.
pub const DECLINED_NOTE: &str = "The user declined this in the approval card, so it was not done. Do not retry it or look for another way to do it; if it still matters, ask them in chat.";

/// What Claude reads when the card went unanswered.
pub const TIMED_OUT_NOTE: &str = "The approval card got no answer within 120 s, so this was not done. Do not retry it; ask the user in chat if it matters.";

/// One call, with approval cards. `payload` goes to the daemon; when the daemon answers that a step
/// is waiting for a yes and `asker` is present, the user is asked. On Allow the same payload goes again
/// with the token and the daemon carries on from that step. Every other outcome leaves the first
/// answer, the plain refusal, as the result (plus a line telling Claude not to retry when the user said
/// no).
pub async fn run_call<A: Asker>(
    daemon: &Arc<dyn Daemon>,
    asker: Option<&A>,
    mut payload: Value,
    cards: &CardLog,
) -> Reply {
    if asker.is_some()
        && let Some(map) = payload.as_object_mut()
    {
        map.insert("approvable".into(), Value::Bool(true));
    }
    let mut shown = 0;
    loop {
        let sent = payload.clone();
        let d = Arc::clone(daemon);
        let exchange = tokio::task::spawn_blocking(move || guarded_exchange(|| d.exchange(&sent)))
            .await
            .unwrap_or_else(|e| {
                Exchange::plain(Reply::error(format!("winhands shim failed: {e}")))
            });
        let (Some(asker), Some(offer)) = (asker, exchange.offer.as_ref()) else {
            return exchange.reply;
        };
        if shown >= MAX_CARDS_PER_CALL {
            return exchange.reply;
        }
        shown += 1;
        cards.card(offer);
        let asked = Instant::now();
        let decision = asker.ask(offer).await;
        cards.answer(offer, decision, asked.elapsed());
        match decision {
            Decision::Allow => {
                if let Some(map) = payload.as_object_mut() {
                    map.insert("approval_token".into(), Value::from(offer.token.clone()));
                }
            }
            Decision::Deny => return with_note(exchange.reply, DECLINED_NOTE),
            Decision::TimedOut => return with_note(exchange.reply, TIMED_OUT_NOTE),
            Decision::Unavailable => return exchange.reply,
        }
    }
}

fn guarded_exchange(call: impl FnOnce() -> Exchange) -> Exchange {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(e) => e,
        Err(payload) => Exchange::plain(Reply::error(format!(
            "winhands shim failed: internal error: {}",
            wh_core::daemon::panic_message(payload.as_ref())
        ))),
    }
}

fn with_note(mut reply: Reply, note: &str) -> Reply {
    reply.blocks.push(Block::Text(note.to_string()));
    reply
}

/// Run one tool call, turning a panic into an error reply so the MCP server keeps answering. Nothing
/// in `forward` is meant to panic; this is the net under it (and under `build_payload`).
pub fn guarded(call: impl FnOnce() -> Reply) -> Reply {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(reply) => reply,
        Err(payload) => Reply::error(format!(
            "winhands shim failed: internal error: {}",
            wh_core::daemon::panic_message(payload.as_ref())
        )),
    }
}

/// The payload for a call, or the error reply when an argument has the wrong type. A panic building
/// it is an error reply too.
fn guarded_payload(defaults: &ShimDefaults, args: &Map<String, Value>) -> Result<Value, Reply> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_payload(args, defaults)
    })) {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(e)) => Err(Reply::error(format!(
            "Error executing tool {TOOL_NAME}: {e}"
        ))),
        Err(payload) => Err(Reply::error(format!(
            "winhands shim failed: internal error: {}",
            wh_core::daemon::panic_message(payload.as_ref())
        ))),
    }
}

fn to_result(reply: Reply) -> CallToolResult {
    let blocks = reply
        .blocks
        .into_iter()
        .map(|b| match b {
            Block::Text(t) => ContentBlock::text(t),
            Block::Image { data, mime } => ContentBlock::image(data, mime),
        })
        .collect();
    if reply.is_error {
        CallToolResult::error(blocks)
    } else {
        CallToolResult::success(blocks)
    }
}

/// The server handler.
pub struct Shim {
    daemon: Arc<dyn Daemon>,
    defaults: ShimDefaults,
    activity: Activity,
    cards: Arc<CardLog>,
}

impl Shim {
    /// A shim forwarding over `link`, adding `defaults` to every call.
    pub fn new(link: Arc<DaemonLink>, defaults: ShimDefaults, activity: Activity) -> Self {
        Self::over(link, defaults, activity, CardLog::standard())
    }

    /// A shim over any [`Daemon`], writing its card log with `cards`.
    pub fn over(
        daemon: Arc<dyn Daemon>,
        defaults: ShimDefaults,
        activity: Activity,
        cards: CardLog,
    ) -> Self {
        Self {
            daemon,
            defaults,
            activity,
            cards: Arc::new(cards),
        }
    }
}

impl ServerHandler for Shim {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(mcp::SERVER_NAME, env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.activity.touch();
        Ok(ListToolsResult::with_all_items(vec![tool_definition()]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.activity.touch();
        let reply = if request.name != TOOL_NAME {
            Reply::error(format!("Unknown tool: {}", request.name))
        } else {
            let args = request.arguments.unwrap_or_default();
            match guarded_payload(&self.defaults, &args) {
                Err(reply) => reply,
                Ok(payload) => {
                    // The daemon call blocks for as long as the action takes, so it runs off the MCP
                    // loop (inside run_call). A client that declared elicitation gets a card instead
                    // of a flat refusal for the steps the user can approve.
                    if PeerAsker::supported(&context.peer) {
                        let asker = PeerAsker(context.peer.clone());
                        run_call(&self.daemon, Some(&asker), payload, &self.cards).await
                    } else {
                        run_call::<PeerAsker>(&self.daemon, None, payload, &self.cards).await
                    }
                }
            }
        };
        self.activity.touch();
        Ok(to_result(reply).into())
    }
}

/// Everything the reaper thread needs, resolved once at start-up.
pub struct ReaperSetup {
    /// The decision logic and the records.
    pub reaper: Reaper,
    /// Reads the live process table.
    pub table: Box<dyn Fn() -> Option<ProcTable> + Send>,
    /// Shared with the handler.
    pub activity: Activity,
}

/// How often the reaper looks (`time.sleep(30)`).
pub const REAPER_PERIOD: Duration = Duration::from_secs(30);

/// Removes this shim's record when it exits normally (the reaper removes it when it exits abnormally).
pub struct Cleanup {
    registry: Registry,
    pid: u32,
}

impl Cleanup {
    /// Remove the record.
    pub fn run(&self) {
        self.registry.unregister(self.pid);
    }
}

/// Register this shim and start the reaper thread. Best effort: any failure only means this shim is
/// never the one chosen to leave.
pub fn start_reaper(setup: ReaperSetup) -> Cleanup {
    let ReaperSetup {
        reaper,
        table,
        activity,
    } = setup;
    let cleanup = Cleanup {
        registry: reaper.registry.clone(),
        pid: reaper.me,
    };
    if let Some(t) = table() {
        let created = t.created_of(reaper.me);
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        reaper.registry.register(&ShimRecord {
            pid: reaper.me,
            spawner: spawner_pid(&t, reaper.me, &reaper.own_image),
            created,
            started,
        });
    }
    let _ = std::thread::Builder::new()
        .name("winhands-reaper".into())
        .spawn(move || {
            loop {
                std::thread::sleep(REAPER_PERIOD);
                let Some(t) = table() else {
                    continue; // never leave on a detection failure
                };
                if reaper.should_exit(&t, activity.idle()) {
                    reaper.registry.unregister(reaper.me);
                    std::process::exit(0);
                }
            }
        });
    cleanup
}

/// Build the reaper for this process over `backend`'s process table. `None` when there is no backend
/// to read it from (an OS winhands cannot drive).
pub fn reaper_for(
    backend: Arc<dyn wh_core::backend::Backend>,
    cfg: &wh_core::config::Config,
    activity: Activity,
) -> Option<ReaperSetup> {
    let exe = std::env::current_exe().ok()?;
    let own_name = exe.file_name()?.to_string_lossy().to_lowercase();
    let own_image = normcase(&exe.to_string_lossy());
    let reaper = Reaper {
        registry: Registry::new(cfg.paths().shims_dir()),
        me: std::process::id(),
        own_name,
        own_image,
        idle_limit: Duration::from_secs(u64::try_from(cfg.shim_idle_s()).unwrap_or(0)),
    };
    Some(ReaperSetup {
        reaper,
        table: Box::new(move || backend.process_table().ok().map(ProcTable::new)),
        activity,
    })
}

/// Serve MCP on stdin and stdout until the client hangs up.
pub fn serve_stdio(shim: Shim) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    runtime.block_on(async {
        let running = shim
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| format!("MCP start-up failed: {e}"))?;
        running
            .waiting()
            .await
            .map(|_| ())
            .map_err(|e| format!("MCP server stopped: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wh_core::paths::Paths;

    fn dead_link() -> DaemonLink {
        // a port nothing listens on, and no way to start anything
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        DaemonLink::new(
            "127.0.0.1",
            port,
            Paths::in_dir(std::env::temp_dir().join("wh-shim-none")),
            None,
            Duration::from_secs(1),
        )
    }

    fn defaults() -> ShimDefaults {
        ShimDefaults {
            coords: "pixels".into(),
            only_apps: Vec::new(),
        }
    }

    #[test]
    fn the_tool_is_the_python_tool() {
        let t = tool_definition();
        assert_eq!(t.name, "computer");
        assert_eq!(t.description.as_deref(), Some(TOOL_DESCRIPTION));
        assert_eq!(
            Value::Object((*t.input_schema).clone()),
            Value::Object(mcp::input_schema())
        );
    }

    #[test]
    fn serialised_the_tool_matches_the_python_list_tools_entry() {
        let ours = serde_json::to_value(tool_definition()).unwrap();
        let python: Value =
            serde_json::from_str(include_str!("../../../tests/parity/python_list_tools.json"))
                .unwrap();
        // 2026-10-11: the browser module appended a paragraph and two parameters (ref, url); the
        // Python text and the Python parameters are still there unchanged, in order
        let mut keys: Vec<&String> = ours.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            ["description", "inputSchema", "name"],
            "and nothing else"
        );
        assert_eq!(ours["name"], python[0]["name"]);
        let py_desc = python[0]["description"].as_str().unwrap();
        assert!(ours["description"].as_str().unwrap().starts_with(py_desc));
        let ours_props = ours["inputSchema"]["properties"].as_object().unwrap();
        let py_props = python[0]["inputSchema"]["properties"].as_object().unwrap();
        for (i, (k, v)) in py_props.iter().enumerate() {
            assert_eq!(ours_props.keys().nth(i), Some(k));
            assert_eq!(&ours_props[k], v);
        }
    }

    #[test]
    fn a_bad_argument_is_an_error_reply_not_a_panic() {
        let r = forward(
            &dead_link(),
            &defaults(),
            json!({"coordinate": "no"}).as_object().unwrap(),
        );
        assert!(r.is_error);
        let Block::Text(t) = &r.blocks[0] else {
            panic!("text")
        };
        assert!(
            t.starts_with("Error executing tool computer:") && t.contains("coordinate"),
            "{t}"
        );
    }

    #[test]
    fn no_daemon_and_no_way_to_start_one_is_a_clear_error() {
        let r = forward(
            &dead_link(),
            &defaults(),
            json!({"action": "screenshot"}).as_object().unwrap(),
        );
        assert!(r.is_error);
        let Block::Text(t) = &r.blocks[0] else {
            panic!("text")
        };
        assert!(
            t.contains("unreachable") && t.contains("Start it with:"),
            "{t}"
        );
    }

    #[test]
    fn activity_measures_idle_time_and_touch_resets_it() {
        let a = Activity::new();
        std::thread::sleep(Duration::from_millis(30));
        assert!(a.idle() >= Duration::from_millis(25));
        a.touch();
        assert!(a.idle() < Duration::from_millis(25));
    }

    #[test]
    fn replies_become_mcp_results_with_the_error_flag() {
        let ok = serde_json::to_value(to_result(Reply {
            blocks: vec![
                Block::Text("hi".into()),
                Block::Image {
                    data: "AAAA".into(),
                    mime: "image/png".into(),
                },
            ],
            is_error: false,
        }))
        .unwrap();
        assert_eq!(ok["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(
            ok["content"][1],
            json!({"type": "image", "data": "AAAA", "mimeType": "image/png"})
        );
        assert_eq!(ok["isError"], false);
        let bad = serde_json::to_value(to_result(Reply::error("no"))).unwrap();
        assert_eq!(bad["isError"], true);
    }

    // ------------------------------------------------------------------ approval cards

    use std::collections::VecDeque;

    const REFUSAL: &str = "REFUSED by the browser safety gate: clicking \"Search\" would submit a form. Ask the user in chat and wait for a clear yes. Do not retry or work around this.";

    fn test_offer() -> Offer {
        Offer {
            token: "tok1".into(),
            summary: "Click \"Search\" (submits a form) on en.wikipedia.org".into(),
            log_summary: "Click \"Search\" (submits a form) on en.wikipedia.org".into(),
            kind: "form_submit".into(),
            step: 1,
        }
    }

    /// A daemon that stops with a card offer until the payload carries the token.
    #[derive(Default)]
    struct FakeDaemon {
        payloads: Mutex<Vec<Value>>,
    }

    impl Daemon for FakeDaemon {
        fn exchange(&self, payload: &Value) -> Exchange {
            self.payloads.lock().unwrap().push(payload.clone());
            if payload.get("approval_token").and_then(Value::as_str) == Some("tok1") {
                Exchange::plain(Reply {
                    blocks: vec![Block::Text("clicked e1".into())],
                    is_error: false,
                })
            } else {
                let offer = payload
                    .get("approvable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(test_offer);
                Exchange {
                    reply: Reply {
                        blocks: vec![Block::Text(REFUSAL.into())],
                        is_error: false,
                    },
                    offer,
                }
            }
        }
    }

    struct FixedAsker(Mutex<VecDeque<Decision>>, Mutex<Vec<String>>);

    impl FixedAsker {
        fn saying(d: &[Decision]) -> Self {
            Self(Mutex::new(d.iter().copied().collect()), Mutex::default())
        }
    }

    impl Asker for FixedAsker {
        async fn ask(&self, offer: &Offer) -> Decision {
            self.1.lock().unwrap().push(offer.summary.clone());
            self.0.lock().unwrap().pop_front().unwrap_or(Decision::Deny)
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn texts(r: &Reply) -> Vec<String> {
        r.blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.clone()),
                Block::Image { .. } => None,
            })
            .collect()
    }

    fn log_lines(dir: &std::path::Path) -> Vec<Value> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            for l in std::fs::read_to_string(e.path()).unwrap().lines() {
                out.push(serde_json::from_str(l).unwrap());
            }
        }
        out
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wh-cards-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn allow_resends_with_the_token_and_logs_card_and_answer() {
        let dir = scratch("allow");
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let asker = FixedAsker::saying(&[Decision::Allow]);
        let cards = CardLog::in_dir(Some(dir.clone()));
        let r = rt().block_on(run_call(
            &daemon,
            Some(&asker),
            json!({"action": "browser_click", "ref": "e1"}),
            &cards,
        ));
        assert_eq!(texts(&r), ["clicked e1"]);
        let sent = fake.payloads.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["approvable"], true);
        assert!(sent[0].get("approval_token").is_none());
        assert_eq!(sent[1]["approval_token"], "tok1");
        assert_eq!(sent[1]["ref"], "e1");
        assert_eq!(
            asker.1.lock().unwrap().as_slice(),
            ["Click \"Search\" (submits a form) on en.wikipedia.org"]
        );
        let lines = log_lines(&dir);
        assert!(lines.iter().any(|l| l["action"] == "approval_card"));
        let ans = lines
            .iter()
            .find(|l| l["action"] == "approval_answer")
            .unwrap();
        assert_eq!(ans["detail"]["answer"], "allow");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn deny_and_timeout_keep_the_refusal_and_add_a_do_not_retry_note() {
        for (d, note) in [
            (Decision::Deny, DECLINED_NOTE),
            (Decision::TimedOut, TIMED_OUT_NOTE),
        ] {
            let fake = Arc::new(FakeDaemon::default());
            let daemon: Arc<dyn Daemon> = fake.clone();
            let asker = FixedAsker::saying(&[d]);
            let r = rt().block_on(run_call(
                &daemon,
                Some(&asker),
                json!({"action": "browser_click", "ref": "e1"}),
                &CardLog::in_dir(None),
            ));
            assert_eq!(texts(&r), [REFUSAL, note], "{d:?}");
            assert_eq!(fake.payloads.lock().unwrap().len(), 1, "no resend");
        }
    }

    #[test]
    fn an_unavailable_card_leaves_the_refusal_exactly_as_it_was() {
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let asker = FixedAsker::saying(&[Decision::Unavailable]);
        let r = rt().block_on(run_call(
            &daemon,
            Some(&asker),
            json!({"action": "browser_click", "ref": "e1"}),
            &CardLog::in_dir(None),
        ));
        assert_eq!(texts(&r), [REFUSAL]);
    }

    #[test]
    fn without_an_asker_nothing_is_marked_approvable() {
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let r = rt().block_on(run_call::<FixedAsker>(
            &daemon,
            None,
            json!({"action": "browser_click", "ref": "e1"}),
            &CardLog::in_dir(None),
        ));
        assert_eq!(texts(&r), [REFUSAL]);
        assert!(fake.payloads.lock().unwrap()[0].get("approvable").is_none());
    }

    #[test]
    fn only_an_accepted_allow_is_a_yes() {
        let allow = json!({"decision": "Allow"});
        let deny = json!({"decision": "Deny"});
        assert_eq!(
            decision_from(&ElicitationAction::Accept, Some(&allow)),
            Decision::Allow
        );
        assert_eq!(
            decision_from(&ElicitationAction::Accept, Some(&deny)),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Accept, None),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Decline, Some(&allow)),
            Decision::Deny
        );
        assert_eq!(
            decision_from(&ElicitationAction::Cancel, Some(&allow)),
            Decision::Deny
        );
    }

    #[test]
    fn the_card_is_a_form_with_an_allow_deny_choice_defaulting_to_deny() {
        let p = serde_json::to_value(card_params(&test_offer()).unwrap()).unwrap();
        assert_eq!(p["mode"], "form");
        assert_eq!(
            p["message"],
            "Mac hands wants to: Click \"Search\" (submits a form) on en.wikipedia.org"
        );
        assert_eq!(
            p["requestedSchema"]["properties"]["decision"]["enum"],
            json!(["Allow", "Deny"])
        );
        assert_eq!(
            p["requestedSchema"]["properties"]["decision"]["default"],
            "Deny"
        );
    }

    // A real MCP client on the other end of a pipe: it declares (or not) the elicitation capability,
    // answers the card the way the test says, and reports what the tool call returned.
    struct Run {
        result: Value,
        payloads: Vec<Value>,
        cards: Vec<Value>,
    }

    fn drive(capabilities: Value, answer: Option<Value>) -> Run {
        drive_args(
            capabilities,
            answer,
            json!({"action": "browser_click", "ref": "e1"}),
        )
    }

    fn drive_args(capabilities: Value, answer: Option<Value>, arguments: Value) -> Run {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let fake = Arc::new(FakeDaemon::default());
        let daemon: Arc<dyn Daemon> = fake.clone();
        let shim = Shim::over(daemon, defaults(), Activity::new(), CardLog::in_dir(None));
        let (cli, srv) = tokio::io::duplex(1 << 16);
        let (srv_r, srv_w) = tokio::io::split(srv);
        let (cli_r, mut cli_w) = tokio::io::split(cli);
        let (result, cards) = rt().block_on(async move {
            let running = tokio::spawn(async move { shim.serve((srv_r, srv_w)).await });
            let mut lines = BufReader::new(cli_r).lines();
            let send = |v: Value| {
                let mut b = v.to_string().into_bytes();
                b.push(b'\n');
                b
            };
            let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": capabilities,
                "clientInfo": {"name": "fake", "version": "0"}}});
            cli_w.write_all(&send(init)).await.unwrap();
            let _ = lines.next_line().await.unwrap().unwrap();
            cli_w
                .write_all(&send(
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                ))
                .await
                .unwrap();
            let call = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "computer", "arguments": arguments}});
            cli_w.write_all(&send(call)).await.unwrap();
            let mut cards = Vec::new();
            let result = loop {
                let line = lines.next_line().await.unwrap().expect("server closed");
                let v: Value = serde_json::from_str(&line).unwrap();
                if v["method"] == "elicitation/create" {
                    cards.push(v["params"].clone());
                    let reply = match &answer {
                        Some(a) => json!({"jsonrpc": "2.0", "id": v["id"], "result": a}),
                        None => json!({"jsonrpc": "2.0", "id": v["id"],
                                       "error": {"code": -32601, "message": "no"}}),
                    };
                    cli_w.write_all(&send(reply)).await.unwrap();
                } else if v["id"] == 2 {
                    break v["result"].clone();
                }
            };
            drop(cli_w);
            running.abort();
            (result, cards)
        });
        let payloads = fake.payloads.lock().unwrap().clone();
        Run {
            result,
            payloads,
            cards,
        }
    }

    fn result_texts(v: &Value) -> Vec<String> {
        v["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["text"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_client_with_elicitation_gets_a_card_and_allow_carries_on() {
        let r = drive(
            json!({"elicitation": {"form": {}}}),
            Some(json!({"action": "accept", "content": {"decision": "Allow"}})),
        );
        assert_eq!(r.cards.len(), 1);
        assert_eq!(r.cards[0]["mode"], "form");
        assert!(
            r.cards[0]["message"]
                .as_str()
                .unwrap()
                .starts_with("Mac hands wants to: Click")
        );
        assert_eq!(result_texts(&r.result), ["clicked e1"]);
        assert_eq!(r.payloads.len(), 2);
        assert_eq!(r.payloads[1]["approval_token"], "tok1");
    }

    #[test]
    fn a_declined_card_returns_the_refusal_and_the_do_not_retry_note() {
        for answer in [
            json!({"action": "decline"}),
            json!({"action": "cancel"}),
            json!({"action": "accept", "content": {"decision": "Deny"}}),
        ] {
            let r = drive(json!({"elicitation": {"form": {}}}), Some(answer));
            assert_eq!(r.cards.len(), 1);
            assert_eq!(result_texts(&r.result), [REFUSAL, DECLINED_NOTE]);
            assert_eq!(r.payloads.len(), 1, "no resend");
        }
    }

    #[test]
    fn the_approval_code_reaches_the_daemon_and_is_not_marked_approvable() {
        let r = drive_args(
            json!({}),
            None,
            json!({"action": "browser_click", "ref": "e1", "approval_code": "K7QM"}),
        );
        assert_eq!(r.payloads[0]["approval_code"], "K7QM");
        assert!(r.payloads[0].get("approvable").is_none());
        // and inside a batch it still rides top level
        let r = drive_args(
            json!({}),
            None,
            json!({"actions": [{"action": "browser_click", "ref": "e1"}], "approval_code": "K7QM"}),
        );
        assert_eq!(r.payloads[0]["approval_code"], "K7QM");
    }

    #[test]
    fn a_client_without_elicitation_gets_todays_refusal_and_no_approvable_flag() {
        let r = drive(json!({}), None);
        assert!(r.cards.is_empty());
        assert_eq!(result_texts(&r.result), [REFUSAL]);
        assert!(r.payloads[0].get("approvable").is_none());
    }
}
