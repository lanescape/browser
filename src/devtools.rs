//! One CDP session per tab on an owner-private Unix socket, framed like --remote-debugging-pipe.
//! Each client attaches and detaches its own DevToolsAgentHost client, and Chromium's session
//! teardown undoes what it left behind.
use crate::{cdp, post, rfb};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const TOKEN: &str = "devtools";

/// Sends a command to a session on Chromium's UI thread: session, engine id, whether it's input.
pub type Send = fn(u64, u64, bool, &[u8]);
const QUEUE: usize = 128 << 20;
const INFLIGHT: usize = 256;
const CONNECT: Duration = Duration::from_secs(15);

/// Command ids are engine-wide and never reused, so a reply owed to a departed client can never
/// answer a later client's command.
fn engine_id() -> Option<u64> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    cdp::allocate(&NEXT)
}

/// Whether the client hung up, checked while the reader is not consuming the socket.
fn hung_up(stream: &UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    #[cfg(target_os = "linux")]
    let events = libc::POLLIN | libc::POLLRDHUP;
    #[cfg(not(target_os = "linux"))]
    let events = libc::POLLIN;
    let mut poll = libc::pollfd {
        fd: stream.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: poll reads one valid descriptor.
    if unsafe { libc::poll(&mut poll, 1, 0) } <= 0 {
        return false;
    }
    if poll.revents & !libc::POLLIN != 0 {
        return true;
    }
    let mut byte = 0u8;
    // SAFETY: recv writes at most one byte to `byte`.
    let n = unsafe {
        libc::recv(
            poll.fd,
            (&raw mut byte).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n == 0
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reason {
    Client = 0,
    Requested = 1,
    TabClosed = 2,
    Overflow = 3,
    Protocol = 4,
    ConnectTimeout = 5,
    Shutdown = 6,
}

enum Item {
    Browser(Vec<u8>),
    Engine(Vec<u8>),
    /// A response too large to deliver, by engine id.
    Oversized(u64),
}

impl Item {
    fn len(&self) -> usize {
        match self {
            Item::Browser(b) | Item::Engine(b) => b.len(),
            Item::Oversized(_) => 0,
        }
    }
}

#[derive(Default)]
struct Outbound {
    items: VecDeque<Item>,
    bytes: usize,
    closed: bool,
}

struct Pending {
    client: u64,
    method: String,
    child: Option<String>,
}

struct Shared {
    queue: Mutex<Outbound>,
    ready: Condvar,
    reason: Mutex<Option<Reason>>,
    stream: Mutex<Option<UnixStream>>,
    /// In-flight commands by engine id.
    pending: Mutex<HashMap<u64, Pending>>,
    drained: Condvar,
    children: Mutex<HashMap<String, String>>,
    display: rfb::Shared,
}

impl Shared {
    /// Records the first reason only and wakes every thread.
    fn close(&self, reason: Reason) -> Reason {
        let reason = *self.reason.lock().unwrap().get_or_insert(reason);
        self.queue.lock().unwrap().closed = true;
        self.ready.notify_all();
        self.drained.notify_all();
        if let Some(s) = self.stream.lock().unwrap().as_ref() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        reason
    }

    fn closing(&self) -> bool {
        self.reason.lock().unwrap().is_some()
    }

    /// CDP cannot drop messages, so a full queue ends the session.
    fn push(&self, item: Item) {
        let len = item.len();
        let mut queue = self.queue.lock().unwrap();
        if queue.closed {
            return;
        }
        if queue.bytes + len > QUEUE {
            drop(queue);
            self.close(Reason::Overflow);
            return;
        }
        queue.bytes += len;
        queue.items.push_back(item);
        self.ready.notify_one();
    }

    fn controlled(&self) -> bool {
        self.display.controller.load(Ordering::Acquire) != 0
    }
}

fn live() -> &'static Mutex<HashMap<u64, Arc<Shared>>> {
    static LIVE: OnceLock<Mutex<HashMap<u64, Arc<Shared>>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// A viewer took CONTROL while this input command was queued.
pub fn refused_by_engine(session: u64, engine: u64) {
    let Some(shared) = live().lock().unwrap().get(&session).cloned() else {
        return;
    };
    if let Some(p) = answered(&shared, engine) {
        shared.push(Item::Engine(cdp::error(
            p.client,
            p.child.as_deref(),
            &p.method,
            "a viewer controls input",
        )));
    }
}

/// A message from a tab's agent host. Runs on Chromium's UI thread.
pub fn message_from_engine(session: u64, message: &[u8]) {
    let Some(shared) = live().lock().unwrap().get(&session).cloned() else {
        return;
    };
    if message.len() <= cdp::MESSAGE {
        shared.push(Item::Browser(message.to_vec()));
    } else if let Some(id) = cdp::leading_id(message) {
        shared.push(Item::Oversized(id));
    } else {
        shared.close(Reason::Overflow);
    }
}

struct Session {
    tab: u64,
    shared: Arc<Shared>,
    socket: PathBuf,
}

/// A closed session waiting for Chromium to detach it. Its DevToolsClosed follows, so a navigation
/// after that event carries none of the client's state.
struct Detaching {
    tab: u64,
    reason: Reason,
}

#[derive(Default)]
pub struct DevTools {
    next: u64,
    sessions: HashMap<u64, Session>,
    detaching: HashMap<u64, Detaching>,
}

fn closed(tab: u64, session: u64, reason: Reason) {
    crate::event(
        json!({"method":"devtools.closed","params":{"tab":tab,"session":session,"reason":reason as i32}}),
    );
}

impl DevTools {
    pub fn open(
        &mut self,
        runtime: &Path,
        tab: u64,
        display: rfb::Shared,
        send: Send,
    ) -> Result<Value, String> {
        if self.sessions.values().any(|s| s.tab == tab) {
            return Err("tab already has a devtools session".into());
        }
        if self.detaching.values().any(|d| d.tab == tab) {
            return Err(
                "the tab's previous devtools session has not reported DevToolsClosed".into(),
            );
        }
        self.next += 1;
        let id = self.next;
        let socket = runtime.join(format!("devtools-{id}.sock"));
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        let ready = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| listener.set_nonblocking(true));
        if let Err(e) = ready {
            let _ = std::fs::remove_file(&socket);
            return Err(e.to_string());
        }
        let shared = Arc::new(Shared {
            queue: Mutex::default(),
            ready: Condvar::new(),
            reason: Mutex::new(None),
            stream: Mutex::new(None),
            pending: Mutex::default(),
            drained: Condvar::new(),
            children: Mutex::default(),
            display,
        });
        live().lock().unwrap().insert(id, shared.clone());
        post(json!({"op":"devtools_attach","tab":tab,"session":id}));
        let path = socket.clone();
        let accept_shared = shared.clone();
        thread::spawn(move || accept(listener, path, id, accept_shared, send));
        self.sessions.insert(
            id,
            Session {
                tab,
                shared,
                socket: socket.clone(),
            },
        );
        Ok(json!({"tab":tab,"session":id,"socket":socket}))
    }

    pub fn request_close(&mut self, session: u64) -> Result<(), String> {
        if !self.sessions.contains_key(&session) {
            return Err("unknown devtools session".into());
        }
        self.close(session, Reason::Requested);
        Ok(())
    }

    /// Detaches in Chromium, then reports DevToolsClosed once it has.
    fn close(&mut self, id: u64, reason: Reason) {
        let Some(s) = self.sessions.remove(&id) else {
            return;
        };
        let reason = s.shared.close(reason);
        live().lock().unwrap().remove(&id);
        let _ = std::fs::remove_file(&s.socket);
        self.detaching.insert(id, Detaching { tab: s.tab, reason });
        post(json!({"op":"devtools_detach","tab":s.tab,"session":id}));
    }

    pub fn detached(&mut self, session: u64) {
        if let Some(d) = self.detaching.remove(&session) {
            closed(d.tab, session, d.reason);
        }
    }

    /// Reports the tab's session as closed, before its Closed event.
    pub fn closed_tab(&mut self, tab: u64) {
        let ids: Vec<u64> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.tab == tab)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let s = self.sessions.remove(&id).unwrap();
            let reason = s.shared.close(Reason::TabClosed);
            live().lock().unwrap().remove(&id);
            let _ = std::fs::remove_file(&s.socket);
            closed(tab, id, reason);
        }
        self.detaching.retain(|session, d| {
            if d.tab == tab {
                closed(tab, *session, d.reason);
                return false;
            }
            true
        });
    }

    pub fn shutdown(&mut self) {
        let ids: Vec<u64> = self.sessions.keys().copied().collect();
        for id in ids {
            let s = self.sessions.remove(&id).unwrap();
            let reason = s.shared.close(Reason::Shutdown);
            live().lock().unwrap().remove(&id);
            let _ = std::fs::remove_file(&s.socket);
            closed(s.tab, id, reason);
        }
    }

    /// Closes sessions whose socket threads ended them.
    pub fn tick(&mut self) {
        let ended: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| (*s.shared.reason.lock().unwrap()).map(|r| (*id, r)))
            .collect();
        for (id, reason) in ended {
            self.close(id, reason);
        }
    }
}

fn accept(listener: UnixListener, path: PathBuf, session: u64, shared: Arc<Shared>, send: Send) {
    let deadline = Instant::now() + CONNECT;
    let stream = loop {
        if shared.closing() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                shared.close(if e.kind() == io::ErrorKind::WouldBlock {
                    Reason::ConnectTimeout
                } else {
                    Reason::Client
                });
                return;
            }
        }
    };
    // One client per session: the path disappears once it is taken.
    let _ = std::fs::remove_file(&path);
    drop(listener);
    let (Ok(()), Ok(writer), Ok(control)) = (
        stream.set_nonblocking(false),
        stream.try_clone(),
        stream.try_clone(),
    ) else {
        shared.close(Reason::Client);
        return;
    };
    *shared.stream.lock().unwrap() = Some(control);
    if shared.closing() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
        return;
    }
    let write_shared = shared.clone();
    thread::spawn(move || write(writer, write_shared));
    read(stream, session, shared, send);
}

fn read(stream: UnixStream, session: u64, shared: Arc<Shared>, send: Send) {
    let mut reader = io::BufReader::with_capacity(1 << 16, stream);
    loop {
        let mut frame = Vec::new();
        match cdp::read_frame(&mut reader, &mut frame) {
            Ok(true) => {}
            Ok(false) => {
                shared.close(Reason::Client);
                return;
            }
            Err(e) => {
                shared.close(if e.kind() == io::ErrorKind::InvalidData {
                    Reason::Protocol
                } else {
                    Reason::Client
                });
                return;
            }
        }
        let Some(command) = cdp::parse(&frame) else {
            shared.close(Reason::Protocol);
            return;
        };
        {
            let mut pending = shared.pending.lock().unwrap();
            while pending.len() >= INFLIGHT && !shared.closing() {
                // Close notifies without this lock, and a hangup sends nothing; recheck both.
                if hung_up(reader.get_ref()) {
                    drop(pending);
                    shared.close(Reason::Client);
                    return;
                }
                pending = shared
                    .drained
                    .wait_timeout(pending, Duration::from_millis(100))
                    .unwrap()
                    .0;
            }
            if shared.closing() {
                return;
            }
            if pending.values().any(|p| p.client == command.id) {
                drop(pending);
                shared.close(Reason::Protocol);
                return;
            }
        }
        let children = shared.children.lock().unwrap().clone();
        let reason = if command
            .session
            .as_ref()
            .is_some_and(|s| !children.contains_key(s))
        {
            Some("the session is not attached to this tab")
        } else {
            cdp::refusal(&command, &children, shared.controlled())
        };
        if let Some(reason) = reason {
            shared.push(Item::Engine(cdp::error(
                command.id,
                command.session.as_deref(),
                &command.method,
                reason,
            )));
            continue;
        }
        let Some(engine) = engine_id() else {
            shared.push(Item::Engine(cdp::error(
                command.id,
                command.session.as_deref(),
                &command.method,
                "the engine's command ids are exhausted",
            )));
            continue;
        };
        let bytes = cdp::encode(&command, engine);
        shared.pending.lock().unwrap().insert(
            engine,
            Pending {
                client: command.id,
                method: command.method.clone(),
                child: command.session.clone(),
            },
        );
        send(
            session,
            engine,
            command.method.starts_with("Input."),
            &bytes,
        );
    }
}

fn write(mut stream: UnixStream, shared: Arc<Shared>) {
    loop {
        let item = {
            let mut queue = shared.queue.lock().unwrap();
            while queue.items.is_empty() && !queue.closed {
                queue = shared.ready.wait(queue).unwrap();
            }
            if queue.closed {
                return;
            }
            let item = queue.items.pop_front().unwrap();
            queue.bytes -= item.len();
            item
        };
        let Some(bytes) = deliver(item, &shared) else {
            continue;
        };
        if stream
            .write_all(&bytes)
            .and_then(|()| stream.write_all(&[0]))
            .is_err()
        {
            shared.close(Reason::Client);
            return;
        }
    }
}

fn answered(shared: &Shared, engine: u64) -> Option<Pending> {
    let pending = shared.pending.lock().unwrap().remove(&engine);
    shared.drained.notify_all();
    pending
}

/// Prepares one browser message for the client, off the UI thread. Only messages that need
/// tracking or cookie scrubbing are decoded; the rest pass through byte for byte.
fn deliver(item: Item, shared: &Shared) -> Option<Vec<u8>> {
    let bytes = match item {
        Item::Engine(bytes) => return Some(bytes),
        Item::Oversized(engine) => {
            let p = answered(shared, engine)?;
            return Some(cdp::error(
                p.client,
                p.child.as_deref(),
                &p.method,
                "the response exceeds 64 MiB",
            ));
        }
        Item::Browser(bytes) => bytes,
    };
    if let Some(engine) = cdp::leading_id(&bytes) {
        let p = answered(shared, engine)?;
        if !cdp::needs_scrub(&p.method) {
            return cdp::with_id(&bytes, p.client);
        }
        let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else {
            return Some(cdp::error(
                p.client,
                p.child.as_deref(),
                &p.method,
                "the response could not be filtered",
            ));
        };
        v["id"] = p.client.into();
        cdp::scrub(&mut v);
        return Some(serde_json::to_vec(&v).unwrap());
    }
    let tracked = |m: &str| {
        cdp::needs_scrub(m) || matches!(m, "Target.attachedToTarget" | "Target.detachedFromTarget")
    };
    if cdp::leading_method(&bytes).is_some_and(|m| !tracked(m)) {
        return Some(bytes);
    }
    let mut v = serde_json::from_slice::<Value>(&bytes).ok()?;
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    let params = &v["params"];
    match method {
        "Target.attachedToTarget" => {
            if let (Some(session), Some(target)) = (
                params["sessionId"].as_str(),
                params["targetInfo"]["targetId"].as_str(),
            ) {
                shared
                    .children
                    .lock()
                    .unwrap()
                    .insert(session.into(), target.into());
            }
        }
        "Target.detachedFromTarget" => {
            if let Some(session) = params["sessionId"].as_str() {
                shared.children.lock().unwrap().remove(session);
            }
        }
        _ => {}
    }
    if !cdp::needs_scrub(method) {
        return Some(bytes);
    }
    cdp::scrub(&mut v);
    Some(serde_json::to_vec(&v).unwrap())
}
