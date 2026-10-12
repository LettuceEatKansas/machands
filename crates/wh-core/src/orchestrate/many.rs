//! One call, many actions, and everything around it. Port of `computer.run_many` (the batch loop, the
//! per-call options, the trailing settle and picture) and the computer branch of
//! `daemon._execute_tool` (pause, acknowledgement, rate cap, audit entry, failure fields).
//!
//! `{"actions": [{...}, {...}]}` or a single action. A batch stops at the first error and says which
//! step failed. It returns every zoom or screenshot taken inside the batch, plus ONE screenshot at the
//! end after the screen settles, so "hover spot A, zoom, hover spot B, zoom" compares two spots in one
//! model turn.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};

use super::args::{Args, flag};
use super::error::{StepError, StepResult};
use super::guard::normalize_only_app;
use super::{CallOpts, Computer, Ctx};
use crate::actions::{POINTER_ACTIONS, is_action, is_passive};
use crate::approval::{self, Approving, Entry, Kind, Need, Offer, Pending};
use crate::audit::log_action;
use crate::backend::Rect;
use crate::caller::Caller;
use crate::config::{Config, truthy};
use crate::destructive::Policy;
use crate::keys::{is_opener, normalize_key_text};
use crate::scale::{CallScale, Coords, pick_display};
use crate::textutil::py_str;

/// What a call returns: the text, every picture taken, and their MIME type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManyResult {
    /// One line per step, then the settle line.
    pub text: String,
    /// Base64 pictures, in the order `[image N]` numbers them.
    pub images: Vec<String>,
    /// `image/jpeg` or `image/png`.
    pub mime: &'static str,
    /// A step stopped for approval and the caller can show a card (see [`crate::approval`]).
    pub approval: Option<Pending>,
}

/// What the daemon sends back for a `computer` call (Python `_execute_tool`'s dict).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// The call ran (a failed step inside a batch is still `ok`; see `failed_step`).
    pub ok: bool,
    /// The result text (`ok` only).
    pub result: Option<String>,
    /// Why the call was refused or failed (not `ok`), or the first line of a failed step's error.
    pub error: Option<String>,
    /// Pictures, base64.
    pub images: Vec<String>,
    /// Their MIME type.
    pub mime: &'static str,
    /// The 1-based step that failed, when the text names one.
    pub failed_step: Option<u32>,
    /// The failed step could go ahead if the user agrees: a single-use token and the card sentence.
    pub offer: Option<Offer>,
}

impl ToolOutcome {
    /// The JSON the HTTP daemon returns: `{"ok", "result", "content": [...], "failed_step", "error"}`
    /// or `{"ok": false, "error"}`.
    pub fn to_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("ok".into(), Value::from(self.ok));
        if let Some(text) = &self.result {
            out.insert("result".into(), Value::from(text.clone()));
            let mut content = vec![json!({"type": "text", "text": text})];
            for im in &self.images {
                content.push(json!({"type": "image", "data": im, "mimeType": self.mime}));
            }
            out.insert("content".into(), Value::Array(content));
            if let Some(n) = self.failed_step {
                out.insert("failed_step".into(), Value::from(n));
            }
        }
        if let Some(e) = &self.error {
            out.insert("error".into(), Value::from(e.clone()));
        }
        if let Some(offer) = &self.offer {
            out.insert("approval".into(), offer.to_json());
        }
        Value::Object(out)
    }
}

static STEP_FAILED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"step (\d+) \([^)]*\) failed: (.+)").expect("STEP_FAILED"));

/// `{"failed_step": n, "error": "..."}` if `text` names a failed step (Python
/// `_batch_failure_fields`). A script can read it without string-matching the result.
pub fn batch_failure_fields(text: &str) -> Option<(u32, String)> {
    let m = STEP_FAILED.captures(text)?;
    let n = m[1].parse().ok()?;
    let first = m[2].lines().next().unwrap_or("").to_string();
    Some((n, first))
}

impl CallOpts {
    /// The configured defaults with this call's own arguments layered on top. Every call starts from
    /// the defaults: present (even a falsy value like `"only_apps": []`) is that call's exact choice
    /// and absent is always the default, never a leftover from someone else's earlier call
    /// (D1.1, 2026-09-21: a caller that asked for norm1000 coordinates, a smaller screenshot or a
    /// second monitor leaked that choice into a later call from a different caller).
    pub(crate) fn from_args(c: &Computer, args: &Args, policy: Policy) -> StepResult<Self> {
        let scale = CallScale::from_args(&c.cfg, &Value::Object(args.clone()))?;
        let display: Option<Rect> = match scale.display_index {
            Some(n) => {
                let mut mons = c.backend.displays()?;
                crate::scale::sort_displays(&mut mons);
                Some(pick_display(&mons, n)?)
            }
            None => None,
        };
        let only_apps: BTreeSet<String> = match args.get("only_apps") {
            Some(Value::Array(list)) => list
                .iter()
                .map(|a| normalize_only_app(&py_str(a)))
                .collect(),
            Some(Value::String(s)) if !s.is_empty() => BTreeSet::from([normalize_only_app(s)]),
            _ => BTreeSet::new(),
        };
        Ok(Self {
            coords: scale.coords,
            long_edge: scale.long_edge,
            display,
            only_apps,
            snap_call: args.get("snap").is_some_and(truthy),
            policy,
            scope: super::scope::ScopeCell::default(),
            approval: Approving::default(),
        })
    }

    /// The configured defaults and nothing else (a read outside a `computer` call).
    pub(crate) fn defaults(cfg: &Config, policy: Policy) -> Self {
        let long = cfg
            .shot_long()
            .clamp(crate::scale::SHOT_LONG_MIN, crate::scale::SHOT_LONG_MAX);
        Self {
            coords: Coords::from_name(&cfg.coords()),
            long_edge: long as u32,
            display: None,
            only_apps: BTreeSet::new(),
            snap_call: false,
            policy,
            scope: super::scope::ScopeCell::default(),
            approval: Approving::default(),
        }
    }
}

/// `on_fail: "skip"`: carry on after this step fails instead of stopping the batch.
fn on_fail_skips(step: &Args) -> bool {
    match step.get("on_fail") {
        Some(v) if truthy(v) => py_str(v).to_lowercase() == "skip",
        _ => false,
    }
}

fn step_action_text(step: &Args) -> String {
    match step.get("action") {
        None => "None".to_string(),
        Some(v) => py_str(v),
    }
}

impl Ctx<'_> {
    /// After F2: wait for the rename box's caret. 2026-09-21: twice, right after Explorer opened, F2's
    /// box came late, the typed name was lost and Return kept the old name.
    ///
    /// F2 means "rename" only in File Explorer's file list. Elsewhere (Excel edits a cell in place with
    /// no separate caret-bearing box, a game's own F2 hotkey) no caret ever appears and this aborted
    /// every batch that used F2 for anything else. Only abort when File Explorer is actually in front;
    /// elsewhere just give the app a moment and move on. Python `_wait_rename_box`.
    fn wait_rename_box(&self) -> StepResult<()> {
        let max_s = 1.5;
        self.sleep(0.1);
        let (caret, _) = self.poll(
            || self.be().has_caret(None).unwrap_or(false).then_some(()),
            max_s - 0.1,
            0.05,
        );
        if caret.is_some() {
            self.sleep(0.05);
            return Ok(());
        }
        let in_explorer = self
            .fg_root()
            .and_then(|w| self.info(w))
            .is_some_and(|i| super::ctx::is_file_manager(&i));
        if !in_explorer {
            return Ok(());
        }
        // No box: stop the batch here. 2026-09-21: nothing was selected, F2 did nothing, and the
        // batch's Return then OPENED the file (in the owner's Notepad) instead of confirming a rename.
        Err(StepError::target(
            "F2 opened no rename box (is the file selected? click it once first); stopped before \
             the rest of the batch",
        ))
    }

    /// After an opener shortcut: wait until the foreground window or focused control changes and the
    /// screen calms, capped at `max_s`. Returns at once if nothing opened (settle's own cap). Win11's
    /// XAML apps (Notepad, new Explorer) keep one focus handle, so a missing change is not proof
    /// nothing opened: stop polling after 0.4 s and let settle watch the pixels. Python
    /// `_wait_focus_change`.
    fn wait_focus_change(&self) -> StepResult<()> {
        let max_s = 1.5;
        let (fg0, focus0) = self.c.mem().pre_key;
        let t = self.now();
        self.sleep(0.12);
        while self.now() - t < 0.4 {
            let fg = self.be().foreground_window().ok().flatten();
            let focus = self.be().focus_token().unwrap_or(0);
            if fg != fg0 || focus != focus0 {
                break;
            }
            self.sleep(0.05);
        }
        self.settle(Some((max_s - (self.now() - t)).max(0.2)), 0.15, 2)?;
        Ok(())
    }

    /// The batch loop. Python `run_many` after the per-call options are set.
    fn run_steps(&self, args: &Args) -> StepResult<ManyResult> {
        let want_shot = flag(args.get("shot"), true);
        let (steps, own_args): (Vec<Value>, bool) = match args.get("actions") {
            Some(Value::Array(list)) if !list.is_empty() => (list.clone(), false),
            Some(v) if truthy(v) && !v.is_array() => (vec![v.clone()], false),
            _ => (vec![Value::Object(args.clone())], true),
        };
        // The call's own arguments are the one step when it is a lone action (not a recipe, whose
        // steps are its own): its `scope` was applied for the whole call already.
        let own_args = own_args && !crate::recipes::is_recipe_step(&steps[0]);
        // 2026-10-08: a `recipe` step becomes that recipe's own steps, so everything below (expect, the
        // stop at the first failure, settling, the safety checks in `run`) treats them like any batch.
        let mut items = crate::recipes::expand(&self.c.recipes, &steps, &self.o.only_apps)
            .map_err(StepError::value)?;
        // 2026-10-11: `find` is not an action but models send it; settle on the real name up front
        // so the guards, the settle floor and the trailing-shot rules all see the same one.
        for item in &mut items {
            if let Value::Object(o) = &mut item.step
                && let Some(Value::String(a)) = o.get("action")
            {
                let canon = crate::actions::canonical_action(a.trim());
                if canon != a.as_str() {
                    let canon = canon.to_string();
                    o.insert("action".into(), Value::String(canon));
                }
            }
        }
        let steps: Vec<Value> = items.iter().map(|item| item.step.clone()).collect();
        let mut notes: Vec<String> = Vec::new();
        let mut shots: Vec<String> = Vec::new();
        let mut pending: Option<Pending> = None; // a step stopped for a yes from the user
        // 2026-10-11: resuming on an approval, the steps before the waiting one already ran in the
        // first call. Their lines and pictures come from the approval; the loop starts at the step
        // that was waiting, and only if it is the very step the approval was issued for.
        let resume_at = match &self.o.approval.resume {
            None => 1,
            Some(r) => {
                let same = steps
                    .get(r.index.wrapping_sub(1))
                    .and_then(Value::as_object)
                    .is_some_and(|s| approval::step_id(s) == r.step_id);
                if !same {
                    return Err(StepError::permission(APPROVAL_MISMATCH));
                }
                notes = r.notes.clone();
                shots = r.shots.clone();
                r.index
            }
        };
        let input_before = self.be().input_events();
        let mut acted_in_call = resume_at > 1; // an acting step has already finished in this call
        let mut pre_input_failure = false; // a step failed before anything was sent
        for (i, item) in items.iter().enumerate() {
            if i + 1 < resume_at {
                continue;
            }
            let Value::Object(step) = &item.step else {
                return Err(StepError::value(format!(
                    "step {} must be an object with an action",
                    i + 1
                )));
            };
            let notes_before_step = notes.len();
            if !item.header.is_empty() {
                notes.push(item.header.clone()); // a recipe step: say which recipe is running
            }
            let outcome = self.skip_reason(step).and_then(|skip| {
                if skip.is_empty() {
                    // a recipe's own only_apps narrows the call's for its steps
                    self.run_scoped_approved(step, item.scope.as_ref(), own_args, i + 1)
                        .map(Some)
                } else {
                    notes.push(format!(
                        "step {} ({}) skipped: {skip}",
                        i + 1,
                        step_action_text(step)
                    ));
                    Ok(None)
                }
            });
            let (note, img) = match outcome {
                Ok(Some(done)) => done,
                Ok(None) => continue, // a `when` that does not hold right now: leave this step out
                Err(exc) => {
                    if on_fail_skips(step) {
                        // on_fail "skip": say so, then carry on (not worded "failed:")
                        notes.push(format!(
                            "step {} ({}) errored and was skipped: {exc}",
                            i + 1,
                            step_action_text(step)
                        ));
                        continue;
                    }
                    notes.push(format!(
                        "step {} ({}) failed: {}",
                        i + 1,
                        step_action_text(step),
                        exc
                    ));
                    if let StepError::Approval(need) = &exc {
                        pending = Some(Pending {
                            need: self.complete_need(step, need),
                            step_id: approval::step_id(step),
                            index: i + 1,
                            notes: notes[..notes_before_step].to_vec(),
                            shots: shots.clone(),
                        });
                    }
                    // 2026-10-08 (W3): a validation error, unknown action, bad or missing target or a
                    // refusal (ValueError incl. TargetError, PermissionError incl. destructive and
                    // protected) raised before any input went out leaves the screen as the caller last
                    // saw it, so no screenshot is attached (a bogus action used to cost a 126 KB
                    // image). Anything else, or any failure after earlier steps acted or SendInput
                    // ran, still gets the picture.
                    pre_input_failure = !acted_in_call
                        && self.be().input_events() == input_before
                        && exc.is_validation();
                    break;
                }
            };
            let step_action = step
                .get("action")
                .map(|v| py_str(v).trim().to_string())
                .unwrap_or_default();
            if is_action(&step_action) && !is_passive(&step_action) {
                acted_in_call = true;
            }
            // 2026-10-11: shot=false means text only, but find_text still attached a crop (~300
            // tokens a hit). A picture stays only where the step itself asks for one.
            let img =
                img.filter(|_| want_shot || matches!(step_action.as_str(), "zoom" | "screenshot"));
            let has_img = img.is_some();
            if let Some(img) = img {
                shots.push(img);
                notes.push(format!("{note} [image {}]", shots.len()));
            } else {
                notes.push(note.clone());
            }
            if note.contains("expect: FAIL") {
                if on_fail_skips(step) {
                    continue;
                }
                if steps.len() > 1 {
                    // the same "step N (action) failed:" line every other stopping failure carries, so
                    // the daemon's failed_step field (and wh's exit code) see it (live run 2026-10-08)
                    notes.push(format!(
                        "step {} ({}) failed: expect not met, batch stopped",
                        i + 1,
                        if step_action.is_empty() {
                            "?"
                        } else {
                            &step_action
                        }
                    ));
                }
                break;
            }
            if !has_img && i + 1 < steps.len() {
                let raw_action = step.get("action").and_then(Value::as_str);
                let pointer_or_input = raw_action.is_some_and(|a| {
                    POINTER_ACTIONS.contains(&a) || matches!(a, "key" | "type" | "window")
                });
                if pointer_or_input && self.bg_active() {
                    // background mode: nothing to wait for on the shared screen or keyboard focus
                    self.sleep(0.15);
                } else if pointer_or_input {
                    let text = super::args::text(step, "text");
                    let next_is_pointer = steps[i + 1]
                        .get("action")
                        .and_then(Value::as_str)
                        .is_some_and(|a| POINTER_ACTIONS.contains(&a));
                    if raw_action == Some("key") && next_is_pointer {
                        // a key that selects something, then a click: no redraw to wait for
                        self.sleep(0.08);
                    } else if raw_action == Some("key") && normalize_key_text(&text) == "f2" {
                        if let Err(exc) = self.wait_rename_box() {
                            notes.push(format!("step {} and later skipped: {exc}", i + 2));
                            break;
                        }
                    } else if raw_action == Some("key") && is_opener(&normalize_key_text(&text)) {
                        // 2026-09-21 eval: ctrl+n then type in one batch saved an EMPTY file: the
                        // paste landed before Notepad's new tab (or the Save As dialog) had focus.
                        self.wait_focus_change()?;
                    } else {
                        // One calm read is enough here: the next step acts anyway (W4, 2026-10-08).
                        self.settle(Some(0.35), 0.0, 1)?; // let each step land before the next one
                    }
                }
            }
        }
        let last_raw = steps
            .last()
            .and_then(|s| s.as_object())
            .and_then(|o| o.get("action"));
        let last: Option<&str> = last_raw.and_then(Value::as_str);
        if self.cfg().auto_shot() && !Self::no_trailing_shot(last) && !pre_input_failure {
            let acted = last.is_some_and(|a| {
                !matches!(a, "wait" | "wait_for_text" | "wait_for_file") && is_action(a)
            });
            if want_shot || acted {
                // Acting calls always settle, even with shot=false: the next call must find the UI at
                // rest.
                let min_s = if acted { self.settle_floor_s() } else { 0.0 };
                // 2026-10-11: a "no visible change" note was tried here and removed: settle's first look
                // comes after the click has drawn, so it fired on clicks that worked (REAPER live test)
                let (waited, _) = self.settle_watch(None, min_s, 2)?;
                if !self.bg_active() {
                    self.remember_target();
                }
                if want_shot {
                    shots.push(self.screenshot_b64(None)?);
                    notes.push(format!(
                        "screen settled in {:.0} ms [image {}: {}]",
                        waited * 1000.0,
                        shots.len(),
                        self.image_tag()
                    ));
                } else {
                    notes.push(format!("screen settled in {:.0} ms", waited * 1000.0));
                }
            }
        }
        Ok(ManyResult {
            text: notes.join("\n"),
            images: shots,
            mime: self.mime(),
            approval: pending,
        })
    }

    /// Run one step; if it stops for approval and the user already said yes to exactly this step and
    /// target, run it again with just that one refusal switched off.
    fn run_scoped_approved(
        &self,
        step: &Args,
        scope: Option<&BTreeSet<String>>,
        call_args: bool,
        index: usize,
    ) -> StepResult<super::actions::StepOut> {
        match self.run_scoped(step, scope, call_args) {
            Err(StepError::Approval(need)) => {
                let granted = self
                    .o
                    .approval
                    .resume
                    .as_ref()
                    .is_some_and(|r| r.index == index && r.key == need.key());
                if !granted {
                    return Err(StepError::Approval(need));
                }
                let mut again = step.clone();
                let mut o = CallOpts {
                    approval: Approving::default(), // the yes is spent on this one rerun
                    ..self.o.clone()
                };
                if matches!(need.kind, Kind::Destructive(_)) {
                    o.policy = Policy::allow();
                } else {
                    again.insert("allow_irreversible".into(), Value::Bool(true));
                }
                let ctx = Ctx {
                    c: self.c,
                    o,
                    who: self.who,
                };
                ctx.run_scoped(&again, scope, call_args)
            }
            other => other,
        }
    }

    /// The need as the card will say it: what the step does and where, filled in from the step and
    /// the front window when the guard did not know.
    fn complete_need(&self, step: &Args, need: &Need) -> Need {
        let mut n = need.clone();
        if n.action.is_empty() {
            n.action = approval::describe_step(step, false);
            n.action_log = approval::describe_step(step, true);
        }
        if n.place.is_empty()
            && let Some(app) = self
                .fg_root()
                .map(|w| self.app_label(w))
                .filter(|a| !a.is_empty())
        {
            n.place = format!("in {app}");
        }
        n
    }
}

/// Said when an approval is used on a call whose waiting step is not the one it was issued for.
const APPROVAL_MISMATCH: &str = "that approval was issued for a different step than the one now \
    waiting, so nothing ran. Ask again by repeating the original call";

impl Computer {
    /// `browser_*` steps go to the browser module and never touch the desktop: no screenshot, no
    /// settling, no window guard. `None` when the call has no browser step. A batch is all browser or
    /// all desktop, because a desktop step's trailing screenshot means nothing for a page.
    fn run_browser(&self, args: &Args, appr: &Approving) -> Option<Result<ManyResult, StepError>> {
        let steps: Vec<Value> = match args.get("actions") {
            Some(Value::Array(list)) if !list.is_empty() => list.clone(),
            _ => vec![Value::Object(args.clone())],
        };
        let is_browser = |s: &Value| {
            s.get("action")
                .and_then(Value::as_str)
                .is_some_and(crate::browser::is_browser_action)
        };
        let count = steps.iter().filter(|s| is_browser(s)).count();
        if count == 0 {
            return None;
        }
        if count != steps.len() {
            return Some(Err(StepError::value(
                "browser_* actions cannot be mixed with desktop actions in one batch; send them as separate calls",
            )));
        }
        let Some(hook) = self.browser.get() else {
            return Some(Err(StepError::value(
                "the browser module is not available in this build",
            )));
        };
        let mut notes: Vec<String> = Vec::new();
        let mut images: Vec<String> = Vec::new();
        let mut pending: Option<Pending> = None;
        let resume_at = match &appr.resume {
            None => 1,
            Some(r) => {
                let same = steps
                    .get(r.index.wrapping_sub(1))
                    .and_then(Value::as_object)
                    .is_some_and(|s| approval::step_id(s) == r.step_id);
                if !same {
                    return Some(Err(StepError::permission(APPROVAL_MISMATCH)));
                }
                notes = r.notes.clone();
                images = r.shots.clone();
                r.index
            }
        };
        for (i, step) in steps.iter().enumerate() {
            if i + 1 < resume_at {
                continue;
            }
            let Value::Object(map) = step else {
                notes.push(format!(
                    "step {} (?) failed: a step must be an object",
                    i + 1
                ));
                break;
            };
            let name = step_action_text(map);
            // the key of an approval for exactly this step; the browser checks it against the
            // element it finds, so a page that changed since the card was shown asks again
            let approved = appr
                .resume
                .as_ref()
                .filter(|r| r.index == i + 1)
                .map(|r| r.key.as_str());
            match hook.run(map, approved) {
                Ok(out) => {
                    let mut text = out.text;
                    for img in out.images {
                        images.push(img);
                        text.push_str(&format!(" [image {}]", images.len()));
                    }
                    notes.push(text);
                }
                Err(e) => {
                    let before = notes.len();
                    notes.push(format!("step {} ({name}) failed: {}", i + 1, e.message()));
                    if let crate::browser::BrowserFail::Approval(need) = &e {
                        let mut need = (**need).clone();
                        if need.action.is_empty() {
                            need.action = format!("Run {name}");
                        }
                        pending = Some(Pending {
                            need,
                            step_id: approval::step_id(map),
                            index: i + 1,
                            notes: notes[..before].to_vec(),
                            shots: images.clone(),
                        });
                    }
                    break;
                }
            }
        }
        Some(Ok(ManyResult {
            text: notes.join("\n"),
            images,
            mime: "image/jpeg",
            approval: pending,
        }))
    }

    /// A context for work outside a `computer` call: the configured defaults, no allowlist.
    pub(crate) fn read_ctx<'a>(&'a self, who: &'a Caller) -> Ctx<'a> {
        Ctx {
            c: self,
            o: CallOpts::defaults(&self.cfg, Policy::REFUSE),
            who,
        }
    }

    /// The `computer` tool: one action or `{"actions": [...]}`. Stops at the first failing step and
    /// names it. Returns the text and every picture the batch took, plus one at the end after the
    /// screen settles. `policy` is this call's destructive opt-in (see
    /// [`Policy::take_from_args`]); `who` is the process driving this connection. Python `run_many`.
    pub fn run_many(
        &self,
        args: &Args,
        policy: Policy,
        who: &Caller,
    ) -> Result<ManyResult, StepError> {
        self.run_many_with(args, policy, who, Approving::default())
    }

    /// [`Computer::run_many`] for a call that may stop for a card (`appr.can_ask`) or resumes on an
    /// approval the user gave (`appr.resume`).
    pub fn run_many_with(
        &self,
        args: &Args,
        policy: Policy,
        who: &Caller,
        appr: Approving,
    ) -> Result<ManyResult, StepError> {
        if let Some(done) = self.run_browser(args, &appr) {
            return done;
        }
        let mut o = CallOpts::from_args(self, args, policy)?;
        o.approval = appr;
        let ctx = Ctx { c: self, o, who };
        // after `only_apps`, because the window goes through the same allowlist check
        ctx.apply_call_scope(args)?;
        ctx.run_steps(args)
    }

    /// The whole `computer` route of the daemon: the per-call opt-in, the pause flag, the
    /// acknowledgement gate, the rate cap, one action at a time, the audit entry, and the failure
    /// fields. Never fails: a refusal or an error comes back as `ok: false`. Python
    /// `_execute_tool(path="computer", ...)`.
    pub fn execute(&self, args: &Value, who: &Caller) -> ToolOutcome {
        self.idle.touch(self.clock.as_ref());
        let _one_at_a_time = self.tool_lock.lock().unwrap_or_else(|e| e.into_inner());
        let outcome = self.execute_locked(args, who);
        self.idle.touch(self.clock.as_ref());
        outcome
    }

    fn fail(&self, args: &Value, err: &StepError) -> ToolOutcome {
        let mut error = err.message().to_string();
        log_action(self.cfg.paths(), "computer", args, &error, false);
        error = self.fails.note_failure("computer", args, &error);
        ToolOutcome {
            ok: false,
            result: None,
            error: Some(error),
            images: Vec::new(),
            mime: "image/jpeg",
            failed_step: None,
            offer: None,
        }
    }

    /// Turn a step that stopped for approval into a token and a card sentence. `None` when no
    /// randomness is available; the caller then just gets the refusal.
    fn offer_for(&self, p: &Pending) -> Option<Offer> {
        let entry = Entry {
            step_id: p.step_id.clone(),
            index: p.index,
            key: p.need.key(),
            expires: std::time::Duration::ZERO,
            notes: p.notes.clone(),
            shots: p.shots.clone(),
        };
        let token = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mint(entry, self.clock.now())?;
        Some(Offer {
            token,
            summary: p.need.summary(),
            log_summary: p.need.log_summary(),
            kind: p.need.kind.tag(),
            step: p.index,
        })
    }

    /// Turn a step that stopped for approval into a chat code and the refusal text that tells
    /// Claude what to show the user. `None` when the step cannot be approved or no randomness is
    /// available; the caller then just gets the refusal.
    fn code_for(&self, p: &Pending) -> Option<String> {
        if !p.need.approvable() {
            return None;
        }
        let entry = Entry {
            step_id: p.step_id.clone(),
            index: p.index,
            key: p.need.key(),
            expires: std::time::Duration::ZERO,
            notes: p.notes.clone(),
            shots: p.shots.clone(),
        };
        let code = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mint_code(entry, &p.need.log_summary(), self.clock.now())?;
        Some(approval::code_refusal(&p.need.summary(), &code))
    }

    /// the user's answer to a chat code (`winhands approve|deny CODE`). The text is the one line the
    /// command prints; the error says why the code did not take.
    pub fn answer_approval_code(&self, code: &str, allow: bool) -> Result<String, String> {
        let code = approval::normalize_code(code);
        let summary = self
            .approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .answer_code(&code, allow, self.clock.now())
            .map_err(|e| match e {
                approval::CodeError::Unknown => {
                    format!("no waiting approval {code} (already answered, used, or never issued)")
                }
                approval::CodeError::Expired => format!("approval {code} expired"),
                other => other.message(),
            })?;
        log_action(
            self.cfg.paths(),
            "approval",
            &serde_json::json!({
                "answer": if allow { "allow" } else { "deny" },
                "via": "chat code",
                "code": code,
                "card": summary,
            }),
            if allow { "allowed" } else { "denied" },
            allow,
        );
        Ok(format!(
            "{} {code}: {summary}",
            if allow { "approved" } else { "denied" }
        ))
    }

    fn execute_locked(&self, raw: &Value, who: &Caller) -> ToolOutcome {
        let Value::Object(map) = raw else {
            return self.fail(raw, &StepError::value("tool args must be an object"));
        };
        let mut map = map.clone();
        // The owner's hard line. Per call, never sticky: the refusals are on unless THIS call asked
        // for them off, and the next call starts from on again.
        let policy = Policy::take_from_args(&mut map);
        // 2026-10-11: approval cards. `approvable` says the shim can show one; `approval_token` is
        // the yes it brings back. Neither is ever forwarded or logged as an argument.
        let can_ask = map.remove("approvable").is_some_and(|v| truthy(&v));
        let token = map
            .remove("approval_token")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|t| !t.is_empty());
        // 2026-10-11: no card possible (the client cannot show one): the user answers in chat, the
        // `winhands approve CODE` hook tells the daemon, and the resent call brings the code back.
        let code = map
            .remove("approval_code")
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|t| !t.trim().is_empty());
        let args = Value::Object(map.clone());
        if self.pause.is_paused() {
            return self.fail(&args, &StepError::permission("paused by user hotkey"));
        }
        if let Err(refused) = crate::ack::check_acknowledged(&self.cfg, "computer", &args) {
            return self.fail(&args, &refused.into());
        }
        if !self.rate.check(self.clock.as_ref()) {
            return self.fail(&args, &StepError::runtime("rate limit exceeded"));
        }
        let resume = match (token, code) {
            (Some(t), _) => {
                let taken = self
                    .approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(&t, self.clock.now());
                match taken {
                    Ok(entry) => Some(entry),
                    Err(why) => {
                        return self.fail(&args, &StepError::permission(why.message()));
                    }
                }
            }
            (None, Some(c)) => {
                let taken = self
                    .approvals
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_code(&c, self.clock.now());
                match taken {
                    Ok(entry) => Some(entry),
                    Err(why) => {
                        return self.fail(&args, &StepError::permission(why.message()));
                    }
                }
            }
            (None, None) => None,
        };
        let resumed = resume.is_some();
        match self.run_many_with(&map, policy, who, Approving { can_ask, resume }) {
            Err(e) => self.fail(&args, &e),
            Ok(mut done) => {
                // a card for a client that can show one; otherwise a chat code
                let offer = if can_ask {
                    done.approval.as_ref().and_then(|p| self.offer_for(p))
                } else {
                    None
                };
                let mut code_card = None;
                if !can_ask && let Some(p) = done.approval.as_ref() {
                    code_card = self.code_for(p).map(|text| {
                        done.text = match done.text.strip_suffix(p.need.reason.as_str()) {
                            Some(head) if !p.need.reason.is_empty() => format!("{head}{text}"),
                            _ => format!("{}\n{text}", done.text),
                        };
                        p.need.log_summary()
                    });
                }
                let steps = match map.get("actions") {
                    Some(Value::Array(a)) if !a.is_empty() => a.len(),
                    _ => 1,
                };
                let ok = !done.text.contains("failed:");
                let mut entry = Map::new();
                entry.insert(
                    "action".into(),
                    map.get("action").cloned().unwrap_or(Value::Null),
                );
                entry.insert("steps".into(), Value::from(steps));
                // 2026-10-08: which recipes ran or were saved (param NAMES only, never values)
                if let Some(Value::Object(extra)) = crate::recipes::audit_fields(&args) {
                    entry.extend(extra);
                }
                // 2026-10-11: every card the daemon offered and every approval it honoured
                if let Some(o) = &offer {
                    entry.insert("approval_card".into(), Value::from(o.log_summary.clone()));
                }
                if let Some(summary) = code_card {
                    entry.insert("approval_code_card".into(), Value::from(summary));
                }
                if resumed {
                    entry.insert("approval_used".into(), Value::Bool(true));
                }
                log_action(
                    self.cfg.paths(),
                    "computer",
                    &Value::Object(entry),
                    &done.text,
                    ok,
                );
                let failure = if ok {
                    None
                } else {
                    batch_failure_fields(&done.text)
                };
                ToolOutcome {
                    ok: true,
                    result: Some(done.text),
                    error: failure.as_ref().map(|(_, e)| e.clone()),
                    images: done.images,
                    mime: done.mime,
                    failed_step: failure.map(|(n, _)| n),
                    offer,
                }
            }
        }
    }
}
