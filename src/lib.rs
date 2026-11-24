//! The Rust host: speaks the control protocol and RFB, and drives the Chromium executable that loaded
//! it (engine/lanescape_browser.h).
#[allow(dead_code)]
mod cdp;
mod clipboard;
mod devtools;
mod ime;
mod input;
mod menu;
#[allow(dead_code)]
mod ordering;
#[allow(dead_code)]
mod protocol;
mod recording;
mod rfb;
mod sandbox;
mod saved;

use input::{Input, character, virtual_key};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::{c_char, c_int},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::Ordering,
        mpsc::{self, RecvTimeoutError, SyncSender},
    },
    time::{Duration, Instant},
};

#[repr(C)]
pub struct LanescapeBrowserFrame {
    tab: u64,
    width: u32,
    height: u32,
    stride: u32,
    pixels: *const u8,
    update_x: u32,
    update_y: u32,
    update_width: u32,
    update_height: u32,
    sequence_gap: u8,
    byte_order: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LanescapeBrowserEngine {
    post: extern "C" fn(*const c_char, usize),
    devtools_send: extern "C" fn(u64, u64, u8, *const u8, usize),
    now_us: extern "C" fn() -> i64,
}

#[repr(C)]
pub struct LanescapeBrowserHost {
    on_started: extern "C" fn(),
    on_event: extern "C" fn(*const c_char, usize),
    on_frame: extern "C" fn(*const LanescapeBrowserFrame),
    on_devtools: extern "C" fn(u64, *const u8, usize),
    on_recording: extern "C" fn(u64, *const u8, usize, i64, u8, u32, u32),
    on_exit: extern "C" fn(c_int) -> c_int,
}

const FENCE_TOKEN: &str = "display-fence";
const LOADING_TOKEN: &str = "loading-state";
/// Fences outstanding across the host.
const FENCE_LIMIT: usize = 64;
/// How long an IME request waits for its renderer, past the 2 seconds it has to start.
const IME_BOUND: Duration = Duration::from_secs(5);
/// A fence unanswered for this long fails, so a client never waits forever on a busy renderer.
const FENCE_BOUND: Duration = Duration::from_secs(5);
/// How long Status waits for Chromium to say what compositing runs.
const GPU_STATUS_BOUND: Duration = Duration::from_secs(5);
const CREATE_BOUND: Duration = Duration::from_secs(15);
/// When each create_tab still unanswered was posted, by request; `on_event` clears a request as
/// Chromium answers it.
static CREATING: Mutex<std::collections::BTreeMap<i64, Instant>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A tab Chromium hasn't created within 15 seconds means it is stuck, and only a new host helps:
/// this one ends at once, as a crash, so the next says so and the supervisor can discard saved
/// tabs that keep doing this (specs/0002-control.md). It watches for the life of the process,
/// shutdown included, and decides under the lock that answers take, so an answer in time always
/// counts.
fn watch_creating() {
    std::thread::spawn(|| {
        loop {
            {
                let creating = CREATING.lock().unwrap();
                if creating
                    .values()
                    .any(|since| since.elapsed() > CREATE_BOUND)
                {
                    eprintln!(
                        "lanescape-browser: Chromium didn't create a tab within 15 seconds; stopping"
                    );
                    // SAFETY: ends the process, the lock still held, without running anything
                    // that might wait on Chromium.
                    unsafe { libc::_exit(1) };
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

static ENGINE: OnceLock<LanescapeBrowserEngine> = OnceLock::new();
static COMMANDS: OnceLock<SyncSender<Value>> = OnceLock::new();
static CHANNEL: Mutex<Option<mpsc::Receiver<Value>>> = Mutex::new(None);
static STARTUP: OnceLock<Startup> = OnceLock::new();
static EXIT: Mutex<Option<String>> = Mutex::new(None);
static DISPLAYS: OnceLock<Mutex<HashMap<u64, Slot>>> = OnceLock::new();

struct Startup {
    root: PathBuf,
    runtime: PathBuf,
    cpu: bool,
    /// The control loop takes the saved tabs; there is one copy of them.
    saved: Mutex<saved::Saved>,
}

/// A tab's display. Chromium can deliver a frame before the host has opened its socket, and an
/// unchanged page delivers no other, so the latest frame waits here until then.
#[derive(Default)]
struct Slot {
    display: Option<rfb::Shared>,
    pending: Option<rfb::Frame>,
}

fn displays() -> &'static Mutex<HashMap<u64, Slot>> {
    DISPLAYS.get_or_init(Default::default)
}

pub(crate) fn event(v: Value) {
    protocol::write(v)
}

fn diagnostic(message: impl std::fmt::Display) {
    if std::env::var_os("LANESCAPE_BROWSER_DIAGNOSTICS").is_some() {
        eprintln!("lanescape-browser[{}]: {message}", std::process::id());
    }
}

fn devtools_send(session: u64, id: u64, is_input: bool, message: &[u8]) {
    let engine = ENGINE
        .get()
        .expect("the engine set this before Chromium started");
    (engine.devtools_send)(
        session,
        id,
        u8::from(is_input),
        message.as_ptr(),
        message.len(),
    );
}

/// Chromium's clock, in microseconds, for a deadline that starts before a command reaches it.
pub(crate) fn engine_now_us() -> i64 {
    (ENGINE
        .get()
        .expect("the engine set this before Chromium started")
        .now_us)()
}

/// Sends a command to Chromium's UI thread.
pub(crate) fn post(command: Value) {
    let json = command.to_string();
    let engine = ENGINE
        .get()
        .expect("the engine set this before Chromium started");
    (engine.post)(json.as_ptr().cast(), json.len());
}

struct Tab {
    context: String,
    display: rfb::Shared,
    input: Input,
}

struct PendingCreate {
    id: Option<Value>,
    context: String,
    /// A saved tab coming back, which the first Initialize waits for.
    restore: bool,
}

struct PendingFence {
    id: Value,
    tab: u64,
    deadline: Instant,
}

struct Host {
    tx: SyncSender<Value>,
    runtime: PathBuf,
    root: PathBuf,
    cpu: bool,
    capabilities: Vec<&'static str>,
    tabs: HashMap<u64, Tab>,
    creating: HashMap<i64, PendingCreate>,
    /// Status requests waiting for Chromium's answer, and how long they wait.
    gpu_status: HashMap<i64, (Option<Value>, Instant)>,
    /// Profile releases under way: who awaits each, and the context it concerns.
    releasing: HashMap<i64, (Vec<Option<Value>>, String)>,
    /// CreateTab requests for a context being released, run once it is.
    after_release: Vec<(Option<Value>, Value)>,
    fences: HashMap<i64, PendingFence>,
    /// Dialogs by request ID, with their tab.
    dialogs: HashMap<u64, u64>,
    next_request: i64,
    previous_exit: i32,
    /// The tabs a previous host saved, until the first Initialize restores or discards them.
    saved: Option<Vec<Value>>,
    /// The `next-tab` last written (`saved::reserve`).
    reserved: u64,
    /// Saved tabs still to restore, the one being restored, and the Initialize requests waiting
    /// for them.
    to_restore: std::collections::VecDeque<Value>,
    restoring: usize,
    initializing: Vec<Option<Value>>,
    /// Profiles of saved tabs this host didn't restore, which must outlast their other tabs.
    deferred: std::collections::HashSet<String>,
    sandbox_checked: bool,
    stopping: bool,
    /// The request being handled, whole: a held one carries its stage with it.
    current_request: Option<Value>,
    devtools: devtools::DevTools,
    ime: ime::Ime,
    clipboard: clipboard::Clipboard,
    menus: menu::Menus,
    /// ImeInput requests on their way to the page, which has the last word on them.
    ime_applying: HashMap<i64, (Option<Value>, u64, Instant)>,
    recordings: recording::Recordings,
    /// Requests waiting for earlier RFB input or an editor check, and those released, which run
    /// before anything else queued. RFB input of a tab with a request checking waits in `blocked`,
    /// bounded, and then runs in order before newer input.
    held: ordering::Held,
    ready: std::collections::VecDeque<Value>,
    blocked: ordering::Blocked,
}

fn identifier(v: &Value) -> Result<String, String> {
    let s = v.as_str().ok_or("expected identifier")?;
    if s.is_empty()
        || s.len() > 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("invalid identifier".into());
    }
    Ok(s.into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A named profile's directory.
fn profile_name(profile: &str) -> String {
    format!("profile-{}", hex(profile.as_bytes()))
}

/// The profile a context belongs to, as the wire names it: a named profile's name, or `~` and the
/// unnamed profile's 32 hex digits.
fn profile_of(context: &str) -> String {
    match context.strip_prefix("unnamed-") {
        Some(id) => format!("~{id}"),
        None => context.strip_prefix("profile-").unwrap_or(context).into(),
    }
}

/// Removes an unnamed profile's directory once Chromium has let it go, on one thread of its own so
/// neither the control loop nor a burst of closes waits on the disk. Chromium's network service
/// can still be finishing writes there, so each is removed again a few seconds later; whatever a
/// stopping host leaves, the next host to start removes (`saved::open`).
fn remove_profile_directory(path: PathBuf) {
    static CLEANUP: OnceLock<Option<mpsc::Sender<PathBuf>>> = OnceLock::new();
    let tx = CLEANUP.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let cleanup = move || {
            let mut again: Vec<(Instant, PathBuf)> = Vec::new();
            loop {
                let now = Instant::now();
                again.retain(|(when, path)| {
                    if *when > now {
                        return true;
                    }
                    let _ = std::fs::remove_dir_all(path);
                    false
                });
                let wait = again
                    .iter()
                    .map(|(when, _)| when.saturating_duration_since(now))
                    .min()
                    .unwrap_or(Duration::from_secs(3600));
                match rx.recv_timeout(wait) {
                    Ok(path) => {
                        let _ = std::fs::remove_dir_all(&path);
                        again.push((Instant::now() + Duration::from_secs(5), path));
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        };
        std::thread::Builder::new()
            .name("profile-cleanup".into())
            .spawn(cleanup)
            .ok()
            .map(|_| tx)
    });
    if let Some(tx) = tx {
        let _ = tx.send(path);
    }
}

/// A new unnamed profile's engine context key, which is also its directory's name.
fn unnamed_context() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| e.to_string())?;
    Ok(format!("unnamed-{}", hex(&bytes)))
}

fn dimensions(p: &Value) -> Result<(u16, u16), String> {
    let w = p["width"].as_u64().unwrap_or(1280);
    let h = p["height"].as_u64().unwrap_or(800);
    if w == 0 || h == 0 || w > 8192 || h > 8192 || w * h > 16777216 {
        return Err("invalid viewport".into());
    }
    Ok((w as u16, h as u16))
}

/// Whether Chromium composites in hardware: software renderers are never reported as hardware
/// acceleration.
fn hardware_compositing(gpu: &Value) -> bool {
    let text = gpu.to_string().to_lowercase();
    gpu["featureStatus"]["gpu_compositing"] == "enabled"
        && ![
            "swiftshader",
            "llvmpipe",
            "software only",
            "software renderer",
            "softpipe",
            "lavapipe",
        ]
        .iter()
        .any(|s| text.contains(s))
}

/// RFB input acts only if its viewer held CONTROL when the RFB thread received it and still does,
/// so input sent while view-only cannot act once a later CONTROL grant is processed.
fn input_authorized(p: &Value, controller: u64) -> bool {
    let viewer = p["viewer"].as_u64().unwrap_or(0);
    viewer != 0 && viewer == controller && p["authorized"] == true
}

impl Host {
    fn respond(id: Option<Value>, result: Result<Value, String>) {
        if let Some(id) = id {
            event(match result {
                Ok(v) => json!({"id":id,"result":v}),
                Err(e) => json!({"id":id,"error":{"message":e}}),
            });
        }
    }

    fn next_request(&mut self) -> i64 {
        self.next_request += 1;
        self.next_request
    }

    /// Opens a tab's RFB socket and hands it any frame that already arrived.
    fn open_display(&self, tab: u64) -> Result<(rfb::Shared, PathBuf), String> {
        let socket = self.runtime.join(format!("{tab}.sock"));
        let display =
            rfb::listen(socket.clone(), tab, self.tx.clone()).map_err(|e| e.to_string())?;
        let mut slots = displays().lock().unwrap();
        let slot = slots.entry(tab).or_default();
        if let Some(frame) = slot.pending.take() {
            *display.frame.lock().unwrap() = Some(frame);
        }
        slot.display = Some(display.clone());
        Ok((display, socket))
    }

    fn release_input(tab_id: u64, t: &mut Tab) {
        for (_, vk) in t.input.keys.drain() {
            post(json!({"op":"key","tab":tab_id,"down":false,"vk":vk,"character":0,"modifiers":0}));
        }
        for mask in [1, 2, 4] {
            if t.input.buttons & mask != 0 {
                post(
                    json!({"op":"mouse","tab":tab_id,"kind":"up","x":t.input.pointer.0,"y":t.input.pointer.1,"button":mask,"modifiers":0,"clicks":1}),
                );
            }
        }
        t.input.buttons = 0;
        t.input.clicks = Default::default();
    }

    fn tab_closed(&mut self, tab: u64) {
        // The engine releases an unnamed profile itself when this was its last tab.
        let admitted = self.tabs.remove(&tab).inspect(|t| {
            t.display.closed.store(true, Ordering::Release);
        });
        self.devtools.closed_tab(tab);
        let enabled = self.capabilities.contains(&ime::TOKEN);
        self.ime.closed_tab(tab, enabled);
        let applying: Vec<_> = self
            .ime_applying
            .iter()
            .filter(|(_, (_, owner, _))| *owner == tab)
            .map(|(token, _)| *token)
            .collect();
        for token in applying {
            let (id, _, _) = self.ime_applying.remove(&token).unwrap();
            Self::respond(id, Err("tab closed".into()));
        }
        self.clipboard.closed_tab(tab);
        self.recordings.closed_tab(tab);
        self.menus.cancel_tab(tab);
        refuse(self.held.closed(tab), "tab closed");
        self.blocked.closed(tab);
        displays().lock().unwrap().remove(&tab);
        self.dialogs.retain(|_, owner| *owner != tab);
        let failed: Vec<_> = self
            .fences
            .iter()
            .filter(|(_, f)| f.tab == tab)
            .map(|(token, _)| *token)
            .collect();
        for token in failed {
            let fence = self.fences.remove(&token).unwrap();
            Self::respond(Some(fence.id), Err("tab closed".into()));
        }
        if admitted.is_some() {
            event(json!({"method":"tab.closed","params":{"tab":tab}}));
        }
    }

    fn engine_event(&mut self, name: &str, p: &Value) {
        let tab = p["tab"].as_u64().unwrap_or(0);
        // A tab the host refused, as one whose ID it couldn't record, never existed for anyone.
        if matches!(
            name,
            "title" | "navigation" | "loaded" | "loading_state" | "dialog"
        ) && !self.tabs.contains_key(&tab)
        {
            return;
        }
        match name {
            "tab_created" => {
                let request = p["request"].as_i64().unwrap_or(0);
                let Some(pending) = self.creating.remove(&request) else {
                    return;
                };
                // Recorded before anyone hears of it, so no later host hands its ID out again.
                let opened = saved::reserve(&self.root, tab, &mut self.reserved)
                    .map_err(|e| format!("can't record tab IDs: {e}"))
                    .and_then(|()| self.open_display(tab));
                match opened {
                    Ok((display, socket)) => {
                        let profile = profile_of(&pending.context);
                        post(json!({"op":"admit","tab":tab}));
                        self.tabs.insert(
                            tab,
                            Tab {
                                context: pending.context,
                                display,
                                input: Input::default(),
                            },
                        );
                        Self::respond(
                            pending.id,
                            Ok(json!({"tab":tab,"profile":profile,"display":socket})),
                        );
                    }
                    Err(e) => {
                        // A saved tab that can't come back now stays saved, with its profile: the
                        // engine keeps a restored tab it was never told to admit.
                        post(json!({"op":"close","tab":tab}));
                        if pending.restore {
                            self.deferred.insert(pending.context);
                        }
                        Self::respond(pending.id, Err(e));
                    }
                }
                if pending.restore {
                    self.restored();
                }
            }
            "tab_refused" => {
                let request = p["request"].as_i64().unwrap_or(0);
                if let Some(pending) = self.creating.remove(&request) {
                    if pending.restore {
                        // It stays saved, for the next host to start.
                        self.deferred.insert(pending.context);
                        self.restored();
                    } else if pending.context.starts_with("unnamed-") {
                        // The engine refused before making the context, so its directory is all
                        // there is.
                        remove_profile_directory(self.root.join(&pending.context));
                    }
                    Self::respond(pending.id, Err("tab limit reached".into()));
                }
            }
            "popup" => {
                let opener = p["opener"].as_u64().unwrap_or(0);
                let context = p["context"].as_str().unwrap_or("").to_string();
                let profile = profile_of(&context);
                // A refused opener's popup would name it.
                let admitted = if self.tabs.contains_key(&opener) {
                    Ok(())
                } else {
                    Err("its opener was refused".to_string())
                };
                let opened = admitted
                    .and_then(|()| {
                        saved::reserve(&self.root, tab, &mut self.reserved)
                            .map_err(|e| e.to_string())
                    })
                    .and_then(|()| self.open_display(tab));
                let Ok((display, socket)) = opened else {
                    post(json!({"op":"close","tab":tab}));
                    return;
                };
                post(json!({"op":"admit","tab":tab}));
                self.tabs.insert(
                    tab,
                    Tab {
                        context,
                        display,
                        input: Input::default(),
                    },
                );
                event(
                    json!({"method":"tab.popup","params":{"tab":tab,"opener":opener,"profile":profile,"display":socket}}),
                );
            }
            "closed" => self.tab_closed(tab),
            "devtools_refused" => {
                devtools::refused_by_engine(
                    p["session"].as_u64().unwrap_or(0),
                    p["id"].as_u64().unwrap_or(0),
                );
            }
            "devtools_detached" => {
                self.devtools.detached(p["session"].as_u64().unwrap_or(0));
            }
            // Swept once the tabs closed so far are forgotten on disk (`restore_over`).
            "context_loaded" => {
                let order = json!({"op":"scavenge_context","context":p["context"],"generation":p["generation"]});
                saved::after_forgets(move || post(order));
            }
            "context_released" => {
                let request = p["request"].as_i64().unwrap_or(0);
                // Its last tab has closed and Chromium has let go of its storage.
                if let Some(context) = p["context"].as_str()
                    && context.starts_with("unnamed-")
                    && p["released"] == true
                    && !self.profile_in_use(context)
                {
                    let path = self.root.join(context);
                    saved::after_forgets(move || remove_profile_directory(path));
                }
                if let Some((waiting, _)) = self.releasing.remove(&request) {
                    for id in waiting {
                        Self::respond(
                            id,
                            if p["released"] == true {
                                Ok(json!({}))
                            } else {
                                Err("close profile tabs before release".into())
                            },
                        );
                    }
                    // Requests that arrived during a ReleaseProfile, in order. An unnamed
                    // profile's release (request zero) held none of them up.
                    for (id, params) in std::mem::take(&mut self.after_release) {
                        if let Some(result) = self.create_tab(&id, &params) {
                            Self::respond(id, result);
                        }
                    }
                }
            }
            "title" => {
                event(json!({"method":"tab.title","params":{"tab":tab,"title":p["title"]}}));
            }
            "navigation" => {
                event(json!({"method":"tab.navigation","params":{"tab":tab,"url":p["url"]}}));
            }
            "loaded" => {
                if !self.sandbox_checked {
                    post(json!({"op":"sandbox","tab":tab}));
                }
                event(
                    json!({"method":"tab.loaded","params":{"tab":tab,"url":p["url"],"http_status":p["http_status"]}}),
                );
            }
            "loading_state" => {
                if self.capabilities.contains(&LOADING_TOKEN) {
                    event(
                        json!({"method":"tab.loading_state","params":{"tab":tab,"is_loading":p["is_loading"],"can_go_back":p["can_go_back"],"can_go_forward":p["can_go_forward"]}}),
                    );
                }
            }
            "dialog" => {
                let request = p["request"].as_u64().unwrap_or(0);
                self.dialogs.insert(request, tab);
                event(
                    json!({"method":"dialog","params":{"tab":tab,"request":request,"kind":p["kind"],"message":p["message"],"default_text":p["default_text"]}}),
                );
            }
            "dialog_closed" => {
                self.dialogs.remove(&p["request"].as_u64().unwrap_or(0));
            }
            "fence_ready" => {
                let token = p["token"].as_i64().unwrap_or(0);
                if let Some(fence) = self.fences.remove(&token) {
                    let sequence = self
                        .tabs
                        .get(&tab)
                        .and_then(|t| t.display.frame.lock().unwrap().as_ref().map(|f| f.sequence));
                    match sequence {
                        Some(sequence) => Self::respond(
                            Some(fence.id),
                            Ok(json!({"tab":tab,"sequence":sequence})),
                        ),
                        None => Self::respond(Some(fence.id), Err("display fence failed".into())),
                    }
                }
            }
            "fence_failed" => {
                let token = p["token"].as_i64().unwrap_or(0);
                if let Some(fence) = self.fences.remove(&token) {
                    let reason = p["reason"].as_str().unwrap_or("display fence failed");
                    Self::respond(Some(fence.id), Err(reason.into()));
                }
            }
            "gpu_status" => {
                let request = p["request"].as_i64().unwrap_or(0);
                if let Some((id, _)) = self.gpu_status.remove(&request) {
                    let gpu = &p["gpu"];
                    let accelerated = hardware_compositing(gpu);
                    let reason = if accelerated {
                        "hardware compositing".to_owned()
                    } else {
                        gpu["featureStatus"].to_string()
                    };
                    Self::respond(id, Ok(self.status(accelerated, &reason)));
                }
            }
            "sandbox" => self.verify_sandbox(p),
            "focus_report" => self.ime.report(p),
            "frame_focused" => self
                .ime
                .frame_focused(tab, p["frame"].as_str().unwrap_or("")),
            "frame_gone" => {
                let frame = p["frame"].as_str();
                self.ime.frame_gone(tab, frame);
                self.clipboard.page_gone(tab, frame);
                if frame.is_none() {
                    self.menus.cancel_tab(tab);
                }
            }
            "clipboard_sent" => self.clipboard.sent(
                p["token"].as_u64().unwrap_or(0),
                p["frame"].as_str().unwrap_or(""),
            ),
            "clipboard_ran" => self.clipboard.ran(p),
            "clipboard_withdrawn" => self
                .clipboard
                .withdrawn(p["token"].as_u64().unwrap_or(0), p["withdrawn"] == true),
            "context_menu" => {
                let viewer = self
                    .tabs
                    .get(&tab)
                    .map_or(0, |t| t.display.controller.load(Ordering::Acquire));
                let clipboard = self.clipboard.enabled();
                self.menus.opened(tab, viewer, p, clipboard);
            }
            "composition_layout" => self.ime.layout(tab, p),
            "ime_applied" => {
                let token = p["token"].as_i64().unwrap_or(0);
                if p["observed"] == true {
                    self.ime.layout(tab, p);
                }
                if let Some((id, tab, _)) = self.ime_applying.remove(&token) {
                    Self::respond(
                        id,
                        if p["ok"] == true {
                            Ok(json!({}))
                        } else {
                            Err("not the current editor".into())
                        },
                    );
                    self.held.finished(tab);
                }
            }
            _ => diagnostic(format_args!("unknown engine event {name}")),
        }
    }

    /// Never runs a page's code unsandboxed for longer than it takes to find out.
    fn verify_sandbox(&mut self, p: &Value) {
        // A renderer already gone can't answer; the next page's load asks again.
        let Some(checked) = sandbox::check(p, &self.root) else {
            return;
        };
        if let Err(why) = checked {
            eprintln!("lanescape-browser: the renderer is not sandboxed ({why}); stopping");
            if let Some(startup) = STARTUP.get() {
                let _ = std::fs::remove_dir_all(&startup.runtime);
            }
            std::process::exit(1);
        }
        self.sandbox_checked = true;
        diagnostic(format_args!("renderer sandbox verified, {p}"));
    }

    fn pointer(tab: u64, t: &mut Tab, p: &Value) {
        let x = p["x"].as_i64().unwrap_or(0) as i32;
        let y = p["y"].as_i64().unwrap_or(0) as i32;
        t.input.pointer = (x, y);
        post(
            json!({"op":"mouse","tab":tab,"kind":"move","x":x,"y":y,"modifiers":t.input.modifiers()}),
        );
        let buttons = p["buttons"].as_u64().unwrap_or(0) as u8;
        for mask in [1u8, 2, 4] {
            if (buttons ^ t.input.buttons) & mask != 0 {
                let up = buttons & mask == 0;
                if !up {
                    t.input.clicks.press(mask, (x, y), Instant::now());
                }
                post(
                    json!({"op":"mouse","tab":tab,"kind":if up {"up"} else {"down"},"x":x,"y":y,"button":mask,"modifiers":t.input.modifiers(),"clicks":t.input.clicks.count}),
                );
            }
        }
        for (mask, dx, dy) in [(16, 0, -120), (8, 0, 120), (32, 120, 0), (64, -120, 0)] {
            if buttons & mask != 0 {
                post(
                    json!({"op":"wheel","tab":tab,"x":x,"y":y,"dx":dx,"dy":dy,"modifiers":t.input.modifiers()}),
                );
            }
        }
        t.input.buttons = buttons;
    }

    fn key(tab: u64, t: &mut Tab, p: &Value) {
        let raw = p["key"].as_u64().unwrap_or(0);
        let code = virtual_key(raw);
        let down = p["down"] == true;
        if t.input.is_paste(raw, down) {
            return;
        }
        // ISO_Left_Tab is Shift+Tab, whether or not Shift was sent too.
        let implied = if raw == 0xfe20 { 2 } else { 0 };
        let ch = character(raw).map_or(0, |c| c as u32);
        if code != 0 {
            let slot = Input::slot(raw, code);
            if down {
                t.input.keys.insert(slot, code);
            } else {
                t.input.keys.remove(&slot);
            }
            post(
                json!({"op":"key","tab":tab,"down":down,"vk":code,"character":ch,"modifiers":t.input.modifiers() | implied}),
            );
        } else if down && ch != 0 {
            // No key behind it, as for most Unicode keysyms: it can only be typed.
            let mut units = [0; 2];
            for unit in char::from_u32(ch)
                .unwrap_or('\u{fffd}')
                .encode_utf16(&mut units)
            {
                post(
                    json!({"op":"key","tab":tab,"down":true,"vk":0,"character":*unit,"modifiers":t.input.modifiers() | implied}),
                );
            }
        }
    }

    fn status(&self, accelerated: bool, reason: &str) -> Value {
        json!({"gpu_accelerated":accelerated,"backend":if accelerated {"gpu"} else {"cpu"},"fallback_reason":reason,"capabilities":self.capabilities,"clipboard_formats":self.clipboard.formats(),"previous_exit":self.previous_exit})
    }

    /// Answers Initialize or Status with what compositing actually runs, which only Chromium knows.
    fn status_request(&mut self, id: &Option<Value>) -> Option<Result<Value, String>> {
        if self.cpu {
            return Some(Ok(self.status(false, "CPU rendering explicitly selected")));
        }
        let request = self.next_request();
        self.gpu_status
            .insert(request, (id.clone(), Instant::now() + GPU_STATUS_BOUND));
        post(json!({"op":"gpu_status","request":request}));
        None
    }

    fn expire_gpu_status(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .gpu_status
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(request, _)| *request)
            .collect();
        for request in expired {
            let (id, _) = self.gpu_status.remove(&request).unwrap();
            // Compositing may well be on the GPU: this says nothing either way.
            Self::respond(id, Err("GPU diagnostics timed out".into()));
        }
    }

    /// Handles a request. `None` means the response follows from the engine.
    fn request(
        &mut self,
        method: &str,
        id: &Option<Value>,
        p: &Value,
    ) -> Option<Result<Value, String>> {
        Some(match method {
            "version" => Ok(json!({"version":{"build":option_env!("LANESCAPE_BROWSER_BUILD")}})),
            "initialize" | "status" => {
                if method == "initialize"
                    && let Some(tokens) = p["capabilities"].as_array()
                {
                    for token in [
                        LOADING_TOKEN,
                        FENCE_TOKEN,
                        devtools::TOKEN,
                        ime::TOKEN,
                        clipboard::TOKEN,
                        menu::TOKEN,
                        recording::TOKEN,
                    ] {
                        if tokens.iter().any(|t| t == token) && !self.capabilities.contains(&token)
                        {
                            self.capabilities.push(token);
                        }
                    }
                    if self.capabilities.contains(&clipboard::TOKEN) {
                        self.clipboard.enable();
                    }
                    if self.capabilities.contains(&menu::TOKEN) {
                        self.menus.enable();
                    }
                    if let Some(saved) = self.saved.take() {
                        if let Err(e) = self.restore(saved, p["discard_saved_tabs"] == true) {
                            return Some(Err(e));
                        }
                        if self.restoring == 0 {
                            self.restore_over();
                        }
                    }
                    // Its answer says the restored tabs are there.
                    if self.restoring > 0 {
                        self.initializing.push(id.clone());
                        return None;
                    }
                }
                return self.status_request(id);
            }
            "tab.create" => return self.create_tab(id, p),
            "profile.release" => {
                let profile = match identifier(&p["profile"]) {
                    Ok(profile) => profile,
                    Err(e) => return Some(Err(e)),
                };
                let context = format!("profile-{profile}");
                // Tabs still being created count as much as those that exist.
                if self.tabs.values().any(|t| t.context == context)
                    || self.creating.values().any(|c| c.context == context)
                {
                    return Some(Err("close profile tabs before release".into()));
                }
                // Destroying the context drops any other flush of it, so one release serves all.
                if let Some((waiting, _)) = self.releasing.values_mut().find(|(_, c)| *c == context)
                {
                    waiting.push(id.clone());
                    return None;
                }
                let request = self.next_request();
                self.releasing
                    .insert(request, (vec![id.clone()], context.clone()));
                post(json!({"op":"release_context","context":context,"request":request}));
                return None;
            }
            "dialog.reply" => {
                let (Some(request), Some(tab)) = (p["request"].as_u64(), p["tab"].as_u64()) else {
                    return Some(Err("dialog request required".into()));
                };
                if self.dialogs.get(&request) != Some(&tab) {
                    return Some(Err("unknown dialog".into()));
                }
                self.dialogs.remove(&request);
                post(
                    json!({"op":"dialog_reply","tab":tab,"request":request,"accept":p["accept"] == true,"text":p["text"].as_str().unwrap_or("")}),
                );
                Ok(json!({}))
            }
            "display.fence" => return self.fence(id, p),
            "recording.start" | "recording.stop" => {
                if !self.capabilities.contains(&recording::TOKEN) {
                    return Some(Err("recording capability not negotiated".into()));
                }
                let Some(tab) = p["tab"].as_u64().filter(|t| self.tabs.contains_key(t)) else {
                    return Some(Err("unknown tab".into()));
                };
                if method == "recording.start" {
                    self.recordings.start(&self.runtime, tab)
                } else {
                    self.recordings.stop(tab)
                }
            }
            "devtools.open" => {
                if !self.capabilities.contains(&devtools::TOKEN) {
                    return Some(Err("devtools capability not negotiated".into()));
                }
                let Some(tab) = p["tab"].as_u64() else {
                    return Some(Err("tab required".into()));
                };
                let Some(display) = self.tabs.get(&tab).map(|t| t.display.clone()) else {
                    return Some(Err("unknown tab".into()));
                };
                self.devtools
                    .open(&self.runtime, tab, display, devtools_send)
            }
            "devtools.close" => match p["session"].as_u64() {
                Some(session) => self.devtools.request_close(session).map(|()| json!({})),
                None => Err("session required".into()),
            },
            // Checked on receipt, when its RFB input count is reached and again once the editor's
            // renderer confirmed the editor; only then does it reach the page and get its response.
            // The tab's next request is checked only after that, so it sees focus changes this one
            // causes.
            "ime.input" => return self.ime_input(id, p),
            "clipboard.transfer" => self.with_clipboard(|c, host| c.transfer(host, p)),
            "clipboard.read" => self.with_clipboard(|c, host| c.read(host, p)),
            // Checked on receipt and admitted once its RFB input count is reached; the response
            // acknowledges admission, ClipboardFinished the outcome.
            "clipboard.operation" => return self.clipboard_operation(id, p),
            "menu.reply" => self.menu_reply(p).map(|()| json!({})),
            // Capabilities this host doesn't offer yet refuse as if not negotiated.
            "preferences.set" => Err("user-preferences capability not negotiated".into()),
            "tab.list" => {
                let mut tabs: Vec<_> = self.tabs.iter().collect();
                tabs.sort_by_key(|(id, _)| **id);
                Ok(json!(
                    tabs.into_iter()
                        .map(|(id, t)| json!({"tab":id,"profile":profile_of(&t.context),"display":self.runtime.join(format!("{id}.sock"))}))
                        .collect::<Vec<_>>()
                ))
            }
            "shutdown" => {
                self.stopping = true;
                Ok(json!({}))
            }
            _ => return Some(self.tab_operation(method, p)),
        })
    }

    /// A renderer that never answers an IME request can't leave it, and the tab, waiting forever.
    fn expire_ime(&mut self) {
        let now = Instant::now();
        let expired: Vec<i64> = self
            .ime_applying
            .iter()
            .filter(|(_, (_, _, by))| now >= *by)
            .map(|(token, _)| *token)
            .collect();
        for token in expired {
            let (id, tab, _) = self.ime_applying.remove(&token).unwrap();
            // Still queued behind something, it must not reach the page after this answer.
            post(json!({"op":"ime_forget","tab":tab,"token":token}));
            Self::respond(id, Err("not the current editor".into()));
            self.held.finished(tab);
        }
    }

    /// Gives the clipboard the host it lives in.
    fn with_clipboard<T>(
        &mut self,
        f: impl FnOnce(&mut clipboard::Clipboard, &mut Host) -> T,
    ) -> T {
        let mut clipboard = std::mem::take(&mut self.clipboard);
        let result = f(&mut clipboard, self);
        self.clipboard = clipboard;
        result
    }

    fn clipboard_operation(
        &mut self,
        id: &Option<Value>,
        p: &Value,
    ) -> Option<Result<Value, String>> {
        let operation = id.as_ref().and_then(Value::as_u64).unwrap_or(0);
        if let Err(e) = self.with_clipboard(|c, host| c.operation(host, operation, p, false)) {
            return Some(Err(e));
        }
        let request = self
            .current_request
            .clone()
            .unwrap_or_else(|| json!({"id":id,"method":"clipboard.operation","params":p}));
        match self.held.hold(&request) {
            Ok(true) => return None,
            Ok(false) => {}
            Err(e) => return Some(Err(e)),
        }
        Some(
            self.with_clipboard(|c, host| c.operation(host, operation, p, true))
                .map(|()| json!({})),
        )
    }

    /// With `clipboard`, menu Cut and Copy run its copy flow; Paste completes only through
    /// ClipboardOperation, which carries the viewer's data.
    fn menu_reply(&mut self, p: &Value) -> Result<(), String> {
        if p["cancel"] != true && self.clipboard.enabled() {
            match p["command"].as_i64() {
                Some(112 | 113) => return self.with_clipboard(|c, host| c.menu_copy(host, p)),
                Some(114 | 115) => {
                    return Err("menu Paste is answered with ClipboardOperation".into());
                }
                _ => {}
            }
        }
        self.menus.reply(p)
    }

    fn ime_input(&mut self, id: &Option<Value>, p: &Value) -> Option<Result<Value, String>> {
        if !self.capabilities.contains(&ime::TOKEN) {
            return Some(Err("native-ime capability not negotiated".into()));
        }
        let tab = p["tab"].as_u64().unwrap_or(0);
        let controller = match self.tabs.get(&tab) {
            Some(t) => t.display.controller.load(Ordering::Acquire),
            None => return Some(Err("unknown tab".into())),
        };
        let dialog = self.dialogs.values().any(|owner| *owner == tab);
        let input = match self.ime.check(p, controller, dialog) {
            Ok(input) => input,
            Err(e) => return Some(Err(e)),
        };
        let request = json!({"id":id,"method":"ime.input","params":p});
        let request = self.current_request.clone().unwrap_or(request);
        match ordering::checked(&request) {
            Some(true) => {
                let token = self.next_request();
                self.ime.apply(input, token);
                self.ime_applying
                    .insert(token, (id.clone(), tab, Instant::now() + IME_BOUND));
                return None;
            }
            Some(false) => return Some(Err("not the current editor".into())),
            None => {}
        }
        match self.held.hold(&request) {
            Ok(true) => {}
            Ok(false) => {
                if let Err(e) = self.held.check(&request) {
                    return Some(Err(e));
                }
            }
            Err(e) => return Some(Err(e)),
        }
        None
    }

    /// Brings back the tabs a previous host saved, keeping their IDs, or with `discard` removes
    /// them and their unnamed profiles (specs/0002-control.md, Restarts).
    fn restore(&mut self, saved: Vec<Value>, discard: bool) -> Result<(), String> {
        let context_of = |tab: &Value| tab["context"].as_str().unwrap_or("").to_string();
        if discard {
            // Gone from the disk before their profiles go, and before Initialize answers, so a
            // host that dies now can't bring back the tabs it was told to drop. If that fails, the
            // next Initialize tries again.
            let ids: Vec<_> = saved.iter().filter_map(|t| t["tab"].as_u64()).collect();
            if let Err(e) = saved::forget_now(&self.root, ids) {
                self.saved = Some(saved);
                return Err(format!("can't discard saved tabs: {e}"));
            }
            for context in saved.iter().map(context_of) {
                if context.starts_with("unnamed-") {
                    remove_profile_directory(self.root.join(context));
                }
            }
            return Ok(());
        }
        self.to_restore = saved.into();
        self.restore_next();
        Ok(())
    }

    /// Restores the next saved tab that can come back now, one at a time, so a single saved
    /// history is in memory and on its way to Chromium at once.
    fn restore_next(&mut self) {
        while let Some(tab) = self.to_restore.pop_front() {
            let id = tab["tab"].as_u64().unwrap_or(0);
            let context = tab["context"].as_str().unwrap_or("").to_string();
            // A tab that doesn't fit now, or whose profile is being released, stays saved for the
            // next host to start.
            if self.tabs.len() + self.creating.len() >= 128
                || self.releasing.values().any(|(_, c)| *c == context)
            {
                self.deferred.insert(context);
                continue;
            }
            let tab = match saved::load(&self.root, id) {
                Ok(Some(tab)) => tab,
                // Gone or changed since startup read it: nothing to restore.
                Ok(None) => continue,
                Err(e) => {
                    eprintln!("lanescape-browser: can't read saved tab {id}: {e}");
                    self.deferred.insert(context);
                    continue;
                }
            };
            let directory = match context.strip_prefix("profile-") {
                Some(name) => profile_name(name),
                None => context.clone(),
            };
            let Ok(path) = self.profile_directory(&directory, false) else {
                self.deferred.insert(context);
                continue;
            };
            let request = self.next_request();
            self.creating.insert(
                request,
                PendingCreate {
                    id: None,
                    context: context.clone(),
                    restore: true,
                },
            );
            self.restoring += 1;
            CREATING.lock().unwrap().insert(request, Instant::now());
            post(
                json!({"op":"create_tab","request":request,"tab":id,"context":context,"path":path,"url":"about:blank","width":tab["width"],"height":tab["height"],"state":tab}),
            );
            return;
        }
    }

    /// Whether anything still needs an unnamed profile's storage: a tab, one being created or
    /// restored, or a saved tab not restored yet or at all.
    fn profile_in_use(&self, context: &str) -> bool {
        self.tabs.values().any(|t| t.context == context)
            || self.creating.values().any(|c| c.context == context)
            || self.to_restore.iter().any(|t| t["context"] == context)
            || self.deferred.contains(context)
    }

    /// The first Initialize has restored or discarded what it will: session storage that no live
    /// tab holds can go, except in profiles with saved tabs still to come back.
    fn restore_over(&self) {
        // Each context is then swept once the tabs closed so far are forgotten on disk
        // ("context_loaded"): a sweep before that, then a crash, would bring one back without
        // its storage.
        post(json!({"op":"scavenge","keep":self.deferred}));
    }

    /// One saved tab is back, or won't be; the first Initialize answers once all have.
    fn restored(&mut self) {
        self.restoring -= 1;
        self.restore_next();
        if self.restoring == 0 {
            self.restore_over();
            for id in std::mem::take(&mut self.initializing) {
                if let Some(result) = self.status_request(&id) {
                    Self::respond(id, result);
                }
            }
        }
    }

    /// A profile's directory under the data directory, created owner-only if it is new. A new
    /// unnamed profile's must be new.
    fn profile_directory(&self, name: &str, new: bool) -> Result<PathBuf, String> {
        let path = self.root.join(name);
        if path.is_symlink() {
            return Err("profile symlink refused".into());
        }
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(!new)
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn create_tab(&mut self, id: &Option<Value>, p: &Value) -> Option<Result<Value, String>> {
        let result = (|| -> Result<bool, String> {
            if self.tabs.len() + self.creating.len() >= 128 {
                return Err("tab limit reached".into());
            }
            let (width, height) = dimensions(p)?;
            let url = p["url"].as_str();
            if !url.is_some_and(cdp::navigable) {
                return Err("tabs load only web pages".into());
            }
            // A client from before unnamed profiles would otherwise get its "private" tab a shared
            // profile's storage.
            if p["private"] == true {
                return Err("private tabs are unnamed profiles: send an empty profile".into());
            }
            let (context, path) = if p["profile"].as_str().unwrap_or("").is_empty() {
                let context = unnamed_context()?;
                let path = self.profile_directory(&context, true)?;
                (context, path)
            } else {
                let profile = identifier(&p["profile"])?;
                let path = self.profile_directory(&profile_name(&profile), false)?;
                let context = format!("profile-{profile}");
                // Its context is being destroyed. Reusing it now would leave this tab's
                // WebContents on a context about to go, so this waits and runs after.
                if self.releasing.values().any(|(_, c)| *c == context) {
                    return Ok(false);
                }
                (context, path)
            };
            let request = self.next_request();
            self.creating.insert(
                request,
                PendingCreate {
                    id: id.clone(),
                    context: context.clone(),
                    restore: false,
                },
            );
            CREATING.lock().unwrap().insert(request, Instant::now());
            post(
                json!({"op":"create_tab","request":request,"context":context,"path":path,"url":url,"width":width,"height":height}),
            );
            Ok(true)
        })();
        match result {
            Ok(true) => None,
            Ok(false) => {
                self.after_release.push((id.clone(), p.clone()));
                None
            }
            Err(e) => Some(Err(e)),
        }
    }

    fn fence(&mut self, id: &Option<Value>, p: &Value) -> Option<Result<Value, String>> {
        if !self.capabilities.contains(&FENCE_TOKEN) {
            return Some(Err("display-fence capability not negotiated".into()));
        }
        let Some(tab) = p["tab"].as_u64().filter(|t| self.tabs.contains_key(t)) else {
            return Some(Err("unknown tab".into()));
        };
        if self.fences.len() >= FENCE_LIMIT {
            return Some(Err("too many display fences outstanding".into()));
        }
        let token = self.next_request();
        self.fences.insert(
            token,
            PendingFence {
                id: id.clone().unwrap_or(json!(0)),
                tab,
                deadline: Instant::now() + FENCE_BOUND,
            },
        );
        post(json!({"op":"fence","tab":tab,"token":token}));
        None
    }

    fn tab_operation(&mut self, method: &str, p: &Value) -> Result<Value, String> {
        let tab = p["tab"].as_u64().ok_or("tab required")?;
        let t = self.tabs.get_mut(&tab).ok_or("unknown tab")?;
        let viewer = p["viewer"].as_u64().unwrap_or(0);
        match method {
            "internal.release" => {
                // A grant that raced this disconnect names a viewer now gone.
                let _ = t.display.controller.compare_exchange(
                    viewer,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                if t.input.owner == viewer {
                    Self::release_input(tab, t);
                }
                // The RFB thread may already have cleared it, so this follows what's there, after
                // the releases above, and keeps any controller that replaced this viewer.
                let controlled = t.display.controller.load(Ordering::Acquire) != 0;
                post(json!({"op":"controlled","tab":tab,"controlled":controlled}));
            }
            "internal.pointer" | "internal.key" => {
                let controller = t.display.controller.load(Ordering::Acquire);
                if !input_authorized(p, controller) {
                    return Err("stale controller".into());
                }
                t.input.owner = controller;
                if method == "internal.key" {
                    Self::key(tab, t, p);
                } else {
                    Self::pointer(tab, t, p);
                }
            }
            "tab.navigate" => {
                let url = p["url"].as_str().filter(|u| cdp::navigable(u));
                let url = url.ok_or("tabs load only web pages")?;
                // While the old editor is live: once the navigation commits, Chromium has already
                // finished the composition into the page.
                self.ime.cancel_composition(tab);
                post(json!({"op":"navigate","tab":tab,"url":url}));
            }
            "tab.back" => {
                self.ime.cancel_composition(tab);
                post(json!({"op":"back","tab":tab}));
            }
            "tab.forward" => {
                self.ime.cancel_composition(tab);
                post(json!({"op":"forward","tab":tab}));
            }
            "tab.reload" => {
                self.ime.cancel_composition(tab);
                post(json!({"op":"reload","tab":tab}));
            }
            "tab.stop" => post(json!({"op":"stop","tab":tab})),
            "tab.resize" => {
                let (width, height) = dimensions(p)?;
                post(json!({"op":"resize","tab":tab,"width":width,"height":height}));
            }
            "viewer.control" => {
                let viewer = p["viewer"].as_u64().ok_or("viewer required")?;
                if viewer != 0 && !t.display.viewers.lock().unwrap().contains(&viewer) {
                    return Err("unknown viewer".into());
                }
                self.menus.controller_changed(tab, viewer);
                if t.display.controller.load(Ordering::Acquire) != viewer {
                    self.clipboard.controller_changed(tab);
                }
                Self::release_input(tab, t);
                let enabled = self.capabilities.contains(&ime::TOKEN);
                if self.ime.controller_changed(tab, viewer, enabled) {
                    refuse(self.held.controller_changed(tab), "CONTROL changed");
                }
                let t = self.tabs.get_mut(&tab).ok_or("unknown tab")?;
                if viewer != 0 {
                    post(json!({"op":"focus","tab":tab}));
                }
                t.input.owner = viewer;
                t.display.controller.store(viewer, Ordering::Release);
                post(json!({"op":"controlled","tab":tab,"controlled":viewer != 0}));
            }
            "tab.close" => {
                t.display.closed.store(true, Ordering::Release);
                post(json!({"op":"close","tab":tab}));
            }
            _ => return Err("unknown method".into()),
        }
        Ok(json!({}))
    }

    fn expire_fences(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .fences
            .iter()
            .filter(|(_, f)| f.deadline <= now)
            .map(|(token, _)| *token)
            .collect();
        for token in expired {
            let fence = self.fences.remove(&token).unwrap();
            post(json!({"op":"fence_cancel","tab":fence.tab,"token":token}));
            Self::respond(Some(fence.id), Err("display fence timed out".into()));
        }
    }
}

/// Answers requests that were held for RFB input and can no longer be applied.
fn refuse(requests: Vec<Value>, message: &str) {
    for request in requests {
        if let Some(id) = request.get("id") {
            event(json!({"id":id,"error":{"message":message}}));
        }
    }
}

fn control_loop(rx: mpsc::Receiver<Value>, tx: SyncSender<Value>, startup: &Startup) {
    let (previous_exit, saved_tabs, next_tab) = {
        let mut saved = startup.saved.lock().unwrap();
        (
            saved.previous_exit,
            std::mem::take(&mut saved.tabs),
            saved.next_tab,
        )
    };
    let mut host = Host {
        tx: tx.clone(),
        runtime: startup.runtime.clone(),
        root: startup.root.clone(),
        cpu: startup.cpu,
        capabilities: Vec::new(),
        tabs: HashMap::new(),
        creating: HashMap::new(),
        gpu_status: HashMap::new(),
        releasing: HashMap::new(),
        after_release: Vec::new(),
        fences: HashMap::new(),
        dialogs: HashMap::new(),
        next_request: 0,
        previous_exit,
        saved: Some(saved_tabs),
        reserved: next_tab,
        to_restore: Default::default(),
        restoring: 0,
        initializing: Vec::new(),
        deferred: Default::default(),
        sandbox_checked: false,
        stopping: false,
        current_request: None,
        devtools: devtools::DevTools::default(),
        ime: ime::Ime::default(),
        ime_applying: HashMap::new(),
        recordings: recording::Recordings::default(),
        clipboard: clipboard::Clipboard::default(),
        menus: menu::Menus::default(),
        held: ordering::Held::default(),
        ready: Default::default(),
        blocked: ordering::Blocked::default(),
    };
    let input = tx.clone();
    std::thread::spawn(move || {
        let mut reader = std::io::stdin().lock();
        while let Ok(Some(v)) = protocol::read(&mut reader) {
            if input.send(v).is_err() {
                return;
            }
        }
        let _ = input.send(json!({"method":"shutdown"}));
    });
    while !host.stopping {
        host.expire_fences();
        host.expire_gpu_status();
        host.devtools.tick();
        let enabled = host.capabilities.contains(&ime::TOKEN);
        host.ime.tick(enabled);
        host.expire_ime();
        host.with_clipboard(|c, host| c.tick(host));
        // Expiry, answers and new checks, whether or not a request arrives.
        let answers = host.ime.answers();
        let Host {
            held, ime, ready, ..
        } = &mut host;
        let (checked, refused) = held.tick(Instant::now(), answers, |request, token| {
            ime.send_check(request, token)
        });
        ready.extend(checked);
        refuse(refused, "not the current editor");
        // A tab no longer checking runs its waiting input before newer input.
        let mut replayed = false;
        let message = if let Some(request) = host.ready.pop_front() {
            request
        } else if let Some(request) = {
            let Host { blocked, held, .. } = &mut host;
            blocked.next(|tab| held.busy(tab))
        } {
            replayed = true;
            request
        } else {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        let method = message["method"].as_str().unwrap_or("");
        if let Some(name) = method.strip_prefix("engine.") {
            host.engine_event(name, &message["params"]);
            continue;
        }
        let p = &message["params"];
        let (tab, viewer) = (
            p["tab"].as_u64().unwrap_or(0),
            p["viewer"].as_u64().unwrap_or(0),
        );
        // Every RFB key and pointer message counts, whoever holds CONTROL; requests it releases run
        // right after it.
        match method {
            "internal.key" | "internal.pointer"
                if !replayed && (host.held.busy(tab) || host.blocked.waiting(tab)) =>
            {
                // A viewer flooding a tab that is checking is disconnected.
                if !host.blocked.push(message.clone())
                    && let Some(t) = host.tabs.get(&tab)
                {
                    t.display.disconnect(viewer);
                }
                continue;
            }
            // Before anything else, so later input waits behind their checks.
            "internal.key" | "internal.pointer" => {
                for request in host.held.input(tab, viewer).into_iter().rev() {
                    host.ready.push_front(request);
                }
            }
            "internal.release" => {
                host.clipboard.disconnected(tab, viewer);
                host.menus.disconnected(tab, viewer);
                host.blocked.disconnected(tab, viewer);
                refuse(host.held.disconnected(tab, viewer), "viewer disconnected");
                let enabled = host.capabilities.contains(&ime::TOKEN);
                if host.ime.disconnected(tab, viewer, enabled) {
                    refuse(host.held.controller_changed(tab), "CONTROL changed");
                }
            }
            _ => {}
        }
        let id = message.get("id").cloned();
        if !method.starts_with("internal.") {
            diagnostic(format_args!("control handling {method} id={id:?}"));
        }
        host.current_request = Some(message.clone());
        let result = host.request(method, &id, p);
        host.current_request = None;
        // A checked ImeInput on its way to the page finishes when the page has answered it.
        let answered = result.is_some();
        if let Some(result) = result {
            Host::respond(id, result);
        }
        if method == "ime.input" && answered && ordering::checked(&message).is_some() {
            host.held.finished(tab);
        }
    }
    refuse(host.held.all(), "shutting down");
    refuse(host.ready.drain(..).collect(), "shutting down");
    for (_, (id, _, _)) in host.ime_applying.drain() {
        Host::respond(id, Err("shutting down".into()));
    }
    let enabled = host.capabilities.contains(&ime::TOKEN);
    host.ime.shutdown(enabled);
    host.clipboard.shutdown();
    host.menus.shutdown();
    host.recordings.shutdown();
    for tab in host.tabs.values() {
        tab.display.closed.store(true, Ordering::Release);
    }
    for (_, fence) in host.fences.drain() {
        Host::respond(Some(fence.id), Err("shutting down".into()));
    }
    host.devtools.shutdown();
    post(json!({"op":"quit"}));
}

extern "C" fn on_started() {
    // Threads may start from here on: Chromium's startup has finished needing just one.
    let rx = CHANNEL
        .lock()
        .unwrap()
        .take()
        .expect("Chromium reports starting once");
    let tx = COMMANDS.get().expect("set before Chromium started").clone();
    let startup = STARTUP.get().expect("set before Chromium started");
    saved::start(startup.root.clone());
    watch_creating();
    post(json!({"op":"start","next_tab":startup.saved.lock().unwrap().next_tab}));
    std::thread::spawn(move || control_loop(rx, tx, startup));
}

extern "C" fn on_event(json: *const c_char, length: usize) {
    // SAFETY: the engine passes a valid buffer for the callback's duration.
    let bytes = unsafe { std::slice::from_raw_parts(json.cast::<u8>(), length) };
    let Ok(mut params) = serde_json::from_slice::<Value>(bytes) else {
        return;
    };
    let name = params["event"].as_str().unwrap_or("").to_string();
    params.as_object_mut().map(|o| o.remove("event"));
    // Straight to the disk, not through the control loop, which has stopped by the time shutdown
    // saves every tab.
    let tab = params["tab"].as_u64().unwrap_or(0);
    match name.as_str() {
        "tab_created" | "tab_refused" => {
            let request = params["request"].as_i64().unwrap_or(0);
            CREATING.lock().unwrap().remove(&request);
        }
        "tab_state" => return saved::save(tab, params),
        "tab_forget" => return saved::forget(tab),
        _ => {}
    }
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(json!({"method":format!("engine.{name}"),"params":params}));
    }
}

extern "C" fn on_frame(frame: *const LanescapeBrowserFrame) {
    // SAFETY: the frame and its pixels are valid for the callback's duration.
    let frame = unsafe { &*frame };
    let (width, height) = (frame.width as usize, frame.height as usize);
    if width == 0 || height == 0 || width > 8192 || height > 8192 || width * height > 16777216 {
        return;
    }
    let capture = rfb::Capture {
        width: width as u16,
        height: height as u16,
        stride: frame.stride as usize,
        pixels: frame.pixels,
        update: (frame.sequence_gap == 0).then_some((
            frame.update_x as u16,
            frame.update_y as u16,
            frame.update_width as u16,
            frame.update_height as u16,
        )),
        swapped: frame.byte_order == 1,
    };
    let mut slots = displays().lock().unwrap();
    let slot = slots.entry(frame.tab).or_default();
    match &slot.display {
        Some(display) => display.publish(&capture),
        None => slot.pending = Some(capture.frame(1)),
    }
}

extern "C" fn on_devtools(session: u64, message: *const u8, length: usize) {
    // SAFETY: the engine passes a valid buffer for the callback's duration.
    let message = unsafe { std::slice::from_raw_parts(message, length) };
    devtools::message_from_engine(session, message);
}

extern "C" fn on_recording(
    recording: u64,
    data: *const u8,
    length: usize,
    pts: i64,
    key: u8,
    width: u32,
    height: u32,
) {
    // SAFETY: the engine passes a valid buffer for the callback's duration, or none at the end.
    let data = (!data.is_null()).then(|| unsafe { std::slice::from_raw_parts(data, length) });
    recording::frame(recording, data, pts, key != 0, width, height);
}

extern "C" fn on_exit(code: c_int) -> c_int {
    recording::finish();
    if let Some(startup) = STARTUP.get() {
        saved::finish(&startup.root, code == 0);
        let _ = std::fs::remove_dir_all(&startup.runtime);
    }
    if let Some(message) = EXIT.lock().unwrap().take() {
        eprintln!("lanescape-browser: {message}");
    }
    code
}

/// The longest name a socket in the runtime directory has.
const LONGEST_SOCKET: &str = "recording-18446744073709551615.sock";

/// Where the runtime directory goes: the temporary directory, if its sockets' paths fit a Unix
/// socket address there. macOS allows 103 bytes and Linux 107, and macOS's per-user temporary
/// directory is already long.
fn runtime_parent() -> PathBuf {
    // SAFETY: sockaddr_un is plain data.
    let room = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
        .sun_path
        .len()
        - 1;
    let fits = |parent: &std::path::Path| {
        parent
            .join("browser-XXXXXX")
            .join(LONGEST_SOCKET)
            .as_os_str()
            .len()
            <= room
    };
    let temp = std::env::temp_dir();
    if fits(&temp) {
        temp
    } else {
        PathBuf::from("/tmp")
    }
}

fn create_runtime_directory() -> Result<PathBuf, String> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    // A unique, owner-only directory: one named by PID collides with a directory a killed engine
    // left behind once that PID is reused.
    let template = runtime_parent().join("browser-XXXXXX");
    let mut path = std::ffi::CString::new(template.as_os_str().as_bytes())
        .map_err(|e| e.to_string())?
        .into_bytes_with_nul();
    // SAFETY: path is a writable, NUL-terminated template.
    if unsafe { libc::mkdtemp(path.as_mut_ptr().cast()) }.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    path.pop();
    Ok(PathBuf::from(std::ffi::OsString::from_vec(path)))
}

fn data_directory(args: &[String]) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let root = args
        .windows(2)
        .find(|a| a[0] == "--data-dir")
        .map(|a| PathBuf::from(&a[1]))
        .ok_or("--data-dir is required")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)
        .map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let mode = root
        .metadata()
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    if mode & 0o777 != 0o700 {
        return Err("data directory must have mode 0700".into());
    }
    Ok(root)
}

/// Prepares the host, before Chromium starts and on the process's only thread, so it must not
/// start threads. Refuses to run on a bad launch.
fn prepare() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args
        .iter()
        .any(|a| a.starts_with("--rendering=") && a != "--rendering=cpu" && a != "--rendering=auto")
    {
        return Err("--rendering must be auto or cpu".into());
    }
    let root = data_directory(&args)?;
    // Before anything that would need undoing: a host refused the data directory leaves nothing.
    saved::lock(&root).map_err(|e| format!("data directory: {e}"))?;
    let saved = saved::open(&root).map_err(|e| format!("saved tabs: {e}"))?;
    let runtime = create_runtime_directory()?;
    if let Err(e) = protocol::claim_stdout() {
        let _ = std::fs::remove_dir_all(&runtime);
        return Err(e.to_string());
    }
    let cpu = args.iter().any(|a| a == "--rendering=cpu");
    let _ = STARTUP.set(Startup {
        root,
        runtime,
        cpu,
        saved: Mutex::new(saved),
    });
    let (tx, rx) = mpsc::sync_channel(1024);
    let _ = COMMANDS.set(tx);
    *CHANNEL.lock().unwrap() = Some(rx);
    Ok(())
}

/// `lanescape-browser MAJOR.BUILD`, such as `lanescape-browser 1.20261107.1`, or `lanescape-browser MAJOR.development` for a
/// binary built without a build number. The Version handshake keeps the build number alone.
fn version_line(build: Option<&str>) -> String {
    format!(
        "lanescape-browser {}.{}",
        protocol::MAJOR,
        build.unwrap_or("development")
    )
}

/// Runs before Chromium starts (engine/lanescape_browser.h).
///
/// # Safety
/// Both pointers must be valid, as the executable guarantees.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lanescape_browser_host_init(
    engine: *const LanescapeBrowserEngine,
    host: *mut LanescapeBrowserHost,
) -> c_int {
    if std::env::args().any(|a| a == "--version") {
        println!("{}", version_line(option_env!("LANESCAPE_BROWSER_BUILD")));
        return 0;
    }
    // SAFETY: the caller passes a valid engine.
    let _ = ENGINE.set(unsafe { *engine });
    if let Err(message) = prepare() {
        eprintln!("lanescape-browser: {message}");
        return 1;
    }
    // SAFETY: the caller passes valid storage to fill.
    unsafe {
        host.write(LanescapeBrowserHost {
            on_started,
            on_event,
            on_frame,
            on_devtools,
            on_recording,
            on_exit,
        });
    }
    -1
}

#[cfg(test)]
mod tests {

    #[test]
    fn version_line_is_major_then_build() {
        let major = super::protocol::MAJOR;
        assert_eq!(
            super::version_line(Some("20261107.1")),
            format!("lanescape-browser {major}.20261107.1")
        );
        assert_eq!(
            super::version_line(None),
            format!("lanescape-browser {major}.development")
        );
    }

    /// Every socket the host makes has a path short enough to bind, on every system.
    #[test]
    fn sockets_fit_their_address() {
        let longest = super::runtime_parent()
            .join("browser-XXXXXX")
            .join(super::LONGEST_SOCKET);
        let room = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }
            .sun_path
            .len();
        assert!(longest.as_os_str().len() < room, "{longest:?}");
    }
}
