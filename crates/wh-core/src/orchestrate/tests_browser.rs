//! `browser_*` actions through `run_many`: they go to the installed hook, never touch the desktop,
//! and batch only with each other.

use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use super::StepError;
use super::testkit::Kit;
use crate::browser::{BrowserFail, BrowserHook, BrowserOut};

#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<String>>,
}

impl BrowserHook for Fake {
    fn run(
        &self,
        step: &Map<String, Value>,
        _approved: Option<&str>,
    ) -> Result<BrowserOut, BrowserFail> {
        let action = step["action"].as_str().unwrap().to_string();
        self.seen.lock().unwrap().push(action.clone());
        match action.as_str() {
            "browser_click" => Err(BrowserFail::Refused(
                "REFUSED by the browser safety gate".into(),
            )),
            "browser_screenshot" => Ok(BrowserOut {
                text: "shot".into(),
                images: vec!["QUJD".into()],
            }),
            _ => Ok(BrowserOut {
                text: format!("did {action}"),
                images: Vec::new(),
            }),
        }
    }
}

fn kit_with_hook() -> (Kit, Arc<Fake>) {
    let kit = Kit::new();
    let hook = Arc::new(Fake::default());
    kit.computer.set_browser(hook.clone());
    (kit, hook)
}

#[test]
fn a_browser_step_reaches_the_hook_and_takes_no_desktop_screenshot() {
    let (kit, hook) = kit_with_hook();
    let out = kit.run(json!({"action": "browser_read"})).unwrap();
    assert_eq!(out.text, "did browser_read");
    assert!(out.images.is_empty());
    assert_eq!(*hook.seen.lock().unwrap(), ["browser_read"]);
}

#[test]
fn a_batch_runs_in_order_and_stops_at_the_first_failure() {
    let (kit, hook) = kit_with_hook();
    let out = kit
        .run(json!({"actions": [
            {"action": "browser_navigate", "url": "https://a.test"},
            {"action": "browser_click", "ref": "e1"},
            {"action": "browser_read"},
        ]}))
        .unwrap();
    assert!(out.text.contains("did browser_navigate"), "{}", out.text);
    assert!(
        out.text.contains("step 2 (browser_click) failed: REFUSED"),
        "{}",
        out.text
    );
    assert_eq!(hook.seen.lock().unwrap().len(), 2);
}

#[test]
fn a_screenshot_comes_back_as_an_image_block() {
    let (kit, _hook) = kit_with_hook();
    let out = kit.run(json!({"action": "browser_screenshot"})).unwrap();
    assert_eq!(out.images, ["QUJD"]);
    assert_eq!(out.mime, "image/jpeg");
    assert!(out.text.contains("[image 1]"));
}

#[test]
fn browser_and_desktop_steps_do_not_mix() {
    let (kit, hook) = kit_with_hook();
    let err = kit
        .run(json!({"actions": [{"action": "browser_read"}, {"action": "screenshot"}]}))
        .unwrap_err();
    assert!(matches!(err, StepError::Value(_)));
    assert!(hook.seen.lock().unwrap().is_empty());
}

#[test]
fn without_a_hook_the_module_says_it_is_not_available() {
    let kit = Kit::new();
    let err = kit.run(json!({"action": "browser_read"})).unwrap_err();
    assert!(err.message().contains("not available"), "{err}");
}

#[test]
fn a_gate_refusal_through_the_daemon_route_is_a_failed_step() {
    let (kit, _hook) = kit_with_hook();
    let out = kit
        .computer
        .execute(&json!({"action": "browser_click", "ref": "e1"}), &kit.who);
    assert!(out.ok);
    assert_eq!(out.failed_step, Some(1));
    assert!(out.result.unwrap().contains("REFUSED"));
}

#[test]
fn an_unacknowledged_owner_blocks_browser_actions_like_any_acting_action() {
    let kit = Kit::with_env(crate::env::MapEnv::new().with("WINHANDS_ACKNOWLEDGED", "0"));
    let hook = Arc::new(Fake::default());
    kit.computer.set_browser(hook.clone());
    let out = kit.computer.execute(
        &json!({"action": "browser_navigate", "url": "https://a.test"}),
        &kit.who,
    );
    assert!(!out.ok, "{out:?}");
    assert!(hook.seen.lock().unwrap().is_empty());
}
