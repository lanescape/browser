//! The `native-context-menu` capability, with fixed command IDs. Chromium hands a Content embedder no
//! menu model, so the edit commands that apply are listed here from what Blink says the page can do.
use crate::{
    post,
    protocol::{self, control},
};
use control::event::Event;
use serde_json::{Value, json};
use std::collections::HashMap;

pub const TOKEN: &str = "native-context-menu";
const LIMIT: usize = 64;
const BACK: i32 = 100;
const FORWARD: i32 = 101;
const UNDO: i32 = 110;
const REDO: i32 = 111;
const CUT: i32 = 112;
const COPY: i32 = 113;
const PASTE: i32 = 114;
const PASTE_MATCH_STYLE: i32 = 115;
const DELETE: i32 = 116;
const SELECT_ALL: i32 = 117;
/// Blink's edit flags (third_party/blink/public/common/context_menu_data/edit_flags.h).
const CAN_UNDO: u64 = 0x1;
const CAN_REDO: u64 = 0x2;
const CAN_CUT: u64 = 0x4;
const CAN_COPY: u64 = 0x8;
const CAN_PASTE: u64 = 0x10;
const CAN_DELETE: u64 = 0x20;
const CAN_SELECT_ALL: u64 = 0x40;
/// InteractionClosed.Kind.CONTEXT_MENU.
const CLOSED_KIND: i32 = 2;

struct Menu {
    tab: u64,
    owner: u64,
    commands: Vec<i32>,
}

#[derive(Default)]
pub struct Menus {
    enabled: bool,
    next: u64,
    open: HashMap<u64, Menu>,
}

fn closed(tab: u64, request: u64) {
    protocol::emit(control::Output {
        body: Some(control::output::Body::Event(control::Event {
            event: Some(Event::InteractionClosed(control::InteractionClosed {
                tab,
                request,
                kind: CLOSED_KIND,
            })),
        })),
    });
}

fn item(command: i32, label: &str, enabled: bool) -> control::MenuItem {
    control::MenuItem {
        command,
        label: label.into(),
        kind: 1,
        enabled,
        ..Default::default()
    }
}

fn separator() -> control::MenuItem {
    control::MenuItem {
        command: -1,
        kind: 4,
        ..Default::default()
    }
}

/// The items of a menu. Without `clipboard`, Cut, Copy and both Pastes are listed disabled: nothing
/// here is the viewing client's clipboard.
fn items(p: &Value, clipboard: bool) -> Vec<control::MenuItem> {
    let flags = p["flags"].as_u64().unwrap_or(0);
    let can = |flag: u64| flags & flag != 0;
    if p["editable"] == true {
        vec![
            item(UNDO, "&Undo", can(CAN_UNDO)),
            item(REDO, "&Redo", can(CAN_REDO)),
            separator(),
            item(CUT, "Cu&t", clipboard && can(CAN_CUT)),
            item(COPY, "&Copy", clipboard && can(CAN_COPY)),
            item(PASTE, "&Paste", clipboard && can(CAN_PASTE)),
            item(
                PASTE_MATCH_STYLE,
                "Paste and match style",
                clipboard && can(CAN_PASTE),
            ),
            item(DELETE, "&Delete", can(CAN_DELETE)),
            separator(),
            item(SELECT_ALL, "Select &all", can(CAN_SELECT_ALL)),
        ]
    } else if p["selection"] == true {
        vec![item(COPY, "&Copy", clipboard && can(CAN_COPY))]
    } else {
        vec![
            item(BACK, "&Back", p["back"] == true),
            item(FORWARD, "&Forward", p["forward"] == true),
            separator(),
            item(SELECT_ALL, "Select &all", can(CAN_SELECT_ALL)),
        ]
    }
}

impl Menus {
    pub fn enable(&mut self) {
        self.enabled = true;
    }

    /// The page asked for a menu; its renderer has already passed its `contextmenu` event.
    pub fn opened(&mut self, tab: u64, viewer: u64, p: &Value, clipboard: bool) {
        // One menu per tab, as one person has one: the newest is the live one.
        self.cancel_tab(tab);
        if !self.enabled || self.open.len() >= LIMIT {
            post(json!({"op":"menu_closed","tab":tab}));
            return;
        }
        let items = items(p, clipboard);
        let commands = items
            .iter()
            .filter(|i| i.enabled && i.kind == 1)
            .map(|i| i.command)
            .collect();
        self.next += 1;
        let request = self.next;
        let sent = protocol::emit(control::Output {
            body: Some(control::output::Body::Event(control::Event {
                event: Some(Event::ContextMenu(control::ContextMenu {
                    tab,
                    request,
                    x: p["x"].as_i64().unwrap_or(0) as i32,
                    y: p["y"].as_i64().unwrap_or(0) as i32,
                    items,
                    viewer,
                })),
            })),
        });
        if !sent {
            post(json!({"op":"menu_closed","tab":tab}));
            return;
        }
        self.open.insert(
            request,
            Menu {
                tab,
                owner: viewer,
                commands,
            },
        );
    }

    /// The viewer captured by a pending menu of the tab offering `command` enabled.
    pub fn owner(&self, tab: u64, request: u64, command: i32) -> Result<u64, String> {
        self.open
            .get(&request)
            .filter(|m| m.tab == tab && m.commands.contains(&command))
            .map(|m| m.owner)
            .ok_or_else(|| "unknown context menu or unavailable command".into())
    }

    /// Whether `viewer`'s pending menu of the tab offers `command` enabled.
    pub fn allows(&self, tab: u64, request: u64, viewer: u64, command: i32) -> Result<(), String> {
        if self.owner(tab, request, command)? != viewer {
            return Err("the menu belongs to another viewer".into());
        }
        Ok(())
    }

    /// A clipboard operation took over the menu's choice.
    pub fn resolve(&mut self, tab: u64, request: u64) {
        if self.open.remove(&request).is_some() {
            post(json!({"op":"menu_closed","tab":tab}));
            closed(tab, request);
        }
    }

    /// ContextMenuReply, for anything but the clipboard's commands.
    pub fn reply(&mut self, p: &Value) -> Result<(), String> {
        let tab = p["tab"].as_u64().ok_or("tab required")?;
        let request = p["request"].as_u64().unwrap_or(0);
        let menu = self
            .open
            .get(&request)
            .filter(|m| m.tab == tab)
            .ok_or("unknown context menu")?;
        let command = p["command"].as_i64().unwrap_or(0) as i32;
        let cancel = p["cancel"] == true;
        if !cancel && !menu.commands.contains(&command) {
            return Err("unavailable menu command".into());
        }
        self.open.remove(&request);
        if !cancel {
            post(json!({"op":"menu_command","tab":tab,"command":command}));
        }
        post(json!({"op":"menu_closed","tab":tab}));
        closed(tab, request);
        Ok(())
    }

    /// Invalidates the tab's menus: a main-frame navigation, its renderer ending, EOF or close.
    pub fn cancel_tab(&mut self, tab: u64) {
        let requests: Vec<_> = self
            .open
            .iter()
            .filter(|(_, m)| m.tab == tab)
            .map(|(request, _)| *request)
            .collect();
        for request in requests {
            self.resolve(tab, request);
        }
    }

    /// A replacement controller retires menus captured by another viewer.
    pub fn controller_changed(&mut self, tab: u64, viewer: u64) {
        if viewer != 0
            && self
                .open
                .values()
                .any(|m| m.tab == tab && m.owner != viewer)
        {
            self.cancel_tab(tab);
        }
    }

    /// A captured viewer's connection closed.
    pub fn disconnected(&mut self, tab: u64, viewer: u64) {
        if self
            .open
            .values()
            .any(|m| m.tab == tab && m.owner == viewer)
        {
            self.cancel_tab(tab);
        }
    }

    pub fn shutdown(&mut self) {
        let tabs: Vec<_> = self.open.values().map(|m| m.tab).collect();
        for tab in tabs {
            self.cancel_tab(tab);
        }
    }
}
