//! Native IME: the controlling viewer composes with its own input method; the engine reports the
//! focused editor and caret so the viewer can place its candidate window. Each request runs in the
//! renderer of the focused frame through Blink's own input method controller, as one step with its
//! check, and comes back with exactly where the composition and caret are. Every editor shows its
//! composition in the page, as with a local input method. Diagnostics never record composition text
//! or caret positions.
use crate::{
    post,
    protocol::{self, control},
};
use control::event::Event;
use serde_json::{Value, json};
use std::{collections::HashMap, time::Duration};

pub const TOKEN: &str = "native-ime";
const TEXT: usize = 16384;
const UNDERLINES: usize = 64;
const BOUNDS: usize = 1024;
const APPLY: Duration = Duration::from_secs(2);

/// The editable element a tab's renderer last reported focused.
#[derive(Clone, Debug, Default, PartialEq)]
struct Focus {
    /// The browser's identifier of its frame.
    frame: String,
    /// That frame's report counter.
    counter: u64,
    /// The element's bounds when it was reported, in the tab's viewport.
    bounds: control::Rect,
}
/// A renderer's focus report: its frame and the editable element focused there, if any.
#[derive(Clone, Debug, PartialEq)]
struct Report {
    frame: String,
    focus: Option<Focus>,
}
#[derive(Default)]
struct Editor {
    focus: Option<Focus>,
    /// The editor ID issued to the controller; zero for none.
    editor: u64,
    controller: u64,
    composing: bool,
    /// The composition's caret and the bounds of each of its UTF-16 units, as the renderer last said.
    caret: control::Rect,
    bounds: Vec<control::Rect>,
    /// The state last emitted to the controller.
    sent: Option<control::EditorState>,
    /// Counts focus and document changes: accepted reports and documents going away.
    generation: u64,
}
impl Editor {
    /// Ends the composition; true when one was in progress.
    fn end_composition(&mut self) -> bool {
        self.caret = control::Rect::default();
        self.bounds.clear();
        std::mem::take(&mut self.composing)
    }
    /// A renderer reported focus. Only the browser's focused frame, `focused`, can say where
    /// focus is: a report from any other frame is late, from a frame focus has left, or untrue,
    /// so it changes nothing. A frame's report can't overtake its own focus notice, which goes first
    /// on the same channel. True when a composition ended.
    fn report(
        &mut self,
        report: Report,
        focused: Option<&str>,
        mint: impl FnOnce() -> u64,
    ) -> bool {
        if focused != Some(report.frame.as_str()) {
            return false;
        }
        self.generation += 1;
        let focus = report.focus;
        self.editor = if focus.is_some() && self.controller != 0 {
            mint()
        } else {
            0
        };
        self.focus = focus;
        self.end_composition()
    }
    /// `frame`'s document went away; None for every frame. True when a composition ended.
    fn frame_gone(&mut self, frame: Option<&str>) -> bool {
        if frame.is_none() {
            self.generation += 1;
        }
        if !self
            .focus
            .as_ref()
            .is_some_and(|f| frame.is_none_or(|frame| f.frame == frame))
        {
            return false;
        }
        if frame.is_some() {
            self.generation += 1;
        }
        self.focus = None;
        self.editor = 0;
        self.end_composition()
    }
    fn state(&self, tab: u64) -> Option<control::EditorState> {
        if self.controller == 0 {
            return None;
        }
        let focus = self.focus.as_ref().filter(|_| self.editor != 0);
        let composing = self.composing && focus.is_some();
        let bounds = if composing { &self.bounds[..] } else { &[] };
        let caret = if composing {
            self.caret
        } else {
            focus.map_or_else(control::Rect::default, |f| f.bounds)
        };
        Some(control::EditorState {
            tab,
            viewer: self.controller,
            editor: self.editor,
            composing,
            caret: Some(caret),
            character_bounds: bounds.iter().take(BOUNDS).copied().collect(),
            inline_composition: focus.is_some(),
        })
    }
}
fn retired(tab: u64, viewer: u64) {
    emit(control::EditorState {
        tab,
        viewer,
        caret: Some(control::Rect::default()),
        ..Default::default()
    });
}
fn emit(state: control::EditorState) {
    protocol::emit(control::Output {
        body: Some(control::output::Body::Event(control::Event {
            event: Some(Event::EditorState(state)),
        })),
    });
}

#[derive(Debug, PartialEq)]
enum Action {
    Preedit {
        selection: (u32, u32),
        underlines: Vec<Underline>,
    },
    Commit,
    Finish,
    Cancel,
}
#[derive(Debug, PartialEq)]
struct Underline {
    range: (u32, u32),
    color: u32,
    background_color: u32,
    thick: bool,
    style: i32,
}
/// An ImeInput's action and values, checked against its own text.
fn parse(p: &Value) -> Result<(Action, String), String> {
    let number = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0);
    let text = p["text"].as_str().unwrap_or("").to_owned();
    if text.len() > TEXT {
        return Err("IME text exceeds 16384 bytes".into());
    }
    let length = text.encode_utf16().count() as u64;
    let action = match number(p, "action") {
        1 => {
            let (start, end) = (number(p, "selection_start"), number(p, "selection_end"));
            if start > end || end > length {
                return Err("IME selection out of range".into());
            }
            let underlines = p["underlines"].as_array().map_or(&[][..], Vec::as_slice);
            if underlines.len() > UNDERLINES {
                return Err("more than 64 IME underlines".into());
            }
            let underlines = underlines
                .iter()
                .map(|u| {
                    let (from, to) = (number(u, "start"), number(u, "end"));
                    // Signed: an unknown negative enum value is not SOLID.
                    let style = u["style"].as_i64().unwrap_or(0);
                    if from >= to || to > length || !(0..=3).contains(&style) {
                        return Err("IME underline out of range".to_owned());
                    }
                    Ok(Underline {
                        range: (from as u32, to as u32),
                        color: number(u, "color") as u32,
                        background_color: number(u, "background_color") as u32,
                        thick: u["thick"] == true,
                        style: style as i32,
                    })
                })
                .collect::<Result<_, _>>()?;
            Action::Preedit {
                selection: (start as u32, end as u32),
                underlines,
            }
        }
        2 => Action::Commit,
        3 => Action::Finish,
        4 => Action::Cancel,
        _ => return Err("unknown IME action".into()),
    };
    Ok((action, text))
}

/// An accepted ImeInput, ready for the page.
pub struct Input {
    tab: u64,
    /// The frame of the editor it was checked against, and that frame's report.
    frame: String,
    counter: u64,
    action: Action,
    text: String,
}

#[derive(Default)]
pub struct Ime {
    next: u64,
    tabs: HashMap<u64, Editor>,
    /// The frame each tab's browser side has focused.
    focused: HashMap<u64, String>,
    /// Renderer answers to editor checks since the last call: token, frame, confirmed.
    answers: Vec<(u64, String, bool)>,
}

impl Ime {
    /// A renderer's focus report.
    pub fn report(&mut self, p: &Value) {
        let tab = p["tab"].as_u64().unwrap_or(0);
        let frame = p["frame"].as_str().unwrap_or("").to_owned();
        crate::diagnostic(format_args!(
            "focus report tab {tab} frame {frame} counter {} editable {} generation {}",
            p["counter"],
            p["editable"],
            self.generation(tab)
        ));
        let focus = (p["editable"] == true).then(|| Focus {
            frame: frame.clone(),
            counter: p["counter"].as_u64().unwrap_or(0),
            bounds: control::Rect {
                x: p["x"].as_i64().unwrap_or(0) as i32,
                y: p["y"].as_i64().unwrap_or(0) as i32,
                width: p["width"].as_i64().unwrap_or(0) as i32,
                height: p["height"].as_i64().unwrap_or(0) as i32,
            },
        });
        // Focus can leave and come back between the browser side's looks at it, so this may be the
        // first the host hears of where it went.
        if let Some(focused) = p["focused_frame"].as_str() {
            self.frame_focused(tab, focused);
        }
        let focused = self.focused.get(&tab).cloned();
        let report = Report { frame, focus };
        let next = &mut self.next;
        // Chromium itself commits a composition when its editor loses focus, as a local input
        // method does: nothing to cancel.
        self.tabs
            .entry(tab)
            .or_default()
            .report(report, focused.as_deref(), || {
                *next += 1;
                *next
            });
    }

    /// The browser's focused frame changed. An editor in another frame has lost focus, whether or
    /// not its own frame says so.
    pub fn frame_focused(&mut self, tab: u64, frame: &str) {
        self.focused.insert(tab, frame.to_owned());
        if let Some(e) = self.tabs.get_mut(&tab)
            && e.focus.as_ref().is_some_and(|f| f.frame != frame)
        {
            e.generation += 1;
            e.focus = None;
            e.editor = 0;
            e.end_composition();
        }
    }

    /// Renderer answers received since the last call.
    pub fn answers(&mut self) -> Vec<(u64, String, bool)> {
        std::mem::take(&mut self.answers)
    }

    /// Whether the request still names the current editor, once the keys before it have reached
    /// this point. The renderer checks again as it applies it. Returns its frame, answered at once,
    /// or None when the editor is gone.
    pub fn send_check(&mut self, request: &Value, token: u64) -> Option<String> {
        let p = &request["params"];
        let tab = p["tab"].as_u64()?;
        let e = self.tabs.get(&tab).filter(|e| e.editor != 0)?;
        if p["editor"].as_u64() != Some(e.editor) {
            return None;
        }
        let frame = e.focus.as_ref()?.frame.clone();
        self.answers.push((token, frame.clone(), true));
        Some(frame)
    }

    /// Emits each controller's latest state once it changed.
    pub fn tick(&mut self, enabled: bool) {
        if !enabled {
            return;
        }
        let mut changed = Vec::new();
        for (tab, e) in self.tabs.iter_mut() {
            let state = e.state(*tab);
            if state != e.sent {
                changed.extend(state.clone());
                e.sent = state;
            }
        }
        for state in changed {
            emit(state);
        }
    }

    /// CONTROL moved to `viewer` (zero: nobody). True when it changed.
    pub fn controller_changed(&mut self, tab: u64, viewer: u64, enabled: bool) -> bool {
        let next = &mut self.next;
        let e = self.tabs.entry(tab).or_default();
        if e.controller == viewer {
            return false;
        }
        let old = std::mem::replace(&mut e.controller, viewer);
        e.editor = if viewer != 0 && e.focus.is_some() {
            *next += 1;
            *next
        } else {
            0
        };
        e.sent = None;
        // Sent whether or not a composition is known of: a request the host has given up on may yet
        // start one. It's nothing if nothing is composing, and follows any such request to the page.
        let composing = e.end_composition();
        if composing || old != 0 {
            post(json!({"op":"ime","tab":tab,"kind":"cancel"}));
        }
        if old != 0 && enabled {
            retired(tab, old);
        }
        true
    }

    /// A viewer's connection closed; it loses CONTROL if it held it. True when CONTROL changed.
    pub fn disconnected(&mut self, tab: u64, viewer: u64, enabled: bool) -> bool {
        let controls = self.tabs.get(&tab).is_some_and(|e| e.controller == viewer);
        controls && self.controller_changed(tab, 0, enabled)
    }

    /// Cancels the tab's composition, if any, before the supervisor navigates it.
    pub fn cancel_composition(&mut self, tab: u64) {
        if self.tabs.get_mut(&tab).is_some_and(|e| e.end_composition()) {
            post(json!({"op":"ime","tab":tab,"kind":"cancel"}));
        }
    }

    /// A frame's document went away: it navigated or was detached. None means every frame, for
    /// main-frame navigation and renderer termination.
    pub fn frame_gone(&mut self, tab: u64, frame: Option<&str>) {
        let cancel = match frame {
            Some(frame) => self
                .tabs
                .get_mut(&tab)
                .is_some_and(|e| e.frame_gone(Some(frame))),
            // Counted even before any focus report: the document changed.
            None => self.tabs.entry(tab).or_default().frame_gone(None),
        };
        if cancel {
            post(json!({"op":"ime","tab":tab,"kind":"cancel"}));
        }
    }

    /// Where the renderer says the tab's composition is, if it has one, after a request or because
    /// the page moved.
    pub fn layout(&mut self, tab: u64, p: &Value) {
        let Some(e) = self.tabs.get_mut(&tab) else {
            return;
        };
        let rect = |b: &Value| {
            let field = |key: &str| b[key].as_i64().unwrap_or(0) as i32;
            control::Rect {
                x: field("x"),
                y: field("y"),
                width: field("width"),
                height: field("height"),
            }
        };
        e.composing = p["composing"] == true;
        e.caret = rect(&p["caret"]);
        e.bounds = p["bounds"]
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(rect)
            .collect();
    }

    /// Counts the tab's focus and document changes.
    pub fn generation(&self, tab: u64) -> u64 {
        self.tabs.get(&tab).map_or(0, |e| e.generation)
    }

    /// The frame of the focused editable element and its renderer's report counter.
    pub fn focus_counter(&self, tab: u64) -> Option<(String, u64)> {
        let f = self.tabs.get(&tab)?.focus.as_ref()?;
        Some((f.frame.clone(), f.counter))
    }

    /// The frame of the focused editable element, whether or not anyone holds CONTROL.
    pub fn editor_frame(&self, tab: u64) -> Option<String> {
        self.tabs.get(&tab)?.focus.as_ref().map(|f| f.frame.clone())
    }

    pub fn closed_tab(&mut self, tab: u64, enabled: bool) {
        self.focused.remove(&tab);
        let Some(e) = self.tabs.remove(&tab) else {
            return;
        };
        if e.controller != 0 && enabled {
            retired(tab, e.controller);
        }
    }

    /// EOF: every composition is cancelled and every editor retired.
    pub fn shutdown(&mut self, enabled: bool) {
        for (tab, mut e) in std::mem::take(&mut self.tabs) {
            if e.end_composition() {
                post(json!({"op":"ime","tab":tab,"kind":"cancel"}));
            }
            if e.controller != 0 && enabled {
                retired(tab, e.controller);
            }
        }
    }

    /// Checks an ImeInput against the tab as it is now: on receipt, when its RFB input count is
    /// reached, and after its renderer confirmed the editor, immediately before it reaches the
    /// page. `controller` is the tab's holder of CONTROL and `dialog` whether a dialog is pending.
    pub fn check(&self, p: &Value, controller: u64, dialog: bool) -> Result<Input, String> {
        let tab = p["tab"].as_u64().ok_or("tab required")?;
        let viewer = p["viewer"].as_u64().unwrap_or(0);
        if viewer == 0 || controller != viewer {
            return Err("viewer does not hold CONTROL".into());
        }
        if dialog {
            return Err("JavaScript dialog pending".into());
        }
        let (current, frame, counter) = self.tabs.get(&tab).map_or((0, None, 0), |e| {
            let f = e.focus.as_ref();
            (
                e.editor,
                f.map(|f| f.frame.clone()),
                f.map_or(0, |f| f.counter),
            )
        });
        // The editor's frame must still be the one the browser has focused.
        if current == 0
            || p["editor"].as_u64() != Some(current)
            || frame.as_ref() != self.focused.get(&tab)
        {
            return Err("not the current editor".into());
        }
        let (action, text) = parse(p)?;
        Ok(Input {
            tab,
            frame: frame.unwrap_or_default(),
            counter,
            action,
            text,
        })
    }

    /// Applies an accepted ImeInput; the page answers `token`, and the engine refuses it there if
    /// focus has left the editor's frame.
    pub fn apply(&mut self, input: Input, token: i64) {
        let Input {
            tab,
            frame,
            counter,
            action,
            text,
        } = input;
        // How long the renderer has to start it, from now. The host's own bound, after which it
        // answers for the request, is longer.
        let deadline = crate::engine_now_us() + APPLY.as_micros() as i64;
        if let Action::Preedit {
            selection,
            underlines,
        } = &action
        {
            let underlines: Vec<Value> = underlines
                .iter()
                .map(|u| {
                    json!({"start":u.range.0,"end":u.range.1,"color":u.color as f64,"background":u.background_color as f64,"thick":u.thick,"style":u.style})
                })
                .collect();
            post(
                json!({"op":"ime","tab":tab,"frame":frame,"counter":counter,"token":token,"deadline_us":deadline,"kind":"preedit","text":text,"start":selection.0,"end":selection.1,"underlines":underlines}),
            );
            return;
        }
        let (kind, text) = match action {
            Action::Commit => ("commit", text),
            Action::Finish => ("finish", String::new()),
            _ => ("cancel", String::new()),
        };
        post(
            json!({"op":"ime","tab":tab,"frame":frame,"counter":counter,"token":token,"deadline_us":deadline,"kind":kind,"text":text}),
        );
    }
}
