//! Background mode (`app` + `background: true`, 2026-10-11) over a fake desktop. The world: a TextEdit
//! window that a Notes window covers and that is NOT in front, so every test can check that nothing
//! the person is using moved (mouse, keyboard events, front window), and that an app which refuses
//! is told so instead of being driven with the real mouse or keyboard.

use std::time::Duration;

use serde_json::{Value, json};

use super::ManyResult;
use super::testkit::Kit;
use crate::backend::{
    Affordance, Backend, Element, ElementAction, ElementId, Point, Rect, Role, WindowRef,
};
use crate::env::MapEnv;
use crate::keys::Key;

const EDIT: &str = "com.apple.TextEdit";
/// The TextEdit window, under the cover.
const DOC: Rect = Rect::new(20, 20, 220, 140);

struct World {
    kit: Kit,
    doc: WindowRef,
    cover: WindowRef,
}

fn world() -> World {
    let kit = Kit::with_env(
        MapEnv::new()
            .with("WINHANDS_IMG", "png")
            .with("WINHANDS_MARK", "0"),
    );
    let doc = kit.window("Untitled", EDIT, 5, DOC);
    // the cover is added last, so it is on top of the z-order and in front
    let cover = kit.window("Notes", "com.apple.Notes", 6, Rect::new(0, 0, 320, 180));
    kit.fake.set_foreground(Some(cover));
    kit.fake.set_tree(
        doc,
        Element::new("win", Role::Window, "Untitled")
            .with_child(button("save", "Save", Rect::new(40, 30, 100, 50), true))
            .with_child(button("send", "Send", Rect::new(110, 30, 170, 50), true))
            .with_child(button(
                "dead",
                "Decoration",
                Rect::new(40, 60, 100, 80),
                false,
            ))
            .with_child(field("body", Rect::new(30, 90, 230, 150), true)),
    );
    World { kit, doc, cover }
}

fn button(id: &str, name: &str, r: Rect, pressable: bool) -> Element {
    let mut e = Element::new(id, Role::Button, name).at(r);
    if pressable {
        e.affordances.push(Affordance::Invoke);
    }
    e
}

fn field(id: &str, r: Rect, focused: bool) -> Element {
    let mut e = Element::new(id, Role::Edit, "").at(r).valued("");
    e.affordances.push(Affordance::Value);
    e.focused = focused;
    e
}

fn bg(mut v: Value) -> Value {
    let m = v.as_object_mut().expect("an object");
    m.insert("app".into(), json!("TextEdit"));
    m.insert("background".into(), json!(true));
    v
}

fn run(w: &World, v: Value) -> ManyResult {
    w.kit.run(v).expect("the call runs")
}

/// A refused call: the batch reports `step N (action) failed: <why>` in its text, or the call itself
/// errors before any step ran.
struct Refused(String);

impl Refused {
    fn message(&self) -> &str {
        &self.0
    }
}

fn fail(w: &World, v: Value) -> Refused {
    match w.kit.run(v) {
        Ok(done) => {
            assert!(
                done.text.contains("failed:"),
                "expected a refusal: {}",
                done.text
            );
            Refused(done.text)
        }
        Err(e) => Refused(e.message().to_string()),
    }
}

/// Nothing the person is using was touched.
fn assert_undisturbed(w: &World) {
    assert!(w.kit.fake.events().is_empty(), "no real input event");
    assert_eq!(w.kit.fake.input_events(), 0);
    assert_eq!(w.kit.fake.foreground_window().unwrap(), Some(w.cover));
    assert!(w.kit.fake.window_ops().is_empty());
    assert!(w.kit.fake.wakes().is_empty());
}

fn performed(w: &World) -> Vec<(String, ElementAction)> {
    w.kit
        .fake
        .performed()
        .into_iter()
        .map(|(id, a)| (id.0, a))
        .collect()
}

fn value_of(w: &World, id: &str) -> String {
    w.kit
        .fake
        .element(&ElementId(id.into()))
        .unwrap()
        .value
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// click
// ---------------------------------------------------------------------------

#[test]
fn a_background_click_presses_the_control_and_moves_nothing() {
    let w = world();
    // window 220x140 at (20,20); "Save" centre is (90,40) -> fractions 70/220, 20/140
    let out = run(
        &w,
        bg(json!({"action": "left_click", "at": [70.0 / 219.0, 20.0 / 139.0]})),
    );
    assert!(out.text.contains("pressed button 'Save'"), "{}", out.text);
    assert!(out.text.contains("in the background"), "{}", out.text);
    assert_eq!(
        performed(&w),
        vec![("save".to_string(), ElementAction::Press)]
    );
    assert_undisturbed(&w);
}

#[test]
fn a_background_click_reads_coordinates_in_the_window_picture() {
    let w = world();
    // 0..1000 of THIS window: "Save" centre = (70/220, 20/140) of the window
    let out = run(
        &w,
        bg(json!({"action": "left_click", "coords": "norm1000", "coordinate": [318, 143]})),
    );
    assert!(out.text.contains("'Save'"), "{}", out.text);
    assert_eq!(performed(&w)[0].0, "save");
    assert_undisturbed(&w);
}

#[test]
fn the_pictures_come_from_the_window_alone_even_when_covered_or_minimized() {
    let w = world();
    let out = run(&w, bg(json!({"action": "screenshot"})));
    assert_eq!(out.images.len(), 1);
    assert!(out.text.contains("window \"Untitled\""), "{}", out.text);
    assert!(!w.kit.fake.isolated_captures().is_empty());
    // every capture was the app window's rectangle, never the whole screen
    assert!(w.kit.fake.captured_rects().iter().all(|r| *r == DOC));
    w.kit.fake.update_window(w.doc, |i| i.minimized = true);
    let again = run(&w, bg(json!({"action": "screenshot"})));
    assert_eq!(again.images.len(), 1);
    assert_undisturbed(&w);
}

#[test]
fn a_control_with_no_press_action_is_reported_never_clicked_for_real() {
    let w = world();
    let err = fail(
        &w,
        bg(json!({"action": "left_click", "at": [70.0 / 219.0, 50.0 / 139.0]})),
    );
    let msg = err.message();
    assert!(msg.contains("no accessibility press action"), "{msg}");
    assert!(msg.contains("foreground path"), "{msg}");
    assert_undisturbed(&w);
}

#[test]
fn a_click_on_nothing_says_so() {
    let w = world();
    let err = fail(&w, bg(json!({"action": "left_click", "at": [0.99, 0.5]})));
    assert!(err.message().contains("no control"), "{}", err.message());
    assert!(w.kit.fake.performed().is_empty());
    assert_undisturbed(&w);
}

#[test]
fn a_ref_from_read_is_pressed_directly() {
    let w = world();
    let listing = run(&w, bg(json!({"action": "read"}))).text;
    assert!(listing.contains("Save"), "{listing}");
    let n = listing
        .lines()
        .find(|l| l.contains("Save"))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|t| t.parse::<i64>().ok())
        .expect("a numbered line for Save");
    let out = run(&w, bg(json!({"action": "left_click", "ref": n})));
    assert_eq!(performed(&w)[0].0, "save", "{listing}\n{}", out.text);
    assert_undisturbed(&w);
}

#[test]
fn an_unknown_ref_asks_for_a_read_first() {
    let w = world();
    let err = fail(&w, bg(json!({"action": "left_click", "ref": 9})));
    assert!(err.message().contains("run read"), "{}", err.message());
}

#[test]
fn a_send_button_is_refused_under_an_allowlist_unless_allowed() {
    let w = world();
    let at = json!([140.0 / 219.0, 20.0 / 139.0]);
    let only = json!(["com.apple.textedit"]);
    let err = fail(
        &w,
        bg(json!({"action": "left_click", "at": at, "only_apps": only})),
    );
    assert!(err.message().contains("irreversible"), "{}", err.message());
    assert!(w.kit.fake.performed().is_empty());
    run(
        &w,
        bg(
            json!({"action": "left_click", "at": at, "only_apps": only, "allow_irreversible": true}),
        ),
    );
    assert_eq!(performed(&w)[0].0, "send");
}

// ---------------------------------------------------------------------------
// type
// ---------------------------------------------------------------------------

#[test]
fn typing_inserts_into_the_focused_field_with_no_key_events() {
    let w = world();
    let out = run(&w, bg(json!({"action": "type", "text": "hi there"})));
    assert!(out.text.contains("typed 8 chars"), "{}", out.text);
    assert!(out.text.contains("no key events"), "{}", out.text);
    assert_eq!(value_of(&w, "body"), "hi there");
    assert_eq!(
        performed(&w),
        vec![(
            "body".to_string(),
            ElementAction::InsertText("hi there".into())
        )]
    );
    assert_undisturbed(&w);
    assert_eq!(w.kit.fake.typed_text(), "");
}

#[test]
fn replace_sets_the_whole_value() {
    let w = world();
    run(&w, bg(json!({"action": "type", "text": "old"})));
    run(
        &w,
        bg(json!({"action": "type", "text": "new", "replace": true})),
    );
    assert_eq!(value_of(&w, "body"), "new");
    assert!(matches!(performed(&w)[1].1, ElementAction::SetValue(_)));
    assert_undisturbed(&w);
}

#[test]
fn a_trailing_newline_becomes_a_return_sent_to_the_app_alone() {
    let w = world();
    run(&w, bg(json!({"action": "type", "text": "go\n"})));
    assert_eq!(value_of(&w, "body"), "go");
    let posted = w.kit.fake.posted_keys();
    assert_eq!(posted, vec![(w.doc, vec![Key::Enter], 1)]);
    assert_undisturbed(&w);
}

#[test]
fn an_app_that_will_not_insert_is_told_so_and_not_typed_at() {
    let w = world();
    w.kit.fake.set_no_insert(true);
    let err = fail(&w, bg(json!({"action": "type", "text": "hello"})));
    let msg = err.message();
    assert!(msg.contains("refused"), "{msg}");
    assert!(msg.contains("foreground path"), "{msg}");
    assert_eq!(value_of(&w, "body"), "");
    assert_undisturbed(&w);
}

#[test]
fn several_fields_and_none_focused_asks_for_a_ref() {
    let w = world();
    w.kit.fake.set_tree(
        w.doc,
        Element::new("win", Role::Window, "Untitled")
            .with_child(field("a", Rect::new(30, 30, 100, 50), false))
            .with_child(field("b", Rect::new(30, 60, 100, 80), false)),
    );
    let err = fail(&w, bg(json!({"action": "type", "text": "x"})));
    assert!(err.message().contains("pass ref=N"), "{}", err.message());
    assert!(w.kit.fake.performed().is_empty());
}

#[test]
fn a_password_field_is_never_typed_into() {
    let w = world();
    let mut pw = field("pw", Rect::new(30, 30, 100, 50), true);
    pw.password = true;
    w.kit.fake.set_tree(
        w.doc,
        Element::new("win", Role::Window, "Untitled").with_child(pw),
    );
    let err = fail(&w, bg(json!({"action": "type", "text": "secret"})));
    assert!(err.message().contains("password"), "{}", err.message());
    assert!(w.kit.fake.performed().is_empty());
}

#[test]
fn the_destructive_text_refusal_still_applies() {
    let w = world();
    let err = fail(
        &w,
        bg(json!({"action": "type", "text": "vssadmin delete shadows /all"})),
    );
    assert!(
        err.message().contains("allow_destructive"),
        "{}",
        err.message()
    );
    assert!(w.kit.fake.performed().is_empty());
}

// ---------------------------------------------------------------------------
// key
// ---------------------------------------------------------------------------

#[test]
fn a_key_goes_to_the_apps_process_alone() {
    let w = world();
    let out = run(
        &w,
        bg(json!({"action": "key", "text": "Escape", "repeat": 2})),
    );
    assert!(
        out.text.contains("pressed esc in the background"),
        "{}",
        out.text
    );
    assert_eq!(
        w.kit.fake.posted_keys(),
        vec![(w.doc, vec![Key::Escape], 2)]
    );
    assert_undisturbed(&w);
}

#[test]
fn a_chord_keeps_its_modifiers() {
    let w = world();
    run(&w, bg(json!({"action": "key", "text": "cmd+a"})));
    let (_, keys, _) = w.kit.fake.posted_keys().remove(0);
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[1], Key::Char('a'));
    assert_undisturbed(&w);
}

#[test]
fn a_system_without_per_process_keys_says_so() {
    let w = world();
    w.kit.fake.set_no_background_keys(true);
    let err = fail(&w, bg(json!({"action": "key", "text": "tab"})));
    assert!(
        err.message().contains("foreground path"),
        "{}",
        err.message()
    );
    assert_undisturbed(&w);
}

// ---------------------------------------------------------------------------
// read, refusals and the unchanged foreground
// ---------------------------------------------------------------------------

#[test]
fn read_looks_at_the_app_window_and_never_raises_it() {
    let w = world();
    w.kit.fake.set_chromium_empty(w.doc);
    let out = run(&w, bg(json!({"action": "read", "wake": true})));
    assert!(
        out.text.contains("wake"),
        "an empty tree gets the hint: {}",
        out.text
    );
    assert_undisturbed(&w);
}

#[test]
fn the_front_app_is_not_where_background_reads() {
    let w = world();
    let out = run(&w, bg(json!({"action": "read"})));
    assert!(out.text.contains("window \"Untitled\""), "{}", out.text);
    assert!(!out.text.contains("Notes"));
}

#[test]
fn background_does_not_wait_for_a_person_at_the_mouse() {
    let w = world();
    w.kit.fake.set_human_idle(Duration::ZERO); // somebody is using the mouse right now
    run(&w, bg(json!({"action": "left_click", "at": [0.3, 0.14]})));
    assert_eq!(w.kit.fake.human_idle_reads(), 0);
    // the same call in the foreground does wait for them
    let _ = w
        .kit
        .run(json!({"action": "left_click", "coordinate": [100, 100]}));
    assert!(w.kit.fake.human_idle_reads() > 0);
}

#[test]
fn actions_that_need_the_real_mouse_or_focus_are_refused_with_the_reason() {
    let w = world();
    for action in [
        "double_click",
        "right_click",
        "mouse_move",
        "scroll",
        "hold_key",
        "click_text",
        "open_app",
        "window",
        "read_text",
    ] {
        let err = fail(
            &w,
            bg(json!({"action": action, "text": "x", "coordinate": [10, 10]})),
        );
        let msg = err.message();
        assert!(msg.contains("no background route"), "{action}: {msg}");
        assert!(msg.contains("nothing was done"), "{action}: {msg}");
    }
    assert_undisturbed(&w);
    assert!(w.kit.fake.performed().is_empty());
}

#[test]
fn background_without_an_app_asks_for_one() {
    let w = world();
    let err = fail(
        &w,
        json!({"action": "type", "text": "x", "background": true}),
    );
    assert!(
        err.message().contains("background needs app"),
        "{}",
        err.message()
    );
}

#[test]
fn background_and_scope_cannot_be_combined() {
    let w = world();
    let err = fail(&w, bg(json!({"action": "screenshot", "scope": "Untitled"})));
    assert!(
        err.message().contains("cannot be combined"),
        "{}",
        err.message()
    );
}

#[test]
fn an_app_that_is_not_allowed_is_refused_in_the_background_too() {
    let w = world();
    let err = fail(
        &w,
        bg(json!({"action": "type", "text": "x", "only_apps": ["notepad.exe"]})),
    );
    assert!(w.kit.fake.performed().is_empty(), "{}", err.message());
}

#[test]
fn the_agents_own_app_cannot_be_driven_in_the_background() {
    let w = world();
    let err = fail(
        &w,
        json!({"action": "type", "text": "x", "app": "Notes", "background": true,
               "only_apps": ["com.apple.textedit"]}),
    );
    assert!(w.kit.fake.performed().is_empty(), "{}", err.message());
}

#[test]
fn without_background_nothing_changes_and_app_alone_is_ignored() {
    let w = world();
    // a plain click goes through the real mouse
    run(
        &w,
        json!({"action": "left_click", "coordinate": [100, 100]}),
    );
    assert!(
        w.kit
            .fake
            .events()
            .iter()
            .any(|e| matches!(e, crate::backend::fake::InputEvent::Click { .. }))
    );
    assert!(w.kit.fake.performed().is_empty());
    // `app` without `background` does not switch the mode on
    w.kit.fake.clear_events();
    run(
        &w,
        json!({"action": "left_click", "coordinate": [100, 100], "app": "TextEdit"}),
    );
    assert!(!w.kit.fake.events().is_empty());
    assert!(w.kit.fake.isolated_captures().is_empty());
    assert!(w.kit.fake.posted_keys().is_empty());
}

#[test]
fn a_batch_runs_every_step_in_the_background() {
    let w = world();
    let out = run(
        &w,
        json!({"app": "TextEdit", "background": true, "actions": [
            {"action": "type", "text": "abc"},
            {"action": "key", "text": "tab"},
            {"action": "left_click", "at": [0.3, 0.14]},
            {"action": "screenshot"},
        ]}),
    );
    assert!(!out.text.contains("failed"), "{}", out.text);
    assert_eq!(value_of(&w, "body"), "abc");
    assert_eq!(w.kit.fake.posted_keys().len(), 1);
    assert!(performed(&w).iter().any(|(id, _)| id == "save"));
    assert_undisturbed(&w);
}

#[test]
fn a_step_can_go_background_alone_or_step_back_to_the_foreground() {
    let w = world();
    // only the first step is background
    let out = run(
        &w,
        json!({"actions": [
            {"action": "type", "text": "bg", "app": "TextEdit", "background": true},
            {"action": "left_click", "coordinate": [100, 100]},
        ], "shot": false}),
    );
    assert!(!out.text.contains("failed"), "{}", out.text);
    assert_eq!(value_of(&w, "body"), "bg");
    let clicks = w
        .kit
        .fake
        .events()
        .into_iter()
        .filter(|e| matches!(e, crate::backend::fake::InputEvent::Click { .. }))
        .count();
    assert_eq!(clicks, 1, "the second step used the real mouse");

    // and the other way round: a foreground step inside a background call
    let w = world();
    run(
        &w,
        json!({"app": "TextEdit", "background": true, "shot": false, "actions": [
            {"action": "type", "text": "bg"},
            {"action": "left_click", "coordinate": [100, 100], "background": false},
        ]}),
    );
    assert_eq!(value_of(&w, "body"), "bg");
    assert!(
        w.kit
            .fake
            .events()
            .iter()
            .any(|e| matches!(e, crate::backend::fake::InputEvent::Click { at, .. } if *at == Point::new(100, 100)))
    );
}

#[test]
fn expect_is_refused_in_the_background() {
    let w = world();
    let err = fail(
        &w,
        bg(json!({"action": "type", "text": "x", "expect": {"text": "x"}})),
    );
    assert!(
        err.message().contains("expect is not available"),
        "{}",
        err.message()
    );
}
