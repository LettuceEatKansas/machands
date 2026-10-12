//! Background mode (2026-10-11): `app` + `background: true` on `left_click`, `type`, `key`, `read` and
//! `screenshot` acts in one app's window without taking the person's mouse, keyboard focus or front
//! window.
//!
//! How each action gets there, with no fallback to the foreground path:
//!
//! * `screenshot`: the window's own surface through [`Backend::capture_window_isolated`], so a
//!   covered window comes out clean. The call is scoped to the window, so coordinates are relative
//!   to that picture exactly as with `scope`. If the OS cannot read the window, the call says so.
//! * `left_click`: the element under the point (or `ref`, a number `read` printed) gets its
//!   accessibility press ([`ElementAction::Press`]). A control with no press action is reported,
//!   not clicked with the real mouse.
//! * `type`: [`ElementAction::InsertText`] on the focused text field (or the only one, or `ref`).
//!   `replace: true` sets the whole value instead ([`ElementAction::SetValue`]). A field that takes
//!   neither is reported, not typed into with key events.
//! * `key`: [`Backend::key_chord_to_window`], keys posted to that one process.
//! * `read`: the element tree of the app window, never raised to wake an empty tree.
//!
//! Every other action is refused with the reason, because each one needs the real mouse, the focus
//! or the front window. The window still goes through the same refusals as every input action
//! (agent host, protected process, `only_apps`), and typed text through the destructive-command
//! check.

use serde_json::Value;

use super::Ctx;
use super::actions::StepOut;
use super::args::{Args, flag, get, int_or, is_truthy, text as text_arg, to_int};
use super::error::{StepError, StepResult};
use super::scope::ScopeState;
use crate::backend::{
    Element, ElementAction, ElementFilter, ElementId, Image, Point, Rect, Role, TreeLimits,
    WakePolicy, WindowRef,
};
use crate::keys::{Platform, key_from_name, normalize_key_text, parse_key_combo};
use crate::textutil::py_repr;

/// Actions that never touch the shared screen, mouse or keyboard focus, so a background call lets
/// them run as they are (all passive: no wait for the person, no target restore).
const PASS_THROUGH: [&str; 9] = [
    "screenshot",
    "wait",
    "list_windows",
    "clipboard_get",
    "clipboard_set",
    "list_files",
    "wait_for_file",
    "cursor_position",
    "recipe_list",
];

/// Did this call or step ask for background mode?
pub(crate) fn wants_background(args: &Args) -> bool {
    flag(args.get("background"), false)
}

/// The `app` a background call names.
pub(crate) fn app_of(args: &Args) -> StepResult<String> {
    let app = text_arg(args, "app");
    if app.trim().is_empty() {
        return Err(StepError::value(
            "background needs app: the app name or bundle id, e.g. app=\"TextEdit\" or \
             app=\"com.apple.TextEdit\"",
        ));
    }
    Ok(app)
}

/// The plain sentence for an action that has no background route.
fn no_route(action: &str, why: &str) -> StepError {
    StepError::target(format!(
        "{action} has no background route ({why}); nothing was done. Run it without background, \
         which takes over the mouse, focus or the front window"
    ))
}

/// The plain sentence for an app that refused a background action.
fn refused(what: &str, why: &str) -> StepError {
    StepError::target(format!(
        "the app refused {what} in the background ({why}); nothing was done and the screen was not \
         taken over. This control needs the foreground path: run it without background"
    ))
}

fn contains(r: &Rect, p: Point) -> bool {
    r.contains(p)
}

fn area(r: &Rect) -> i64 {
    i64::from(r.width().max(0)) * i64::from(r.height().max(0))
}

impl Ctx<'_> {
    /// Is this call acting in a background window?
    pub(crate) fn bg_active(&self) -> bool {
        self.o.scope.get().is_some_and(|s| s.background)
    }

    /// The background window, if the call has one.
    pub(crate) fn bg_window(&self) -> Option<WindowRef> {
        self.o
            .scope
            .get()
            .filter(|s| s.background)
            .map(|s| s.window)
    }

    /// Aim the call at `spec`'s window without touching it. The same window refusals as an input
    /// action; no restore, no raise, no wait for a person (nothing here uses their devices).
    /// Returns the window label.
    pub(crate) fn set_background_scope(&self, spec: &str) -> StepResult<String> {
        let (targets, _note) = self.resolve_window(spec, false)?;
        let target = targets.into_iter().next().ok_or_else(|| {
            StepError::target(format!(
                "no window matches {}; list_windows shows what is open",
                py_repr(spec)
            ))
        })?;
        let label = target.label();
        self.guard_window(&target)?;
        let info = self.info(target.window).ok_or_else(|| {
            StepError::target(format!("{label} is gone; list_windows shows what is open"))
        })?;
        let title = if target.title.is_empty() {
            label.clone()
        } else {
            target.title.clone()
        };
        self.o.scope.set(Some(ScopeState {
            window: target.window,
            title,
            rect: info.rect,
            background: true,
        }));
        Ok(label)
    }

    /// A step's own `background` / `app`, for that step only. `None` when the step has no say and the
    /// call's mode stands.
    pub(crate) fn run_step_background(&self, step: &Args) -> Option<StepResult<StepOut>> {
        let asked = get(step, "background")?;
        let saved = self.o.scope.get();
        if flag(Some(asked), false) {
            let app = text_arg(step, "app");
            if app.trim().is_empty() {
                if self.bg_active() {
                    return None; // the call's own background window stands
                }
                return Some(Err(app_of(step).expect_err("an empty app is an error")));
            }
            if is_truthy(step, "scope") {
                return Some(Err(StepError::value(
                    "scope and background cannot be combined: scope raises the window, background \
                     never does",
                )));
            }
            let out = self.set_background_scope(&app).and_then(|_| self.run(step));
            self.o.scope.set(saved);
            return Some(out);
        }
        // background: false on a step of a background call: this step is a normal foreground one
        if self.bg_active() {
            self.o.scope.set(None);
            let out = self.run(step);
            self.o.scope.set(saved);
            return Some(out);
        }
        None
    }

    /// One action of a background call. `Ok(None)` when the action is one that can run unchanged.
    pub(crate) fn run_background(&self, step: &Args, action: &str) -> StepResult<Option<StepOut>> {
        if PASS_THROUGH.contains(&action) {
            return Ok(None);
        }
        let Some(window) = self.bg_window() else {
            return Ok(None);
        };
        let why = match action {
            "left_click" | "type" | "key" | "read" => None,
            "right_click" | "middle_click" | "double_click" | "triple_click" => {
                Some("only a plain left_click can be done as an accessibility press")
            }
            "mouse_move" | "scroll" | "left_click_drag" | "left_mouse_down" | "left_mouse_up" => {
                Some("it needs the real mouse")
            }
            "hold_key" => Some("holding a key needs the real keyboard"),
            "click_text" | "replace_word" | "find_text" | "read_field" | "wait_for_text"
            | "zoom" => Some("it works from the shared screen, not the app's own window"),
            "open_app" | "file_dialog" | "window" | "folder" | "read_text" => {
                Some("it moves focus or changes the front window")
            }
            _ => Some("it is not one of left_click, type, key, read or screenshot"),
        };
        if let Some(why) = why {
            return Err(no_route(action, why));
        }
        if get(step, "expect").is_some_and(crate::config::truthy) {
            return Err(StepError::value(
                "expect is not available in background mode (it reads the shared screen); read the \
                 window or take a screenshot instead",
            ));
        }
        if self.info(window).is_none() {
            return Err(StepError::target(
                "the app window is gone; list_windows shows what is open",
            ));
        }
        let note = match action {
            "left_click" => self.bg_click(step, window)?,
            "type" => self.bg_type(step, window)?,
            "key" => self.bg_key(step, window)?,
            _ => return self.bg_read(step, window).map(Some),
        };
        Ok(Some((note, None)))
    }

    // ------------------------------------------------------------------ pictures

    /// A region of the background window's own surface. The picture is the whole window frame and
    /// `region` is cut out of it; there is no fallback to cropping the screen, which would show
    /// whatever covers the window.
    pub(crate) fn grab_isolated(&self, window: WindowRef, region: Rect) -> StepResult<Image> {
        let img = self.be().capture_window_isolated(window).map_err(|e| {
            StepError::target(format!(
                "cannot read the app window without bringing it forward: {e}. Screenshots of a \
                 background window need Screen Recording access; for this app use the foreground \
                 path (run without background)"
            ))
        })?;
        let (scale, origin) = (img.scale(), img.origin());
        let px = |v: i32| (f64::from(v) * scale).round().max(0.0) as u32;
        let (x, y) = (px(region.left - origin.x), px(region.top - origin.y));
        let (w, h) = (px(region.width()).max(1), px(region.height()).max(1));
        if x == 0 && y == 0 && w >= img.width() && h >= img.height() {
            return Ok(img);
        }
        img.crop(x, y, w, h).ok_or_else(|| {
            StepError::target("that region is outside the app window's picture".to_string())
        })
    }

    // ------------------------------------------------------------------ elements

    /// The interactive elements of the window, for finding what to act on. Never wakes an empty
    /// tree (that raises the window).
    fn bg_elements(&self, window: WindowRef) -> StepResult<Vec<Element>> {
        let limits = TreeLimits {
            max_items: 600,
            max_depth: 25,
            deadline: std::time::Duration::from_millis(2500),
            filter: ElementFilter::Interactive,
            flatten: true,
            wake: WakePolicy::Never,
        };
        Ok(self.be().element_tree(window, &limits)?.root.children)
    }

    /// The element a `ref` names in this window's last `read`.
    fn bg_ref(&self, step: &Args, window: WindowRef) -> StepResult<Option<(ElementId, String)>> {
        // ref 0 is the first element, so only an absent (or null) ref means "no ref"
        let Some(v) = get(step, "ref") else {
            return Ok(None);
        };
        let n = to_int(v)?;
        let cached = usize::try_from(n)
            .ok()
            .and_then(|n| self.c.element_refs().get(n, Some(window)));
        match cached {
            Some(c) => Ok(Some((c.id, c.name))),
            None => Err(StepError::target(format!(
                "no element ref {n} for this window; run read (background) first and use a number \
                 it printed"
            ))),
        }
    }

    // ------------------------------------------------------------------ click

    fn bg_click(&self, step: &Args, window: WindowRef) -> StepResult<String> {
        if !text_arg(step, "text").is_empty() {
            return Err(no_route(
                "left_click with a modifier key",
                "a modifier click needs the real keyboard and mouse",
            ));
        }
        let (id, name, role, spot) = match self.bg_ref(step, window)? {
            Some((id, name)) => (id, name, String::new(), "by ref".to_string()),
            None => {
                let p = self.bg_point(step)?;
                let el = self.bg_pressable_at(window, p)?;
                let role = el.role.words().to_string();
                (el.id, el.name, role, format!("at {}", self.pt(p.x, p.y)))
            }
        };
        self.check_irreversible_named(&name, &self.app_label(window), step)?;
        let what = if role.is_empty() {
            format!("element {}", py_repr(&name))
        } else {
            format!("{role} {}", py_repr(&name))
        };
        match self.be().perform(&id, ElementAction::Press) {
            Ok(true) => Ok(format!(
                "pressed {what} {spot} in the background (accessibility press; the mouse, focus \
                 and front window were not touched)"
            )),
            Ok(false) => Err(refused(
                &format!("a click on {what}"),
                "it has no accessibility press action, and background mode never sends a real click",
            )),
            Err(e) => Err(refused(&format!("a click on {what}"), &e.to_string())),
        }
    }

    /// Where a background click aims, in screen points inside the window.
    fn bg_point(&self, step: &Args) -> StepResult<Point> {
        let rect = self.display_rect();
        if let Some(v) = get(step, "at") {
            let (fx, fy) = Self::fractions(v, "at")?;
            let (w, h) = (rect.width(), rect.height());
            return Ok(Point {
                x: rect.left + (w - 1).max(0).min((fx * f64::from(w)) as i32),
                y: rect.top + (h - 1).max(0).min((fy * f64::from(h)) as i32),
            });
        }
        match get(step, "coordinate") {
            Some(c) => {
                let (x, y) = self.scale().to_screen_value(c)?;
                Ok(Point { x, y })
            }
            None => Err(StepError::value(
                "a background left_click needs coordinate (in the window screenshot), at=[fx, fy] \
                 or ref (a number read printed)",
            )),
        }
    }

    /// The smallest enabled interactive element under `p`.
    fn bg_pressable_at(&self, window: WindowRef, p: Point) -> StepResult<Element> {
        let els = self.bg_elements(window)?;
        let mut under: Vec<Element> = els
            .into_iter()
            .filter(|e| !e.offscreen && e.rect.as_ref().is_some_and(|r| contains(r, p)))
            .collect();
        under.sort_by_key(|e| e.rect.as_ref().map_or(i64::MAX, area));
        let enabled = under.iter().position(|e| e.enabled);
        match (enabled, under.is_empty()) {
            (Some(i), _) => Ok(under.swap_remove(i)),
            (None, false) => Err(StepError::target(format!(
                "the control at {} is disabled; nothing was done",
                self.pt(p.x, p.y)
            ))),
            (None, true) => Err(StepError::target(format!(
                "no control with an accessibility press action at {}; read lists the controls. If the \
                 app draws its own buttons it needs the foreground path (run without background)",
                self.pt(p.x, p.y)
            ))),
        }
    }

    // ------------------------------------------------------------------ type

    fn bg_type(&self, step: &Args, window: WindowRef) -> StepResult<String> {
        let text = text_arg(step, "text");
        if text.is_empty() {
            return Err(StepError::value("type needs text"));
        }
        // the owner's hard line, exactly as for typing into the foreground
        self.o
            .policy
            .check_text(&text, "the text to type")
            .map_err(crate::input::TypeError::Refused)?;
        let body = text.trim_end_matches(['\n', '\r']);
        let enters = text[body.len()..].matches('\n').count()
            + usize::from(text.ends_with('\r') && !text.ends_with('\n'));
        let (id, label) = self.bg_text_target(step, window)?;
        let replace = flag(step.get("replace"), false);
        if !body.is_empty() {
            let action = if replace {
                ElementAction::SetValue(body.to_string())
            } else {
                ElementAction::InsertText(body.to_string())
            };
            match self.be().perform(&id, action) {
                Ok(true) => {}
                Ok(false) => {
                    return Err(refused(
                        &format!("typing into {label}"),
                        if replace {
                            "its value cannot be set"
                        } else {
                            "it cannot insert at the caret; replace=true sets the whole value instead"
                        },
                    ));
                }
                Err(e) => return Err(refused(&format!("typing into {label}"), &e.to_string())),
            }
        }
        let mut note = format!(
            "typed {} chars into {label} in the background ({}; no key events, focus and front \
             window untouched)",
            text.chars().count(),
            if replace {
                "replaced its value"
            } else {
                "inserted at the caret"
            }
        );
        if enters > 0 {
            let enter = parse_key_combo("enter", Platform::current())?;
            self.be()
                .key_chord_to_window(window, &enter, enters as u32)
                .map_err(|e| refused("the Return after the text", &e.to_string()))?;
            note.push_str(&format!("; then Return x{enters} sent to the app alone"));
        }
        Ok(note)
    }

    /// The text field a background `type` goes into: `ref`, else the focused one, else the only one.
    fn bg_text_target(&self, step: &Args, window: WindowRef) -> StepResult<(ElementId, String)> {
        if let Some((id, name)) = self.bg_ref(step, window)? {
            return Ok((id, format!("element {}", py_repr(&name))));
        }
        let fields: Vec<Element> = self
            .bg_elements(window)?
            .into_iter()
            .filter(|e| e.role == Role::Edit && !e.offscreen)
            .collect();
        let focused: Vec<&Element> = fields.iter().filter(|e| e.focused).collect();
        let pick = match (focused.as_slice(), fields.as_slice()) {
            ([one], _) => Some(*one),
            ([], [only]) => Some(only),
            _ => None,
        };
        let Some(el) = pick else {
            return Err(StepError::target(if fields.is_empty() {
                "no text field found in the app window; if it draws its own text box it needs the \
                 foreground path (run without background)"
                    .to_string()
            } else {
                format!(
                    "{} text fields and none has focus; run read (background) and pass ref=N for \
                     the one to type into",
                    fields.len()
                )
            }));
        };
        if el.password {
            return Err(StepError::target(
                "that is a password field; background mode will not type into it. Type it yourself",
            ));
        }
        let name = if el.name.is_empty() {
            "text field".to_string()
        } else {
            format!("text field {}", py_repr(&el.name))
        };
        Ok((el.id.clone(), name))
    }

    // ------------------------------------------------------------------ key

    fn bg_key(&self, step: &Args, window: WindowRef) -> StepResult<String> {
        let text = text_arg(step, "text");
        let combo = normalize_key_text(&text);
        let reps = int_or(step, "repeat", 1)?.clamp(1, 100) as u32;
        let trimmed = text.trim();
        if trimmed.chars().count() == 1 && key_from_name(&combo).is_none() {
            // a lone symbol means that character: type it, as the foreground path does
            let mut one = step.clone();
            one.insert("text".into(), Value::String(trimmed.repeat(reps as usize)));
            return self.bg_type(&one, window);
        }
        let keys = parse_key_combo(&combo, Platform::current())?;
        if matches!(combo.as_str(), "enter" | "alt+enter") {
            self.refuse_explorer_open(Some(self.root_of(window)))?;
        }
        self.be()
            .key_chord_to_window(window, &keys, reps)
            .map_err(|e| refused(&format!("the key {combo}"), &e.to_string()))?;
        Ok(format!(
            "pressed {combo} in the background (sent to the app's process alone; the keyboard \
             focus and front window were not touched)"
        ))
    }

    // ------------------------------------------------------------------ read

    fn bg_read(&self, step: &Args, window: WindowRef) -> StepResult<StepOut> {
        let text = text_arg(step, "text");
        let spec = if text.is_empty() { window.spec() } else { text };
        let limit = int_or(step, "limit", 40)?;
        let filter = text_arg(step, "filter");
        // wake=false whatever was asked: waking an empty tree raises the window
        let (mut out, hint) = self.do_snapshot(Some(&spec), limit, &filter, false, false)?;
        if hint {
            out.push('\n');
            out.push_str(super::snapshot::WAKE_HINT);
        }
        Ok((out, None))
    }
}
