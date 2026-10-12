//! Approval cards: the "ask the user" path for steps the safety rules would otherwise refuse flat.
//!
//! A refusal that the user could reasonably say yes to (a form submit, a Send button, a typed line break,
//! a destructive command) becomes a [`Need`] instead of a plain error. The daemon turns the first
//! [`Need`] of a call into an offer: a short human sentence plus a single-use token bound to the exact
//! step and target. The MCP shim shows the sentence as an Allow/Deny card; on Allow it sends the same
//! call again with the token, and the daemon runs the call from the step that was waiting. Anything
//! else (Deny, no answer, a client that cannot show a card) leaves the original refusal text as the
//! answer, unchanged.
//!
//! [`Kind::approvable`] is THE list. To make a class of step askable or hard-refused, edit that one
//! `match`. Hard refusals still go through the same gate code, which turns a kind that is not
//! approvable into a plain refusal, so flipping one arm is all it takes.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::destructive::Category;

/// How long a token stays valid after the daemon issued it.
pub const TOKEN_TTL: Duration = Duration::from_secs(300);

/// How long the shim waits for the user to answer a card.
pub const CARD_WAIT: Duration = Duration::from_secs(120);

/// The most characters of typed text a card shows.
pub const TYPED_SHOWN: usize = 80;

/// Offers outstanding at once; the oldest goes first.
const MAX_PENDING: usize = 32;

/// Call arguments that carry the approval itself. They are not part of what a token is bound to.
pub const CONTROL_KEYS: [&str; 5] = [
    "allow_irreversible",
    "allow_destructive",
    "approvable",
    "approval_token",
    "approval_code",
];

/// How long a chat code stays valid after the daemon issued it.
pub const CODE_TTL: Duration = Duration::from_secs(600);

/// The characters of a chat code: no 0, O, 1 or I. Exactly 32, so one random byte picks one.
const CODE_CHARS: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Characters in a chat code.
pub const CODE_LEN: usize = 4;

/// What kind of step wants approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// Browser: a click that submits a form.
    BrowserFormSubmit,
    /// Browser: a button labelled send, buy, pay, delete, accept and the like.
    BrowserRiskyButton,
    /// Browser: typed text that contains a line break (presses Enter).
    BrowserEnterInText,
    /// Browser: closing a tab Mac hands did not open.
    BrowserCloseForeignTab,
    /// Browser: opening something that is not an http(s) page.
    BrowserNonWebUrl,
    /// Desktop: a click on a button named send, buy, delete and the like (`allow_irreversible`).
    DesktopIrreversibleButton,
    /// Desktop: one of the destructive-action rules (`allow_destructive`).
    Destructive(Category),
    /// Browser: typing into a password or payment field. Never.
    PasswordOrPaymentField,
    /// Browser: a CAPTCHA or bot check. Never.
    Captcha,
    /// Browser: a debug port reachable from off this machine. Never.
    DebugPortOffLoopback,
}

impl Kind {
    /// Can the user say yes to this from a card? THE list: edit here to change it.
    pub const fn approvable(self) -> bool {
        match self {
            Kind::BrowserFormSubmit
            | Kind::BrowserRiskyButton
            | Kind::BrowserEnterInText
            | Kind::BrowserCloseForeignTab
            | Kind::BrowserNonWebUrl
            | Kind::DesktopIrreversibleButton
            | Kind::Destructive(_) => true,
            Kind::PasswordOrPaymentField | Kind::Captcha | Kind::DebugPortOffLoopback => false,
        }
    }

    /// A short stable name, part of what a token is bound to and of the log line.
    pub fn tag(self) -> String {
        match self {
            Kind::BrowserFormSubmit => "browser-form-submit".into(),
            Kind::BrowserRiskyButton => "browser-risky-button".into(),
            Kind::BrowserEnterInText => "browser-enter-in-text".into(),
            Kind::BrowserCloseForeignTab => "browser-close-foreign-tab".into(),
            Kind::BrowserNonWebUrl => "browser-non-web-url".into(),
            Kind::DesktopIrreversibleButton => "desktop-irreversible-button".into(),
            Kind::Destructive(c) => format!("destructive-{}", c.label().replace(' ', "-")),
            Kind::PasswordOrPaymentField => "password-or-payment-field".into(),
            Kind::Captcha => "captcha".into(),
            Kind::DebugPortOffLoopback => "debug-port-off-loopback".into(),
        }
    }
}

/// A step that was stopped and could go ahead if the user agrees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// What kind of step.
    pub kind: Kind,
    /// The exact target, as the gate saw it (a button label, a URL). Part of the token binding.
    pub subject: String,
    /// What the step does, in plain words: `Click "Search"`. Empty = the core fills it from the step.
    pub action: String,
    /// The same without anything the user typed, for the log. Empty = same as `action`.
    pub action_log: String,
    /// Why it was stopped, in a few words: `submits a form`.
    pub why: String,
    /// Where: `on en.wikipedia.org` or `in Mail`. Empty when unknown.
    pub place: String,
    /// The full refusal text, exactly what the caller gets when nobody approves.
    pub reason: String,
}

impl Need {
    /// A need with no action text or place yet.
    pub fn new(
        kind: Kind,
        subject: impl Into<String>,
        why: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            subject: subject.into(),
            action: String::new(),
            action_log: String::new(),
            why: why.into(),
            place: String::new(),
            reason: reason.into(),
        }
    }

    /// Set what the step does; `log` is the version safe to write to a log.
    pub fn doing(mut self, action: impl Into<String>, log: impl Into<String>) -> Self {
        self.action = action.into();
        self.action_log = log.into();
        self
    }

    /// Can a card be shown for this?
    pub fn approvable(&self) -> bool {
        self.kind.approvable()
    }

    /// What a token is bound to besides the step itself: the kind and the exact target.
    pub fn key(&self) -> String {
        format!("{}\n{}", self.kind.tag(), self.subject)
    }

    /// The card sentence: `Click "Search" (submits a form) on en.wikipedia.org`.
    pub fn summary(&self) -> String {
        compose(&self.action, &self.why, &self.place)
    }

    /// The same sentence for the log (no typed text).
    pub fn log_summary(&self) -> String {
        let action = if self.action_log.is_empty() {
            &self.action
        } else {
            &self.action_log
        };
        compose(action, &self.why, &self.place)
    }
}

fn compose(action: &str, why: &str, place: &str) -> String {
    let mut out = action.to_string();
    if !why.is_empty() {
        out.push_str(&format!(" ({why})"));
    }
    if !place.is_empty() {
        out.push(' ');
        out.push_str(place);
    }
    out
}

/// The offer a call's answer carries when a step is waiting for a yes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Single use, valid [`TOKEN_TTL`].
    pub token: String,
    /// The card sentence.
    pub summary: String,
    /// The sentence for the log.
    pub log_summary: String,
    /// [`Kind::tag`].
    pub kind: String,
    /// The 1-based step that is waiting.
    pub step: usize,
}

impl Offer {
    /// The `approval` object of the daemon's answer.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "token": self.token,
            "summary": self.summary,
            "log_summary": self.log_summary,
            "kind": self.kind,
            "step": self.step,
        })
    }

    /// Read the `approval` object back (the shim). `None` when it is malformed.
    pub fn from_json(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            token: s("token").filter(|t| !t.is_empty())?,
            summary: s("summary").filter(|t| !t.is_empty())?,
            log_summary: s("log_summary").unwrap_or_default(),
            kind: s("kind").unwrap_or_default(),
            step: v.get("step").and_then(Value::as_u64).unwrap_or(0) as usize,
        })
    }
}

/// What the loop knows when a step stops for approval: the daemon turns it into an [`Offer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// What is waiting.
    pub need: Need,
    /// [`step_id`] of the waiting step.
    pub step_id: String,
    /// Its 1-based position among the call's steps (a recipe counts as its own steps).
    pub index: usize,
    /// The lines of the steps that already ran.
    pub notes: Vec<String>,
    /// The pictures those steps took.
    pub shots: Vec<String>,
}

/// An issued, unused approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// [`step_id`] of the step that may run.
    pub step_id: String,
    /// Its 1-based position.
    pub index: usize,
    /// [`Need::key`].
    pub key: String,
    /// When it stops being valid (the daemon clock).
    pub expires: Duration,
    /// Lines of the steps that ran before it.
    pub notes: Vec<String>,
    /// Their pictures.
    pub shots: Vec<String>,
}

/// Why a token could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeError {
    /// Never issued, or already used.
    Unknown,
    /// Older than [`TOKEN_TTL`].
    Expired,
}

impl TakeError {
    /// The refusal text.
    pub fn message(self) -> &'static str {
        match self {
            TakeError::Unknown => {
                "that approval was already used or never existed, so nothing ran. Ask again by \
                 repeating the original call"
            }
            TakeError::Expired => {
                "that approval expired (they last 5 minutes), so nothing ran. Ask again by \
                 repeating the original call"
            }
        }
    }
}

/// Where a chat code stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeState {
    Waiting,
    Approved,
    Denied,
}

#[derive(Debug, Clone)]
struct CodeEntry {
    entry: Entry,
    state: CodeState,
    /// [`Need::log_summary`], for the one line `winhands approve` prints.
    summary: String,
}

/// Why a chat code could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeError {
    /// Never issued, or already used.
    Unknown,
    /// Older than [`CODE_TTL`].
    Expired,
    /// Issued, but the user has not said yes yet.
    NotYet(String),
    /// the user said no.
    Denied,
}

impl CodeError {
    /// The refusal text.
    pub fn message(&self) -> String {
        match self {
            CodeError::Unknown => "that approval code was already used or never existed, so \
                 nothing ran. Repeat the original call to get a new code"
                .to_string(),
            CodeError::Expired => "that approval code expired (they last 10 minutes), so \
                 nothing ran. Repeat the original call to get a new code"
                .to_string(),
            CodeError::NotYet(code) => {
                format!("The user has not approved {code} yet; wait for their reply")
            }
            CodeError::Denied => "The user denied this; do not retry".to_string(),
        }
    }
}

/// Make a code as typed in chat comparable: trimmed and upper case.
pub fn normalize_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

/// Is this shaped like a chat code (right length, right characters)?
pub fn is_code(code: &str) -> bool {
    code.len() == CODE_LEN
        && code.bytes().all(|b| CODE_CHARS.contains(&b))
        && code.bytes().any(|b| b.is_ascii_digit())
}

/// The refusal text a caller gets when no card can be shown: the one thing to tell the user, and how to
/// carry on once they answered.
pub fn code_refusal(summary: &str, code: &str) -> String {
    format!(
        "Needs the user's OK. Show them this in chat: \"{}\". Ask them to reply \"allow {code}\" (or \
         \"deny {code}\"). After the user replies, repeat this same call with approval_code \"{code}\". \
         Never run winhands approve yourself.",
        card_message(summary)
    )
}

/// The tokens and chat codes the daemon has issued and nobody has used yet.
#[derive(Debug, Default)]
pub struct Store {
    entries: HashMap<String, Entry>,
    codes: HashMap<String, CodeEntry>,
}

impl Store {
    /// Issue a token for `entry` (its `expires` is set here). `None` when no randomness is available,
    /// in which case there is simply no card.
    pub fn mint(&mut self, mut entry: Entry, now: Duration) -> Option<String> {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).ok()?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        self.entries.retain(|_, e| e.expires > now);
        while self.entries.len() >= MAX_PENDING {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(t, _)| t.clone())?;
            self.entries.remove(&oldest);
        }
        entry.expires = now + TOKEN_TTL;
        self.entries.insert(token.clone(), entry);
        Some(token)
    }

    /// Use a token: removes it either way, so a second use always fails.
    pub fn take(&mut self, token: &str, now: Duration) -> Result<Entry, TakeError> {
        let entry = self.entries.remove(token).ok_or(TakeError::Unknown)?;
        if entry.expires <= now {
            return Err(TakeError::Expired);
        }
        Ok(entry)
    }

    /// Issue a chat code for `entry`: 4 characters, waiting for `winhands approve`. `None` when no
    /// randomness is available.
    pub fn mint_code(&mut self, mut entry: Entry, summary: &str, now: Duration) -> Option<String> {
        self.codes.retain(|_, c| c.entry.expires > now);
        while self.codes.len() >= MAX_PENDING {
            let oldest = self
                .codes
                .iter()
                .min_by_key(|(_, c)| c.entry.expires)
                .map(|(k, _)| k.clone())?;
            self.codes.remove(&oldest);
        }
        let code = loop {
            let mut raw = [0u8; CODE_LEN];
            getrandom::fill(&mut raw).ok()?;
            let code: String = raw
                .iter()
                .map(|b| CODE_CHARS[usize::from(b & 31)] as char)
                .collect();
            // a digit keeps codes apart from words: "allow / deny" once unlocked a code "DENY"
            if code.bytes().any(|b| b.is_ascii_digit()) && !self.codes.contains_key(&code) {
                break code;
            }
        };
        entry.expires = now + CODE_TTL;
        self.codes.insert(
            code.clone(),
            CodeEntry {
                entry,
                state: CodeState::Waiting,
                summary: summary.to_string(),
            },
        );
        Some(code)
    }

    /// the user's answer (`winhands approve|deny`). Returns the log sentence of the step. A code answers
    /// once: a second answer, even the same one, is refused.
    pub fn answer_code(
        &mut self,
        code: &str,
        allow: bool,
        now: Duration,
    ) -> Result<String, CodeError> {
        let code = normalize_code(code);
        let c = self.codes.get_mut(&code).ok_or(CodeError::Unknown)?;
        if c.entry.expires <= now {
            self.codes.remove(&code);
            return Err(CodeError::Expired);
        }
        if c.state != CodeState::Waiting {
            return Err(CodeError::Unknown);
        }
        c.state = if allow {
            CodeState::Approved
        } else {
            CodeState::Denied
        };
        Ok(c.summary.clone())
    }

    /// Use a code on the resent call. Only an approved code gives its entry, and only once.
    pub fn take_code(&mut self, code: &str, now: Duration) -> Result<Entry, CodeError> {
        let code = normalize_code(code);
        let c = self.codes.get(&code).ok_or(CodeError::Unknown)?;
        if c.entry.expires <= now {
            self.codes.remove(&code);
            return Err(CodeError::Expired);
        }
        match c.state {
            CodeState::Waiting => Err(CodeError::NotYet(code)),
            CodeState::Denied => Err(CodeError::Denied),
            CodeState::Approved => {
                let c = self.codes.remove(&code).ok_or(CodeError::Unknown)?;
                Ok(c.entry)
            }
        }
    }

    /// How many are outstanding (tests).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// How many chat codes are outstanding (tests).
    pub fn codes_len(&self) -> usize {
        self.codes.len()
    }

    /// No outstanding tokens.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A short stable fingerprint of one step: the step as sent, minus the approval arguments.
pub fn step_id(step: &Map<String, Value>) -> String {
    let mut m = step.clone();
    for k in CONTROL_KEYS {
        m.remove(k);
    }
    let digest = Sha256::digest(Value::Object(m).to_string().as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// What this call brings to approval: whether the client can show a card, and the approval it is
/// resuming on, if any.
#[derive(Debug, Clone, Default)]
pub struct Approving {
    /// The shim said it can show a card (`approvable: true`).
    pub can_ask: bool,
    /// The issued approval this call resumes on.
    pub resume: Option<Entry>,
}

/// Cut typed text for a card: one line, [`TYPED_SHOWN`] characters, line breaks shown as `[Enter]`.
pub fn shown(text: &str) -> String {
    let flat = text
        .replace("\r\n", "\n")
        .replace(['\r', '\n'], " [Enter] ");
    let flat = flat.trim();
    if flat.chars().count() <= TYPED_SHOWN {
        flat.to_string()
    } else {
        let head: String = flat.chars().take(TYPED_SHOWN).collect();
        format!("{head}...")
    }
}

fn text_of(step: &Map<String, Value>, key: &str) -> String {
    match step.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// What a desktop step does, in plain words, for a card. `redact` hides typed text (for the log).
pub fn describe_step(step: &Map<String, Value>, redact: bool) -> String {
    let action = text_of(step, "action");
    let text = text_of(step, "text");
    let quoted = |s: &str| -> String {
        if redact {
            format!("[{} characters]", s.chars().count())
        } else {
            format!("\"{}\"", shown(s))
        }
    };
    match action.as_str() {
        "type" => format!("Type {}", quoted(&text)),
        "left_click" | "double_click" | "right_click" | "middle_click" | "triple_click" => {
            let verb = match action.as_str() {
                "double_click" => "Double-click",
                "right_click" => "Right-click",
                "middle_click" => "Middle-click",
                "triple_click" => "Triple-click",
                _ => "Click",
            };
            if let Some(r) = step.get("ref").filter(|v| !v.is_null()) {
                return format!("{verb} item {}", text_of_value(r));
            }
            match step.get("coordinate").and_then(Value::as_array) {
                Some(xy) if xy.len() == 2 => format!(
                    "{verb} at ({}, {})",
                    text_of_value(&xy[0]),
                    text_of_value(&xy[1])
                ),
                _ => verb.to_string(),
            }
        }
        "key" => format!("Press {}", shown(&text)),
        "" => "Do one step".to_string(),
        other if text.is_empty() => format!("Run {other}"),
        other => format!("Run {other} with {}", quoted(&text)),
    }
}

fn text_of_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The message the card carries.
pub fn card_message(summary: &str) -> String {
    format!("Mac hands wants to: {summary}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(step: &str) -> Entry {
        Entry {
            step_id: step.into(),
            index: 1,
            key: "k".into(),
            expires: Duration::ZERO,
            notes: vec![],
            shots: vec![],
        }
    }

    #[test]
    fn the_list_has_the_never_kinds_off() {
        assert!(!Kind::PasswordOrPaymentField.approvable());
        assert!(!Kind::Captcha.approvable());
        assert!(!Kind::DebugPortOffLoopback.approvable());
        for k in [
            Kind::BrowserFormSubmit,
            Kind::BrowserRiskyButton,
            Kind::BrowserEnterInText,
            Kind::BrowserCloseForeignTab,
            Kind::BrowserNonWebUrl,
            Kind::DesktopIrreversibleButton,
            Kind::Destructive(Category::OsDestruction),
        ] {
            assert!(k.approvable(), "{k:?}");
        }
    }

    #[test]
    fn a_token_works_once() {
        let mut s = Store::default();
        let t = s.mint(entry("a"), Duration::from_secs(10)).unwrap();
        assert_eq!(t.len(), 32);
        assert!(s.take(&t, Duration::from_secs(11)).is_ok());
        assert_eq!(s.take(&t, Duration::from_secs(12)), Err(TakeError::Unknown));
        assert!(s.is_empty());
    }

    #[test]
    fn a_token_expires_after_five_minutes() {
        let mut s = Store::default();
        let t = s.mint(entry("a"), Duration::from_secs(100)).unwrap();
        assert_eq!(
            s.take(&t, Duration::from_secs(100) + TOKEN_TTL),
            Err(TakeError::Expired)
        );
        let t2 = s.mint(entry("a"), Duration::from_secs(100)).unwrap();
        assert!(
            s.take(
                &t2,
                Duration::from_secs(100) + TOKEN_TTL - Duration::from_secs(1)
            )
            .is_ok()
        );
    }

    #[test]
    fn a_code_is_four_safe_characters_and_waits_for_a_yes() {
        let mut s = Store::default();
        let now = Duration::from_secs(5);
        for _ in 0..20 {
            let c = s.mint_code(entry("a"), "sum", now).unwrap();
            assert!(is_code(&c), "{c}");
            assert!(!c.contains(['0', 'O', '1', 'I']), "{c}");
            assert_eq!(
                s.take_code(&c, now),
                Err(CodeError::NotYet(c.clone())),
                "not approved yet"
            );
        }
    }

    #[test]
    fn an_approved_code_works_once_and_in_any_case() {
        let mut s = Store::default();
        let now = Duration::from_secs(5);
        let c = s.mint_code(entry("a"), "Click \"Send\"", now).unwrap();
        let lower = c.to_ascii_lowercase();
        assert_eq!(s.answer_code(&lower, true, now).unwrap(), "Click \"Send\"");
        assert_eq!(
            s.take_code(&format!(" {lower} "), now).unwrap().step_id,
            "a"
        );
        assert_eq!(s.take_code(&c, now), Err(CodeError::Unknown));
        assert_eq!(s.codes_len(), 0);
    }

    #[test]
    fn a_denied_code_stays_denied_and_cannot_be_flipped() {
        let mut s = Store::default();
        let now = Duration::ZERO;
        let c = s.mint_code(entry("a"), "x", now).unwrap();
        s.answer_code(&c, false, now).unwrap();
        assert_eq!(s.take_code(&c, now), Err(CodeError::Denied));
        assert_eq!(s.answer_code(&c, true, now), Err(CodeError::Unknown));
        assert_eq!(s.take_code(&c, now), Err(CodeError::Denied));
    }

    #[test]
    fn a_code_expires_after_ten_minutes_even_when_approved() {
        let mut s = Store::default();
        let now = Duration::from_secs(100);
        let c = s.mint_code(entry("a"), "x", now).unwrap();
        s.answer_code(&c, true, now).unwrap();
        assert_eq!(s.take_code(&c, now + CODE_TTL), Err(CodeError::Expired));
        let c2 = s.mint_code(entry("a"), "x", now).unwrap();
        assert_eq!(
            s.answer_code(&c2, true, now + CODE_TTL),
            Err(CodeError::Expired)
        );
        assert_eq!(s.answer_code("ZZZZ", true, now), Err(CodeError::Unknown));
    }

    #[test]
    fn the_code_refusal_has_the_card_sentence_and_the_code() {
        let t = code_refusal("Click \"Search\" (submits a form)", "K7QM");
        assert_eq!(
            t,
            "Needs the user's OK. Show them this in chat: \"Mac hands wants to: Click \"Search\" \
             (submits a form)\". Ask them to reply \"allow K7QM\" (or \"deny K7QM\"). After the user \
             replies, repeat this same call with approval_code \"K7QM\". Never run winhands \
             approve yourself."
        );
    }

    #[test]
    fn an_invented_token_is_unknown() {
        let mut s = Store::default();
        assert_eq!(s.take("nope", Duration::ZERO), Err(TakeError::Unknown));
    }

    #[test]
    fn the_store_stays_small() {
        let mut s = Store::default();
        for _ in 0..(MAX_PENDING * 2) {
            s.mint(entry("a"), Duration::ZERO).unwrap();
        }
        assert!(s.len() <= MAX_PENDING);
    }

    #[test]
    fn the_step_id_ignores_the_approval_arguments_only() {
        let a = json!({"action": "left_click", "coordinate": [1, 2], "allow_irreversible": false});
        let b = json!({"action": "left_click", "coordinate": [1, 2], "approval_token": "x", "allow_irreversible": true});
        let c = json!({"action": "left_click", "coordinate": [1, 3]});
        let id = |v: &Value| step_id(v.as_object().unwrap());
        assert_eq!(id(&a), id(&b));
        assert_ne!(id(&a), id(&c));
    }

    #[test]
    fn typed_text_is_cut_and_line_breaks_are_shown() {
        assert_eq!(shown("hi\nthere"), "hi [Enter] there");
        let long = "x".repeat(200);
        let s = shown(&long);
        assert_eq!(s.chars().count(), TYPED_SHOWN + 3);
        assert!(s.ends_with("..."));
    }

    #[test]
    fn the_card_sentence_has_action_why_and_place() {
        let mut n = Need::new(Kind::BrowserFormSubmit, "Search", "submits a form", "r")
            .doing("Click \"Search\"", "Click \"Search\"");
        n.place = "on en.wikipedia.org".into();
        assert_eq!(
            n.summary(),
            "Click \"Search\" (submits a form) on en.wikipedia.org"
        );
    }

    #[test]
    fn the_log_sentence_leaves_typed_text_out() {
        let n = Need::new(Kind::BrowserEnterInText, "Search", "has a line break", "r").doing(
            "Type \"my secret\" into \"Search\"",
            "Type 9 characters into \"Search\"",
        );
        assert!(n.summary().contains("my secret"));
        assert!(!n.log_summary().contains("my secret"));
    }

    #[test]
    fn describe_step_is_plain() {
        let step = |v: Value| v.as_object().unwrap().clone();
        assert_eq!(
            describe_step(&step(json!({"action": "type", "text": "hello"})), false),
            "Type \"hello\""
        );
        assert_eq!(
            describe_step(&step(json!({"action": "type", "text": "hello"})), true),
            "Type [5 characters]"
        );
        assert_eq!(
            describe_step(
                &step(json!({"action": "left_click", "coordinate": [10, 20]})),
                false
            ),
            "Click at (10, 20)"
        );
    }

    #[test]
    fn an_offer_survives_json() {
        let o = Offer {
            token: "t".into(),
            summary: "s".into(),
            log_summary: "l".into(),
            kind: "k".into(),
            step: 3,
        };
        assert_eq!(Offer::from_json(&o.to_json()), Some(o));
        assert_eq!(Offer::from_json(&json!({"token": ""})), None);
    }
}
