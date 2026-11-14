//! The `clipboard` capability: explicit copy, cut and paste between the controlling viewer's
//! clipboard and the page, through transfer buffers. Each browsing context has its own clipboard (engine/clipboard.h),
//! holding data only for the frame of the operation running, and its own queue of operations, so a
//! host serves any number of contexts at once. Diagnostics never record clipboard contents.
use crate::{
    Host, diagnostic, post,
    protocol::{self, control},
};
use control::{clipboard_finished::Status, event::Event};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

pub const TOKEN: &str = "clipboard";
/// The only format offered in either direction.
pub const FORMAT: &str = "text/plain";
const CHUNK: usize = 49152;
/// A text representation's limit, in UTF-8 bytes.
const TEXT: usize = 1 << 20;
const REPRESENTATIONS: usize = 8;
const PER_TAB: usize = 16;
const PER_HOST: usize = 64;
/// Admitted operations waiting or running, across the host.
const OPERATIONS: usize = 64;
/// How long one step may take.
const STEP: Duration = Duration::from_secs(5);
/// The page's command runs only if its renderer starts it this soon after it was sent; once
/// started, it owns its context's clipboard until it ends or its frame goes away. The engine holds
/// the same value.
const RUN: Duration = Duration::from_millis(500);
const RUN_MARGIN: Duration = Duration::from_secs(1);

struct Transfer {
    tab: u64,
    viewer: u64,
    /// Copied out of the page; otherwise an upload for a paste.
    read: bool,
    size: usize,
    data: Vec<u8>,
    sealed: bool,
    /// The tab's focus generation when created: an upload dies with the focus it was made for.
    generation: u64,
}
/// Live transfers and the slots reserved for copies in progress; IDs are never reused.
#[derive(Default)]
struct Transfers {
    next: u64,
    live: HashMap<u64, Transfer>,
    /// One tab entry per copy that will hold a read transfer when it completes.
    reserved: Vec<u64>,
}
impl Transfers {
    fn room(&self, tab: u64) -> Result<(), String> {
        let here = self.live.values().filter(|t| t.tab == tab).count()
            + self.reserved.iter().filter(|t| **t == tab).count();
        if here >= PER_TAB || self.live.len() + self.reserved.len() >= PER_HOST {
            return Err("LIMIT: too many live clipboard transfers".into());
        }
        Ok(())
    }
    fn insert(&mut self, transfer: Transfer) -> u64 {
        self.next += 1;
        self.live.insert(self.next, transfer);
        self.next
    }
    fn create(
        &mut self,
        tab: u64,
        viewer: u64,
        format: &str,
        size: u64,
        generation: u64,
    ) -> Result<u64, String> {
        if format != FORMAT {
            return Err("UNSUPPORTED_FORMAT: not a listed clipboard format".into());
        }
        if size > TEXT as u64 {
            return Err("TOO_LARGE: a text representation is at most 1 MiB".into());
        }
        self.room(tab)?;
        Ok(self.insert(Transfer {
            tab,
            viewer,
            read: false,
            size: size as usize,
            data: Vec::new(),
            sealed: false,
            generation,
        }))
    }
    fn get(&mut self, tab: u64, viewer: u64, id: u64, read: bool) -> Result<&mut Transfer, String> {
        self.live
            .get_mut(&id)
            .filter(|t| t.tab == tab && t.viewer == viewer && t.read == read)
            .ok_or_else(|| "unknown clipboard transfer".into())
    }
    fn write(
        &mut self,
        tab: u64,
        viewer: u64,
        id: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<(), String> {
        let t = self.get(tab, viewer, id, false)?;
        if t.sealed {
            return Err("INVALID_DATA: the transfer is sealed".into());
        }
        if data.len() > CHUNK {
            return Err("TOO_LARGE: a WRITE carries at most 49152 bytes".into());
        }
        if offset != t.data.len() as u64 {
            return Err("INVALID_DATA: a WRITE must start where the last one ended".into());
        }
        if t.data.len() + data.len() > t.size {
            return Err("TOO_LARGE: the WRITE overflows the declared size".into());
        }
        t.data.extend_from_slice(data);
        Ok(())
    }
    fn seal(&mut self, tab: u64, viewer: u64, id: u64) -> Result<(), String> {
        let t = self.get(tab, viewer, id, false)?;
        if t.sealed {
            return Err("INVALID_DATA: the transfer is already sealed".into());
        }
        if t.data.len() != t.size {
            return Err("INVALID_DATA: fewer bytes written than declared".into());
        }
        if std::str::from_utf8(&t.data).is_err() {
            return Err("INVALID_DATA: text is not valid UTF-8".into());
        }
        t.sealed = true;
        Ok(())
    }
    fn release(&mut self, tab: u64, viewer: u64, id: u64) -> Result<(), String> {
        self.live
            .get(&id)
            .filter(|t| t.tab == tab && t.viewer == viewer)
            .ok_or("unknown clipboard transfer")?;
        self.live.remove(&id);
        Ok(())
    }
    /// Up to `length` bytes from `offset`, zero or above [`CHUNK`] meaning [`CHUNK`], and
    /// whether they end the representation.
    fn read(
        &mut self,
        tab: u64,
        viewer: u64,
        id: u64,
        offset: u64,
        length: u64,
    ) -> Result<(Vec<u8>, bool), String> {
        let t = self.get(tab, viewer, id, true)?;
        if offset >= t.data.len() as u64 {
            return Err("offset outside the representation".into());
        }
        let length = if length == 0 || length > CHUNK as u64 {
            CHUNK
        } else {
            length as usize
        };
        let start = offset as usize;
        let end = (start + length).min(t.data.len());
        Ok((t.data[start..end].to_vec(), end == t.data.len()))
    }
    fn reserve(&mut self, tab: u64) -> Result<(), String> {
        self.room(tab)?;
        self.reserved.push(tab);
        Ok(())
    }
    fn unreserve(&mut self, tab: u64) {
        if let Some(i) = self.reserved.iter().position(|t| *t == tab) {
            self.reserved.remove(i);
        }
    }
    /// A reserved slot becomes a read transfer holding copied text.
    fn copied(&mut self, tab: u64, viewer: u64, data: Vec<u8>) -> u64 {
        self.unreserve(tab);
        self.insert(Transfer {
            tab,
            viewer,
            read: true,
            size: data.len(),
            sealed: true,
            data,
            generation: 0,
        })
    }
    /// Checks a paste's uploads: one to eight sealed write transfers of this tab and viewer, at
    /// most one per format, made for the tab's current focus `generation`; an upload made for an
    /// earlier focus is retired, as if the change had been seen already. With `consume`, they
    /// are freed and their text returned.
    fn take(
        &mut self,
        tab: u64,
        viewer: u64,
        ids: &[u64],
        generation: u64,
        consume: bool,
    ) -> Result<Vec<u8>, String> {
        if ids.is_empty() {
            return Err("a paste needs a sealed transfer".into());
        }
        if ids.len() > REPRESENTATIONS {
            return Err("LIMIT: an operation carries at most 8 representations".into());
        }
        for id in ids {
            let t = self.get(tab, viewer, *id, false)?;
            if t.generation != generation {
                self.live.remove(id);
                return Err("unknown clipboard transfer".into());
            }
            if !t.sealed {
                return Err("INVALID_DATA: the transfer is not sealed".into());
            }
        }
        // One format is listed, so a second representation repeats it.
        if ids.len() > 1 {
            return Err("INVALID_DATA: one representation per format".into());
        }
        Ok(if consume {
            self.live
                .remove(&ids[0])
                .map(|t| t.data)
                .unwrap_or_default()
        } else {
            Vec::new()
        })
    }
    fn retire(&mut self, gone: impl Fn(&Transfer) -> bool) {
        self.live.retain(|_, t| !gone(t));
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Copy = 1,
    Cut = 2,
    Paste = 3,
    PastePlain = 4,
}
struct Op {
    /// The request ID; zero for a copy chosen from a context menu.
    id: u64,
    menu: u64,
    tab: u64,
    viewer: u64,
    kind: Kind,
    /// The browsing context whose queue it waits in.
    context: String,
    /// The upload a paste offers.
    text: Vec<u8>,
    /// For a copy: the client accepts text, so a slot is reserved for it.
    accept: bool,
    generation: u64,
}
impl Op {
    fn copies(&self) -> bool {
        matches!(self.kind, Kind::Copy | Kind::Cut)
    }
}
/// What a command's renderer and frame answered: started, ran, or refused unrun.
struct Ran {
    token: u64,
    stage: i64,
    selection: bool,
    editable: bool,
    copied: Copied,
}
#[derive(Default)]
struct Copied {
    text: String,
    /// The frame committed anything at all, in any format.
    nonempty: bool,
    too_large: bool,
}
struct Active {
    op: Op,
    /// The page's own command in its focused frame is still to run.
    to_run: bool,
    /// Its request, once sent, and its deadline.
    token: u64,
    /// The latest request the engine has had, which it may be holding on to.
    sent: u64,
    deadline: Instant,
    /// The frame the page step went to, and whether the page started running it.
    frame: String,
    started: bool,
    /// Until when a command its renderer refused only for arriving late is sent again.
    run_until: Option<Instant>,
    /// ClipboardFinished was already emitted, as CANCELLED.
    finished: bool,
    /// Cancelled before the engine said it had gone to its renderer, so it's asked to take it back.
    /// Until it answers, the command may be running, and holds its context's clipboard.
    withdrawing: bool,
    /// The page ran its command.
    delivered: bool,
    selection: bool,
    editable: bool,
    copied: Copied,
}
impl Active {
    fn new(op: Op) -> Self {
        Self {
            op,
            to_run: true,
            token: 0,
            sent: 0,
            deadline: Instant::now() + STEP,
            frame: String::new(),
            started: false,
            run_until: None,
            finished: false,
            withdrawing: false,
            delivered: false,
            selection: false,
            editable: false,
            copied: Copied::default(),
        }
    }
    /// Nothing more reaches the page, and the engine lets go of anything it holds for this.
    fn abandon(&mut self) {
        self.to_run = false;
        self.token = 0;
        if self.sent != 0 {
            post(json!({"op":"clipboard_end","context":self.op.context,"token":self.sent}));
        }
    }
    /// The page command has gone to its renderer, which has not answered whether it ran. Before
    /// the engine says so it may still be waiting behind earlier input, and can be taken back.
    fn running(&self) -> bool {
        self.token != 0 && self.to_run && !self.frame.is_empty()
    }
    /// Finishes the operation CANCELLED, once.
    fn cancel(&mut self) {
        if !self.finished {
            finished(&self.op, Status::Cancelled);
            self.finished = true;
        }
    }
}

fn emit(event: Event) {
    protocol::emit(control::Output {
        body: Some(control::output::Body::Event(control::Event {
            event: Some(event),
        })),
    });
}
fn finished(op: &Op, status: Status) {
    diagnostic(format_args!(
        "clipboard tab {} operation {} menu {} finished {status:?}",
        op.tab, op.id, op.menu
    ));
    emit(Event::ClipboardFinished(control::ClipboardFinished {
        tab: op.tab,
        viewer: op.viewer,
        operation: op.id,
        menu_request: op.menu,
        status: status as i32,
    }));
}
/// The viewer holds CONTROL of the tab and no JavaScript dialog of the tab is pending.
fn authorized(host: &Host, tab: u64, viewer: u64) -> Result<(), String> {
    let current = host
        .tabs
        .get(&tab)
        .map(|t| t.display.controller.load(Ordering::Acquire))
        .ok_or("unknown tab")?;
    if viewer == 0 || current != viewer {
        return Err("viewer does not hold CONTROL".into());
    }
    if host.dialogs.values().any(|owner| *owner == tab) {
        return Err("JavaScript dialog pending".into());
    }
    Ok(())
}
fn number(p: &Value, key: &str) -> u64 {
    p[key].as_u64().unwrap_or(0)
}

/// One browsing context's operations, which run one at a time.
#[derive(Default)]
struct Queue {
    waiting: VecDeque<Op>,
    active: Option<Active>,
}

#[derive(Default)]
pub struct Clipboard {
    enabled: bool,
    transfers: Transfers,
    queues: HashMap<String, Queue>,
    next_token: u64,
    ran: Vec<Ran>,
}
impl Clipboard {
    pub fn enable(&mut self) {
        self.enabled = true;
    }
    pub fn formats(&self) -> Vec<&'static str> {
        if self.enabled {
            vec![FORMAT]
        } else {
            Vec::new()
        }
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }
    fn check(&self, host: &Host, p: &Value) -> Result<(u64, u64), String> {
        if !self.enabled {
            return Err("clipboard capability not negotiated".into());
        }
        let (tab, viewer) = (number(p, "tab"), number(p, "viewer"));
        authorized(host, tab, viewer)?;
        Ok((tab, viewer))
    }
    /// ClipboardTransfer: CREATE, WRITE, SEAL or RELEASE.
    pub fn transfer(&mut self, host: &Host, p: &Value) -> Result<Value, String> {
        let (tab, viewer) = self.check(host, p)?;
        let id = number(p, "transfer");
        match number(p, "action") {
            1 => {
                let size = number(p, "size");
                let format = p["format"].as_str().unwrap_or("");
                let generation = host.ime.generation(tab);
                let id = self
                    .transfers
                    .create(tab, viewer, format, size, generation)?;
                diagnostic(format_args!(
                    "clipboard tab {tab} transfer {id} created, {size} bytes"
                ));
                Ok(json!({"transfer":id,"format":FORMAT,"size":size}))
            }
            2 => {
                let data: Vec<u8> = p["data"]
                    .as_array()
                    .map(|a| a.iter().map(|b| b.as_u64().unwrap_or(0) as u8).collect())
                    .unwrap_or_default();
                self.transfers
                    .write(tab, viewer, id, number(p, "offset"), &data)?;
                Ok(json!({}))
            }
            3 => {
                self.transfers.seal(tab, viewer, id)?;
                Ok(json!({}))
            }
            4 => {
                self.transfers.release(tab, viewer, id)?;
                Ok(json!({}))
            }
            _ => Err("unknown clipboard transfer action".into()),
        }
    }
    /// ReadClipboardTransfer.
    pub fn read(&mut self, host: &Host, p: &Value) -> Result<Value, String> {
        let (tab, viewer) = self.check(host, p)?;
        let (id, offset) = (number(p, "transfer"), number(p, "offset"));
        let (data, end) = self
            .transfers
            .read(tab, viewer, id, offset, number(p, "length"))?;
        Ok(json!({"transfer":id,"offset":offset,"data":data,"end":end}))
    }
    fn pending(&self) -> usize {
        self.queues
            .values()
            .map(|q| q.waiting.len() + usize::from(q.active.is_some()))
            .sum()
    }
    fn is_pending(&self, id: u64) -> bool {
        self.queues.values().any(|q| {
            q.waiting.iter().any(|op| op.id == id)
                || q.active
                    .as_ref()
                    .is_some_and(|a| !a.finished && a.op.id == id)
        })
    }
    /// ClipboardOperation, checked on receipt and, with `admit`, again when its RFB input has
    /// arrived; admission consumes its uploads and resolves its context menu.
    pub fn operation(
        &mut self,
        host: &mut Host,
        id: u64,
        p: &Value,
        admit: bool,
    ) -> Result<(), String> {
        let (tab, viewer) = self.check(host, p)?;
        if id == 0 {
            return Err("operation ID zero is reserved".into());
        }
        if self.is_pending(id) {
            return Err("operation ID already pending".into());
        }
        if self.pending() >= OPERATIONS {
            return Err("LIMIT: too many clipboard operations pending".into());
        }
        let kind = match number(p, "kind") {
            1 => Kind::Copy,
            2 => Kind::Cut,
            3 => Kind::Paste,
            4 => Kind::PastePlain,
            _ => return Err("unknown clipboard operation".into()),
        };
        let menu = number(p, "menu_request");
        let ids: Vec<u64> = p["transfers"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        let generation = host.ime.generation(tab);
        let mut op = Op {
            id,
            menu,
            tab,
            viewer,
            kind,
            context: context_of(host, tab),
            text: Vec::new(),
            accept: false,
            generation,
        };
        if op.copies() {
            if menu != 0 {
                return Err("menu Cut and Copy are chosen by ContextMenuReply".into());
            }
            if !ids.is_empty() {
                return Err("a copy carries no transfers".into());
            }
            op.accept = p["accept"]
                .as_array()
                .is_some_and(|a| a.iter().any(|f| f == FORMAT));
            if op.accept {
                // The copy's read transfer needs room when it completes.
                self.transfers.room(tab)?;
            }
        } else {
            if menu != 0 {
                let command = if kind == Kind::Paste { 114 } else { 115 };
                host.menus.allows(tab, menu, viewer, command)?;
            }
            self.transfers.take(tab, viewer, &ids, generation, false)?;
        }
        if !admit {
            return Ok(());
        }
        if op.copies() {
            if op.accept {
                self.transfers.reserve(tab)?;
            }
        } else {
            op.text = self.transfers.take(tab, viewer, &ids, generation, true)?;
            if menu != 0 {
                host.menus.resolve(tab, menu);
            }
        }
        diagnostic(format_args!(
            "clipboard tab {tab} operation {id} {kind:?} admitted"
        ));
        self.queues
            .entry(op.context.clone())
            .or_default()
            .waiting
            .push_back(op);
        Ok(())
    }
    /// ContextMenuReply choosing Cut or Copy: the same flow as COPY and CUT, as operation zero
    /// carrying the menu request.
    pub fn menu_copy(&mut self, host: &mut Host, p: &Value) -> Result<(), String> {
        let (tab, menu) = (number(p, "tab"), number(p, "request"));
        let command = p["command"].as_i64().unwrap_or(0) as i32;
        let viewer = host.menus.owner(tab, menu, command)?;
        authorized(host, tab, viewer)?;
        if self.pending() >= OPERATIONS {
            return Err("LIMIT: too many clipboard operations pending".into());
        }
        self.transfers.reserve(tab)?;
        host.menus.resolve(tab, menu);
        let op = Op {
            id: 0,
            menu,
            tab,
            viewer,
            kind: if command == 112 {
                Kind::Cut
            } else {
                Kind::Copy
            },
            context: context_of(host, tab),
            text: Vec::new(),
            accept: true,
            generation: host.ime.generation(tab),
        };
        self.queues
            .entry(op.context.clone())
            .or_default()
            .waiting
            .push_back(op);
        Ok(())
    }
    pub fn sent(&mut self, token: u64, frame: &str) {
        if let Some(a) = self
            .queues
            .values_mut()
            .filter_map(|q| q.active.as_mut())
            .find(|a| a.token == token)
        {
            a.frame = frame.to_owned();
        }
    }
    /// The engine's answer to `clipboard_withdraw`. If it had gone already, the engine said so first,
    /// and the operation holds on as one that's running.
    pub fn withdrawn(&mut self, token: u64, withdrawn: bool) {
        if let Some(a) = self
            .queues
            .values_mut()
            .filter_map(|q| q.active.as_mut())
            .find(|a| a.withdrawing && a.token == token)
        {
            a.withdrawing = false;
            if withdrawn {
                a.abandon();
            }
        }
    }
    pub fn ran(&mut self, p: &Value) {
        self.ran.push(Ran {
            token: number(p, "token"),
            stage: p["stage"].as_i64().unwrap_or(2),
            selection: p["selection"] == true,
            editable: p["editable"] == true,
            copied: Copied {
                text: p["text"].as_str().unwrap_or("").to_owned(),
                nonempty: p["nonempty"] == true,
                too_large: p["too_large"] == true,
            },
        });
    }
    /// Finishes matching operations CANCELLED. Unless `hard`, an operation whose page command
    /// is running or ran is left to complete. One whose command is running still owns its context's
    /// clipboard until its renderer answers or the command's deadline passes.
    fn cancel(&mut self, matches: impl Fn(&Op) -> bool, hard: bool) {
        for q in self.queues.values_mut() {
            let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut q.waiting)
                .into_iter()
                .partition(|op| matches(op));
            q.waiting = kept.into();
            for op in gone {
                if op.copies() && op.accept {
                    self.transfers.unreserve(op.tab);
                }
                finished(&op, Status::Cancelled);
            }
            if let Some(a) = q.active.as_mut()
                && !a.finished
                && matches(&a.op)
                && (hard || !(a.delivered || a.running()))
            {
                a.cancel();
                if a.token != 0 && a.to_run {
                    if !a.running() && !a.withdrawing {
                        a.withdrawing = true;
                        post(json!({"op":"clipboard_withdraw","tab":a.op.tab,"token":a.token}));
                    }
                } else {
                    a.abandon();
                }
            }
        }
    }
    /// CONTROL moved: the tab's operations finish CANCELLED and its transfers are freed.
    pub fn controller_changed(&mut self, tab: u64) {
        self.cancel(|op| op.tab == tab, true);
        self.transfers.retire(|t| t.tab == tab);
    }
    /// A viewer's connection closed: its operations and transfers go.
    pub fn disconnected(&mut self, tab: u64, viewer: u64) {
        self.cancel(|op| op.tab == tab && op.viewer == viewer, true);
        self.transfers
            .retire(|t| t.tab == tab && t.viewer == viewer);
    }
    pub fn closed_tab(&mut self, tab: u64) {
        self.controller_changed(tab);
        self.page_gone(tab, None);
    }
    /// A frame of the tab went away (None: all of them, as when its renderer terminated): a
    /// command started there will never answer, so it no longer holds the clipboard.
    pub fn page_gone(&mut self, tab: u64, frame: Option<&str>) {
        for a in self.queues.values_mut().filter_map(|q| q.active.as_mut()) {
            // With all of them gone, it can't be running anywhere, whatever the engine last said.
            let gone = match frame {
                Some(frame) => a.running() && frame == a.frame,
                None => a.token != 0 && a.to_run,
            };
            if a.op.tab == tab && gone {
                a.cancel();
                a.abandon();
            }
        }
    }
    /// EOF: every operation finishes CANCELLED.
    pub fn shutdown(&mut self) {
        self.cancel(|_| true, true);
        self.transfers.retire(|_| true);
    }
    pub fn tick(&mut self, host: &mut Host) {
        if !self.enabled {
            return;
        }
        let mut ran = std::mem::take(&mut self.ran);
        // Answers first: a command that ran completes even if it moved focus itself.
        self.advance_all(host, &mut ran);
        // A focus or document change retires uploads and the operations not yet run.
        let ime = &host.ime;
        self.transfers
            .retire(|t| !t.read && t.generation != ime.generation(t.tab));
        for a in self.queues.values().filter_map(|q| q.active.as_ref()) {
            if !a.finished && !a.delivered && a.op.generation != ime.generation(a.op.tab) {
                diagnostic(format_args!(
                    "clipboard tab {} operation {} focus moved: generation {} from {}",
                    a.op.tab,
                    a.op.id,
                    ime.generation(a.op.tab),
                    a.op.generation
                ));
            }
        }
        self.cancel(|op| op.generation != ime.generation(op.tab), false);
        self.advance_all(host, &mut ran);
        self.queues
            .retain(|_, q| !q.waiting.is_empty() || q.active.is_some());
    }
    fn advance_all(&mut self, host: &mut Host, ran: &mut Vec<Ran>) {
        let contexts: Vec<String> = self.queues.keys().cloned().collect();
        for context in contexts {
            while self.advance(host, &context, ran) {}
        }
    }
    /// Moves the context's active operation one step; false when it waits.
    fn advance(&mut self, host: &mut Host, context: &str, ran: &mut Vec<Ran>) -> bool {
        let token = self.next_token + 1;
        let Some(queue) = self.queues.get_mut(context) else {
            return false;
        };
        let Some(a) = queue.active.as_mut() else {
            return self.activate(host, context);
        };
        if !a.to_run {
            let a = queue.active.take().unwrap();
            self.complete(a);
            return true;
        }
        if a.token == 0 {
            self.next_token = token;
            if run(host, a, token) {
                a.token = token;
                a.sent = token;
            } else {
                // The page can no longer be reached as the operation requires.
                diagnostic(format_args!(
                    "clipboard tab {} operation {} unreachable: generation {} from {}",
                    a.op.tab,
                    a.op.id,
                    host.ime.generation(a.op.tab),
                    a.op.generation
                ));
                a.cancel();
                a.abandon();
            }
            return true;
        }
        if let Some(i) = ran.iter().position(|r| r.token == a.token) {
            let r = ran.remove(i);
            match r.stage {
                // Started before its deadline: it runs to the end whatever happens.
                0 => a.started = true,
                // A busy renderer reached it too late: send it again with a fresh deadline while
                // the operation still holds; `run` checks again.
                3 if !a.finished && a.run_until.is_some_and(|t| Instant::now() < t) => {
                    a.token = 0;
                }
                1 if !a.finished => {
                    a.selection = r.selection;
                    a.editable = r.editable;
                    a.copied = r.copied;
                    a.delivered = true;
                    a.to_run = false;
                    a.token = 0;
                }
                // Refused unrun (too late, or focus moved), or ran after cancellation.
                _ => {
                    diagnostic(format_args!(
                        "clipboard tab {} operation {} refused at stage {}",
                        a.op.tab, a.op.id, r.stage
                    ));
                    a.cancel();
                    a.abandon();
                }
            }
            return true;
        }
        // A command its renderer started holds the clipboard until it ends, whatever it takes:
        // only its answer, or its frame or renderer going away, releases it.
        if Instant::now() < a.deadline || (a.running() && a.started) {
            return false;
        }
        diagnostic("clipboard step timed out");
        a.cancel();
        a.abandon();
        true
    }
    /// Starts the context's next operation.
    fn activate(&mut self, host: &Host, context: &str) -> bool {
        let Some(queue) = self.queues.get_mut(context) else {
            return false;
        };
        let Some(op) = queue.waiting.pop_front() else {
            return false;
        };
        if authorized(host, op.tab, op.viewer).is_err() {
            if op.copies() && op.accept {
                self.transfers.unreserve(op.tab);
            }
            finished(&op, Status::Cancelled);
            return true;
        }
        if op.copies() && !op.accept {
            finished(&op, Status::UnsupportedFormat);
            return true;
        }
        // Nothing is offered to a frame without a focused editor.
        if !op.copies() && host.ime.editor_frame(op.tab).is_none() {
            finished(&op, Status::NotEditable);
            return true;
        }
        queue.active = Some(Active::new(op));
        true
    }
    /// Reports the outcome.
    fn complete(&mut self, a: Active) {
        let op = a.op;
        let reserved = op.copies() && op.accept;
        if a.finished {
            if reserved {
                self.transfers.unreserve(op.tab);
            }
            return;
        }
        let status = if op.copies() {
            if a.copied.too_large {
                Status::TooLarge
            } else if !a.copied.text.is_empty() {
                let size = a.copied.text.len();
                let id = self
                    .transfers
                    .copied(op.tab, op.viewer, a.copied.text.into_bytes());
                emit(Event::ClipboardCopied(control::ClipboardCopied {
                    tab: op.tab,
                    viewer: op.viewer,
                    operation: op.id,
                    menu_request: op.menu,
                    representations: vec![control::ClipboardRepresentation {
                        transfer: id,
                        format: FORMAT.into(),
                        size: size as u64,
                    }],
                }));
                Status::Done
            } else if a.copied.nonempty {
                Status::UnsupportedFormat
            } else if a.selection {
                // The page had a selection, yet nothing was copied: it cancelled the event.
                Status::Denied
            } else {
                Status::NothingSelected
            }
        } else if a.editable {
            Status::Done
        } else {
            Status::NotEditable
        };
        if reserved && status != Status::Done {
            self.transfers.unreserve(op.tab);
        }
        finished(&op, status);
    }
}

fn context_of(host: &Host, tab: u64) -> String {
    host.tabs
        .get(&tab)
        .map_or_else(String::new, |t| t.context.clone())
}
/// Sends the page step to the tab's focused frame, checked again just before. The renderer runs it
/// only before its deadline and while the frame's focus is the one reported.
fn run(host: &Host, a: &mut Active, token: u64) -> bool {
    let op = &a.op;
    // A focus or document change since admission: the operation no longer applies.
    if a.finished
        || authorized(host, op.tab, op.viewer).is_err()
        || host.ime.generation(op.tab) != op.generation
    {
        return false;
    }
    let (frame, counter) = host.ime.focus_counter(op.tab).unwrap_or_default();
    let text = std::str::from_utf8(&op.text).unwrap_or("");
    a.frame.clear();
    post(
        json!({"op":"clipboard_run","tab":op.tab,"token":token,"kind":op.kind as i32,"frame":frame,"counter":counter,"text":text}),
    );
    // Unstarted by then, it never runs, so ownership may wait as long as the operation is
    // still willing to send it again: a busy renderer's refusal arrives late.
    let until = *a.run_until.get_or_insert(Instant::now() + STEP);
    a.deadline = until.max(Instant::now() + RUN) + RUN_MARGIN;
    a.started = false;
    true
}
