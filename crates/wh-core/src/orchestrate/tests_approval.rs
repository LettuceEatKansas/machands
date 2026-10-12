//! Approval cards through the daemon's `computer` route: what a stopped step offers, what a token
//! may and may not do, and how a batch carries on after a yes. The shim half (the card itself) is in
//! `winhands/src/shim.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::testkit::Kit;
use crate::approval::{Kind, Need, TOKEN_TTL};
use crate::backend::fake::InputEvent;
use crate::backend::{Element, Rect, Role};
use crate::browser::{BrowserFail, BrowserHook, BrowserOut};
use crate::clock::Clock;

const REFUSAL: &str = "REFUSED by the browser safety gate: clicking \"Search\" would submit a form. Ask the user in chat and wait for a clear yes. Do not retry or work around this.";

/// A browser that asks for a card on every click, like the real gate does for a submit.
#[derive(Default)]
struct Gate {
    seen: Mutex<Vec<(String, Option<String>)>>,
    label: Mutex<String>,
}

impl Gate {
    fn need(&self) -> Need {
        let label = self.label.lock().unwrap().clone();
        let label = if label.is_empty() {
            "Search".into()
        } else {
            label
        };
        let mut n = Need::new(
            Kind::BrowserFormSubmit,
            label.clone(),
            "submits a form",
            REFUSAL,
        )
        .doing(format!("Click \"{label}\""), format!("Click \"{label}\""));
        n.place = "on en.wikipedia.org".into();
        n
    }
}

impl BrowserHook for Gate {
    fn run(
        &self,
        step: &Map<String, Value>,
        approved: Option<&str>,
    ) -> Result<BrowserOut, BrowserFail> {
        let action = step["action"].as_str().unwrap().to_string();
        self.seen
            .lock()
            .unwrap()
            .push((action.clone(), approved.map(str::to_string)));
        match action.as_str() {
            "browser_click" => {
                let need = self.need();
                if approved == Some(need.key().as_str()) {
                    Ok(BrowserOut {
                        text: "clicked e1".into(),
                        images: vec![],
                    })
                } else {
                    Err(BrowserFail::Approval(Box::new(need)))
                }
            }
            "browser_type" => Err(BrowserFail::Refused(
                "REFUSED: this looks like a password or payment field".into(),
            )),
            _ => Ok(BrowserOut {
                text: format!("did {action}"),
                images: vec![],
            }),
        }
    }
}

fn browser_kit() -> (Kit, Arc<Gate>) {
    let kit = Kit::new();
    let gate = Arc::new(Gate::default());
    kit.computer.set_browser(gate.clone());
    (kit, gate)
}

fn call(kit: &Kit, v: Value) -> super::ToolOutcome {
    kit.computer.execute(&v, &kit.who)
}

fn click_call() -> Value {
    json!({"action": "browser_click", "ref": "e1", "approvable": true})
}

/// The chat code in a refusal text.
fn code_of(out: &super::ToolOutcome) -> String {
    let text = out.result.as_deref().expect("a result");
    let at = text.find("approval_code \"").expect("a code in the text") + "approval_code \"".len();
    text[at..at + crate::approval::CODE_LEN].to_string()
}

fn code_call(code: &str) -> Value {
    json!({"action": "browser_click", "ref": "e1", "approval_code": code})
}

fn token_of(out: &super::ToolOutcome) -> String {
    out.offer.as_ref().expect("an offer").token.clone()
}

// ---------------------------------------------------------------------------- the offer

#[test]
fn a_caller_that_can_show_a_card_gets_an_offer_and_the_refusal_text_unchanged() {
    let (kit, _) = browser_kit();
    let out = call(&kit, click_call());
    let offer = out.offer.as_ref().expect("an offer");
    assert_eq!(
        offer.summary,
        "Click \"Search\" (submits a form) on en.wikipedia.org"
    );
    assert_eq!(offer.step, 1);
    assert_eq!(offer.token.len(), 32);
    // the answer is the refusal exactly as it always was
    assert!(out.ok);
    assert_eq!(out.failed_step, Some(1));
    assert!(out.result.as_deref().unwrap().contains(REFUSAL));
    // and the JSON the shim reads carries the offer
    let j = out.to_json();
    assert_eq!(j["approval"]["token"], offer.token.as_str());
}

#[test]
fn a_caller_that_cannot_show_a_card_gets_no_offer_but_a_chat_code() {
    let (kit, _) = browser_kit();
    let out = call(&kit, json!({"action": "browser_click", "ref": "e1"}));
    assert!(out.offer.is_none());
    assert!(out.to_json().get("approval").is_none());
    let text = out.result.as_deref().unwrap();
    let code = code_of(&out);
    assert!(
        text.contains(
            "Show them this in chat: \"Mac hands wants to: Click \"Search\" (submits a form) on en.wikipedia.org\""
        ),
        "{text}"
    );
    assert!(text.contains(&format!("reply \"allow {code}\" (or \"deny {code}\")")));
    assert!(text.contains(&format!("approval_code \"{code}\"")));
    assert!(text.contains("Never run winhands approve yourself."));
    assert!(
        !text.contains(REFUSAL),
        "the code text replaces the refusal"
    );
    // still a failed step 1, so callers and the failure fields behave as before
    assert_eq!(out.failed_step, Some(1));
}

#[test]
fn a_hard_refusal_never_offers_a_card() {
    let (kit, _) = browser_kit();
    let out = call(
        &kit,
        json!({"action": "browser_type", "ref": "e2", "text": "x", "approvable": true}),
    );
    assert!(out.offer.is_none(), "{out:?}");
    assert!(
        out.result
            .as_deref()
            .unwrap()
            .contains("password or payment")
    );
}

// ---------------------------------------------------------------------------- the token

#[test]
fn the_token_runs_the_waiting_step_once() {
    let (kit, gate) = browser_kit();
    let first = call(&kit, click_call());
    let token = token_of(&first);
    let mut again = click_call();
    again["approval_token"] = json!(token);
    let done = call(&kit, again.clone());
    assert!(done.offer.is_none());
    assert_eq!(done.failed_step, None, "{done:?}");
    assert_eq!(done.result.as_deref(), Some("clicked e1"));
    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].1.as_deref(), Some(gate.need().key().as_str()));
    // the same token again: spent
    let reuse = call(&kit, again);
    assert!(!reuse.ok);
    assert!(
        reuse.error.as_deref().unwrap().contains("already used"),
        "{reuse:?}"
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 2, "nothing ran");
}

#[test]
fn a_token_for_one_step_does_not_open_another() {
    let (kit, gate) = browser_kit();
    let first = call(&kit, click_call());
    let token = token_of(&first);
    let out = call(
        &kit,
        json!({"action": "browser_click", "ref": "e7", "approvable": true, "approval_token": token}),
    );
    assert!(!out.ok, "{out:?}");
    assert!(
        out.error.as_deref().unwrap().contains("different step"),
        "{out:?}"
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 1, "the click never ran");
}

#[test]
fn a_token_expires_after_five_minutes() {
    let (kit, gate) = browser_kit();
    let token = token_of(&call(&kit, click_call()));
    kit.clock.sleep(TOKEN_TTL + Duration::from_secs(1));
    let mut again = click_call();
    again["approval_token"] = json!(token);
    let out = call(&kit, again);
    assert!(!out.ok);
    assert!(out.error.as_deref().unwrap().contains("expired"), "{out:?}");
    assert_eq!(gate.seen.lock().unwrap().len(), 1);
}

#[test]
fn an_invented_token_runs_nothing() {
    let (kit, gate) = browser_kit();
    let out = call(
        &kit,
        json!({"action": "browser_click", "ref": "e1", "approval_token": "deadbeef"}),
    );
    assert!(!out.ok);
    assert!(gate.seen.lock().unwrap().is_empty());
}

#[test]
fn if_the_target_changed_after_the_card_the_step_asks_again() {
    let (kit, gate) = browser_kit();
    let token = token_of(&call(&kit, click_call()));
    // the page now has a different button where "Search" was
    *gate.label.lock().unwrap() = "Delete account".into();
    let mut again = click_call();
    again["approval_token"] = json!(token);
    let out = call(&kit, again);
    let offer = out.offer.expect("asks again");
    assert!(
        offer.summary.starts_with("Click \"Delete account\""),
        "{offer:?}"
    );
}

// ---------------------------------------------------------------------------- batches

#[test]
fn a_batch_carries_on_from_the_waiting_step_without_repeating_the_earlier_ones() {
    let (kit, gate) = browser_kit();
    let batch = json!({"approvable": true, "actions": [
        {"action": "browser_navigate", "url": "https://a.test"},
        {"action": "browser_click", "ref": "e1"},
        {"action": "browser_read"},
    ]});
    let first = call(&kit, batch.clone());
    let offer = first.offer.clone().expect("an offer");
    assert_eq!(offer.step, 2);
    let text = first.result.clone().unwrap();
    assert!(text.contains("did browser_navigate"), "{text}");
    assert!(
        text.contains("step 2 (browser_click) failed: REFUSED"),
        "{text}"
    );
    assert!(!text.contains("did browser_read"));

    let mut resend = batch;
    resend["approval_token"] = json!(offer.token);
    let done = call(&kit, resend);
    assert_eq!(done.failed_step, None, "{done:?}");
    let text = done.result.unwrap();
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        ["did browser_navigate", "clicked e1", "did browser_read"]
    );
    let actions: Vec<String> = gate
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|(a, _)| a.clone())
        .collect();
    assert_eq!(
        actions,
        [
            "browser_navigate",
            "browser_click",
            "browser_click",
            "browser_read"
        ],
        "the navigate ran once"
    );
}

// ---------------------------------------------------------------------------- the desktop

fn app_with_buttons(kit: &Kit, names: &[(&str, Rect)]) -> crate::backend::WindowRef {
    let w = kit.front("App", "app.exe", 11);
    set_buttons(kit, w, names);
    w
}

fn set_buttons(kit: &Kit, w: crate::backend::WindowRef, names: &[(&str, Rect)]) {
    let mut root = Element::new("root", Role::Window, "App");
    for (i, (name, rect)) in names.iter().enumerate() {
        root = root.with_child(Element::new(&format!("c{i}"), Role::Button, name).at(*rect));
    }
    kit.fake.set_tree(w, root);
}

fn click_count(kit: &Kit) -> usize {
    kit.fake
        .events()
        .iter()
        .filter(|e| matches!(e, InputEvent::Click { .. }))
        .count()
}

fn send_click() -> Value {
    json!({
        "action": "left_click", "coordinate": [30, 30],
        "only_apps": ["app.exe"], "approvable": true,
    })
}

#[test]
fn an_irreversible_looking_button_offers_a_card_then_clicks_on_a_yes() {
    let kit = Kit::new();
    app_with_buttons(&kit, &[("Send", Rect::new(20, 20, 60, 40))]);
    let first = call(&kit, send_click());
    let offer = first.offer.clone().expect("an offer");
    assert_eq!(
        offer.summary,
        "Click \"Send\" (looks hard to undo) in app.exe"
    );
    assert!(
        first
            .result
            .as_deref()
            .unwrap()
            .contains("allow_irreversible"),
        "{first:?}"
    );
    assert_eq!(click_count(&kit), 0);

    let mut again = send_click();
    again["approval_token"] = json!(offer.token);
    let done = call(&kit, again);
    assert_eq!(done.failed_step, None, "{done:?}");
    assert_eq!(click_count(&kit), 1);
}

#[test]
fn a_desktop_batch_does_not_repeat_the_steps_before_the_waiting_one() {
    let kit = Kit::new();
    app_with_buttons(
        &kit,
        &[
            ("Next", Rect::new(100, 20, 160, 40)),
            ("Send", Rect::new(20, 20, 60, 40)),
        ],
    );
    let batch = json!({"approvable": true, "only_apps": ["app.exe"], "actions": [
        {"action": "left_click", "coordinate": [110, 30]},
        {"action": "left_click", "coordinate": [30, 30]},
    ]});
    let first = call(&kit, batch.clone());
    let offer = first.offer.clone().expect("an offer");
    assert_eq!(offer.step, 2);
    assert_eq!(click_count(&kit), 1, "only the first click so far");
    let mut resend = batch;
    resend["approval_token"] = json!(offer.token);
    let done = call(&kit, resend);
    assert_eq!(done.failed_step, None, "{done:?}");
    assert_eq!(click_count(&kit), 2, "the first click was not repeated");
}

#[test]
fn a_desktop_button_that_changed_after_the_card_asks_again() {
    let kit = Kit::new();
    let w = app_with_buttons(&kit, &[("Send", Rect::new(20, 20, 60, 40))]);
    let offer = call(&kit, send_click()).offer.expect("an offer");
    set_buttons(&kit, w, &[("Delete", Rect::new(20, 20, 60, 40))]);
    let mut again = send_click();
    again["approval_token"] = json!(offer.token);
    let out = call(&kit, again);
    assert_eq!(click_count(&kit), 0, "nothing was clicked {:?}", out.result);
    let second = out.offer.expect("asks again for the new button");
    assert!(second.summary.starts_with("Click \"Delete\""), "{second:?}");
}

#[test]
fn a_destructive_command_offers_a_card_then_types_on_a_yes() {
    let kit = Kit::new();
    kit.front("Terminal", "term.exe", 3);
    let call_json = json!({"action": "type", "text": "format c: /q", "approvable": true});
    let first = call(&kit, call_json.clone());
    let offer = first.offer.clone().expect("an offer");
    assert!(
        offer
            .summary
            .starts_with("Type \"format c: /q\" (could leave this machine unbootable)"),
        "{offer:?}"
    );
    assert!(offer.summary.ends_with("in term.exe"), "{offer:?}");
    assert!(
        first
            .result
            .as_deref()
            .unwrap()
            .contains("allow_destructive")
    );
    assert_eq!(kit.fake.typed_text(), "");

    let mut again = call_json;
    again["approval_token"] = json!(offer.token);
    let done = call(&kit, again);
    assert_eq!(done.failed_step, None, "{done:?}");
    assert!(
        done.result
            .as_deref()
            .unwrap()
            .starts_with("typed 12 chars"),
        "{:?}",
        done.result
    );
}

#[test]
fn typed_text_on_a_card_is_cut_to_eighty_characters_and_kept_out_of_the_log_line() {
    let kit = Kit::new();
    kit.front("Terminal", "term.exe", 3);
    let long = format!("format c: /q {}", "x".repeat(200));
    let out = call(
        &kit,
        json!({"action": "type", "text": long, "approvable": true}),
    );
    let offer = out.offer.expect("an offer");
    assert!(offer.summary.contains("..."), "{}", offer.summary);
    assert!(offer.summary.chars().count() < 200, "{}", offer.summary);
    assert!(
        !offer.log_summary.contains("format c:"),
        "{}",
        offer.log_summary
    );
}

#[test]
fn a_call_without_a_card_gets_a_chat_code_for_a_destructive_command() {
    let kit = Kit::new();
    kit.front("Terminal", "term.exe", 3);
    let out = call(&kit, json!({"action": "type", "text": "format c: /q"}));
    assert!(out.offer.is_none());
    let text = out.result.as_deref().unwrap();
    assert!(text.contains("Needs the user's OK."), "{text}");
    assert!(text.contains("in term.exe"), "{text}");
    assert_eq!(kit.fake.typed_text(), "");
    // the user says yes in chat; the same call with the code types it
    let code = code_of(&out);
    kit.computer.answer_approval_code(&code, true).unwrap();
    let done = call(
        &kit,
        json!({"action": "type", "text": "format c: /q", "approval_code": code}),
    );
    assert_eq!(done.failed_step, None, "{done:?}");
    assert!(
        done.result
            .as_deref()
            .unwrap()
            .starts_with("typed 12 chars")
    );
}

// ---------------------------------------------------------------------------- the chat code

#[test]
fn a_code_that_the_user_has_not_answered_runs_nothing_and_stays_valid() {
    let (kit, gate) = browser_kit();
    let code = code_of(&call(&kit, json!({"action": "browser_click", "ref": "e1"})));
    let out = call(&kit, code_call(&code));
    assert!(!out.ok);
    assert_eq!(
        out.error.as_deref(),
        Some(format!("The user has not approved {code} yet; wait for their reply").as_str())
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 1, "the click never ran");
    // still usable after they answer
    kit.computer.answer_approval_code(&code, true).unwrap();
    assert_eq!(
        call(&kit, code_call(&code)).result.as_deref(),
        Some("clicked e1")
    );
}

#[test]
fn an_approved_code_resumes_once() {
    let (kit, gate) = browser_kit();
    let code = code_of(&call(&kit, json!({"action": "browser_click", "ref": "e1"})));
    let line = kit
        .computer
        .answer_approval_code(&code.to_lowercase(), true)
        .unwrap();
    assert!(
        line.starts_with(&format!("approved {code}: Click \"Search\"")),
        "{line}"
    );
    let done = call(&kit, code_call(&code));
    assert_eq!(done.failed_step, None, "{done:?}");
    assert_eq!(done.result.as_deref(), Some("clicked e1"));
    let seen = gate.seen.lock().unwrap().clone();
    assert_eq!(seen[1].1.as_deref(), Some(gate.need().key().as_str()));
    // reuse: spent
    let again = call(&kit, code_call(&code));
    assert!(!again.ok);
    assert!(
        again.error.as_deref().unwrap().contains("already used"),
        "{again:?}"
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 2);
}

#[test]
fn a_code_for_one_step_does_not_open_another() {
    let (kit, gate) = browser_kit();
    let code = code_of(&call(&kit, json!({"action": "browser_click", "ref": "e1"})));
    kit.computer.answer_approval_code(&code, true).unwrap();
    let out = call(
        &kit,
        json!({"action": "browser_click", "ref": "e7", "approval_code": code}),
    );
    assert!(!out.ok, "{out:?}");
    assert!(
        out.error.as_deref().unwrap().contains("different step"),
        "{out:?}"
    );
    assert_eq!(gate.seen.lock().unwrap().len(), 1, "the click never ran");
}

#[test]
fn a_code_expires_after_ten_minutes() {
    let (kit, gate) = browser_kit();
    let code = code_of(&call(&kit, json!({"action": "browser_click", "ref": "e1"})));
    kit.computer.answer_approval_code(&code, true).unwrap();
    kit.clock
        .sleep(crate::approval::CODE_TTL + Duration::from_secs(1));
    let out = call(&kit, code_call(&code));
    assert!(!out.ok);
    assert!(out.error.as_deref().unwrap().contains("expired"), "{out:?}");
    assert_eq!(gate.seen.lock().unwrap().len(), 1);
}

#[test]
fn a_denied_code_says_do_not_retry() {
    let (kit, gate) = browser_kit();
    let code = code_of(&call(&kit, json!({"action": "browser_click", "ref": "e1"})));
    let line = kit.computer.answer_approval_code(&code, false).unwrap();
    assert!(line.starts_with(&format!("denied {code}")), "{line}");
    let out = call(&kit, code_call(&code));
    assert!(!out.ok);
    assert_eq!(out.error.as_deref(), Some("The user denied this; do not retry"));
    assert_eq!(gate.seen.lock().unwrap().len(), 1);
}

#[test]
fn an_invented_code_cannot_be_answered_or_used() {
    let (kit, gate) = browser_kit();
    assert!(kit.computer.answer_approval_code("ZZZZ", true).is_err());
    let out = call(&kit, code_call("ZZZZ"));
    assert!(!out.ok);
    assert!(gate.seen.lock().unwrap().is_empty());
}

#[test]
fn a_hard_refusal_gets_no_code() {
    let (kit, _) = browser_kit();
    let out = call(
        &kit,
        json!({"action": "browser_type", "ref": "e2", "text": "x"}),
    );
    let text = out.result.as_deref().unwrap();
    assert!(text.contains("password or payment"), "{text}");
    assert!(!text.contains("approval_code"), "{text}");
}

#[test]
fn a_batch_carries_on_from_the_waiting_step_after_a_code() {
    let (kit, gate) = browser_kit();
    let batch = json!({"actions": [
        {"action": "browser_read"},
        {"action": "browser_click", "ref": "e1"},
    ]});
    let first = call(&kit, batch.clone());
    let code = code_of(&first);
    kit.computer.answer_approval_code(&code, true).unwrap();
    let mut resend = batch;
    resend["approval_code"] = json!(code);
    let done = call(&kit, resend);
    assert_eq!(done.failed_step, None, "{done:?}");
    let text = done.result.unwrap();
    assert!(
        text.contains("did browser_read") && text.contains("clicked e1"),
        "{text}"
    );
    let reads = gate
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|(a, _)| a == "browser_read")
        .count();
    assert_eq!(reads, 1, "the earlier step is not repeated");
}
