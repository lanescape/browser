//! What the embedder's tests share: the real executable named by `LANESCAPE_BROWSER_BINARY`, driven over
//! its control channel, its RFB displays and its DevTools sockets. Tests describe what Lanescape browser
//! needs, and wait for the state they want rather than assuming an order between channels.
#![allow(dead_code)]

pub mod control {
    include!(concat!(env!("OUT_DIR"), "/lanescape.browser.rs"));
}
pub mod processes;
pub use control::*;
use prost::Message;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::{io::AsRawFd, net::UnixStream},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

/// How long anything is expected to take on a loaded machine.
pub const PATIENCE: Duration = Duration::from_secs(20);

pub const IME: &str = "native-ime";
pub const DEVTOOLS: &str = "devtools";
pub const CLIPBOARD: &str = "clipboard";
pub const FENCE: &str = "display-fence";
pub const LOADING: &str = "loading-state";
pub const RECORDING: &str = "recording";

/// Serves each path's page on every interface, so `127.0.0.1` and `localhost` are two sites.
pub struct Pages {
    pub port: u16,
}
impl Pages {
    pub fn serve(pages: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pages: Vec<(String, String)> = pages
            .iter()
            .map(|(path, body)| {
                (
                    (*path).to_owned(),
                    body.replace("{port}", &port.to_string()),
                )
            })
            .collect();
        thread::spawn(move || {
            for socket in listener.incoming().flatten() {
                let pages = pages.clone();
                thread::spawn(move || {
                    let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut reader = BufReader::new(&socket);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(n) if n > 2 => {}
                            _ => break,
                        }
                    }
                    let body = pages
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map_or("", |(_, b)| b.as_str());
                    let mut socket = &socket;
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        Self { port }
    }
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
    /// The same path on another site, so it gets a renderer of its own.
    pub fn other_site(&self, path: &str) -> String {
        format!("http://localhost:{}{path}", self.port)
    }
}

pub struct Client {
    pub child: Child,
    next: u64,
    pub root: std::path::PathBuf,
    pub events: Vec<event::Event>,
    responses: Vec<Response>,
}
impl Client {
    pub fn start(capabilities: &[&str]) -> Self {
        Self::start_mode("--rendering=cpu", capabilities)
    }
    /// `mode` is the host's rendering flag.
    pub fn start_mode(mode: &str, capabilities: &[&str]) -> Self {
        Self::start_args(&[mode], capabilities)
    }
    /// With these in its environment.
    pub fn start_env(mode: &str, env: &[(&str, &str)]) -> Self {
        let mut client = Self::start_args(&[], &[]);
        let _ = client.child.kill();
        let _ = client.child.wait();
        client.child = Command::new(std::env::var("LANESCAPE_BROWSER_BINARY").unwrap())
            .args(["--data-dir", client.root.to_str().unwrap(), mode])
            .envs(env.iter().copied())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        client
    }
    /// These launch flags, the first being the host's rendering flag.
    pub fn start_args(args: &[&str], capabilities: &[&str]) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "lanescape-browser-embedder-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = root.canonicalize().unwrap();
        let mut client = Self {
            child: Self::spawn(&root, args),
            next: 0,
            root,
            events: Vec::new(),
            responses: Vec::new(),
        };
        if !capabilities.is_empty() {
            client.initialize(capabilities);
        }
        client
    }
    fn spawn(root: &std::path::Path, args: &[&str]) -> Child {
        Command::new(
            std::env::var("LANESCAPE_BROWSER_BINARY")
                .expect("LANESCAPE_BROWSER_BINARY: the executable to test"),
        )
        .args(["--data-dir", root.to_str().unwrap()])
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap()
    }
    /// A new process on the same data directory.
    pub fn restart(&mut self, capabilities: &[&str]) {
        self.stop();
        self.child = Self::spawn(&self.root, &["--rendering=cpu"]);
        self.events.clear();
        self.responses.clear();
        if !capabilities.is_empty() {
            self.initialize(capabilities);
        }
    }
    pub fn initialize(&mut self, capabilities: &[&str]) -> Status {
        let result = self.request(request::Command::Initialize(Initialize {
            capabilities: capabilities.iter().map(|c| (*c).to_owned()).collect(),
        }));
        let response::Result::Status(status) = result else {
            panic!("{result:?}");
        };
        for wanted in capabilities {
            assert!(
                status.capabilities.iter().any(|c| c == wanted),
                "{wanted}: {status:?}"
            );
        }
        status
    }
    pub fn request(&mut self, command: request::Command) -> response::Result {
        let id = self.send(command);
        self.response(id)
    }
    pub fn send(&mut self, command: request::Command) -> u64 {
        self.next += 1;
        let bytes = Request {
            id: self.next,
            command: Some(command),
        }
        .encode_to_vec();
        let input = self.child.stdin.as_mut().unwrap();
        input
            .write_all(&(bytes.len() as u32).to_le_bytes())
            .unwrap();
        input.write_all(&bytes).unwrap();
        self.next
    }
    pub fn response(&mut self, id: u64) -> response::Result {
        if let Some(i) = self.responses.iter().position(|r| r.id == id) {
            return self.responses.remove(i).result.unwrap();
        }
        loop {
            match self.output(PATIENCE).expect("control output timed out") {
                output::Body::Response(r) if r.id == id => return r.result.unwrap(),
                output::Body::Response(r) => self.responses.push(r),
                output::Body::Event(e) => self.events.push(e.event.unwrap()),
            }
        }
    }
    #[track_caller]
    pub fn ok(&mut self, command: request::Command) {
        let result = self.request(command);
        assert!(matches!(result, response::Result::Ok(_)), "{result:?}");
    }
    #[track_caller]
    pub fn refused(&mut self, command: request::Command, reason: &str) {
        let result = self.request(command);
        assert!(
            matches!(&result, response::Result::Error(e) if e.message.contains(reason)),
            "expected {reason}: {result:?}"
        );
    }
    fn output(&mut self, wait: Duration) -> Option<output::Body> {
        let out = self.child.stdout.as_mut().unwrap();
        let mut poll = libc::pollfd {
            fd: out.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut poll, 1, wait.as_millis() as i32) } <= 0 {
            return None;
        }
        let mut n = [0; 4];
        out.read_exact(&mut n).unwrap();
        let n = u32::from_le_bytes(n) as usize;
        let mut data = vec![0; n];
        out.read_exact(&mut data).unwrap();
        Some(Output::decode(&*data).unwrap().body.unwrap())
    }
    /// The first kept or coming event that matches, which it uses up.
    #[track_caller]
    pub fn event(&mut self, matches: impl Fn(&event::Event) -> bool) -> event::Event {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(i) = self.events.iter().position(&matches) {
                return self.events.remove(i);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.output(left) {
                Some(output::Body::Event(e)) => self.events.push(e.event.unwrap()),
                Some(output::Body::Response(r)) => self.responses.push(r),
                None => panic!("no matching event; have {:?}", self.events),
            }
        }
    }
    /// Events kept so far can't answer a later wait.
    pub fn forget_events(&mut self) {
        self.events.clear();
    }
    /// Keeps whatever arrives during `wait`.
    pub fn collect(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.output(left) {
                Some(output::Body::Event(e)) => self.events.push(e.event.unwrap()),
                Some(output::Body::Response(r)) => self.responses.push(r),
                None => return,
            }
        }
    }
    pub fn tab_sized(
        &mut self,
        profile: &str,
        private: bool,
        url: &str,
        width: u32,
        height: u32,
    ) -> Tab {
        let result = self.request(request::Command::CreateTab(CreateTab {
            profile: profile.into(),
            private,
            url: url.into(),
            width,
            height,
        }));
        let response::Result::Tab(tab) = result else {
            panic!("{result:?}");
        };
        tab
    }
    pub fn tab(&mut self, profile: &str, private: bool, url: &str) -> Tab {
        self.tab_sized(profile, private, url, 400, 300)
    }
    pub fn loaded(&mut self, tab: u64, url: &str) {
        self.event(|e| matches!(e, event::Event::Loaded(l) if l.tab == tab && l.url == url));
    }
    pub fn action(&mut self, tab: u64, action: tab_operation::Action, viewer: u64) {
        self.ok(request::Command::TabOperation(TabOperation {
            tab,
            action: action as i32,
            viewer,
            ..Default::default()
        }));
    }
    pub fn navigate(&mut self, tab: u64, url: &str) {
        self.ok(request::Command::TabOperation(TabOperation {
            tab,
            action: tab_operation::Action::Navigate as i32,
            url: url.into(),
            ..Default::default()
        }));
        self.loaded(tab, url);
    }
    pub fn viewer_id(&mut self, tab: u64) -> u64 {
        match self.event(|e| matches!(e, event::Event::Viewer(v) if v.tab == tab && v.connected)) {
            event::Event::Viewer(v) => v.viewer,
            _ => unreachable!(),
        }
    }
    pub fn devtools(&mut self, tab: u64) -> Cdp {
        let result = self.request(request::Command::OpenDevtools(OpenDevTools { tab }));
        let response::Result::Devtools(d) = result else {
            panic!("{result:?}");
        };
        Cdp::connect(&d.socket)
    }
    /// A viewer connected and holding CONTROL of the tab, with a session on its page.
    pub fn controlled(&mut self, tab: &Tab) -> Controlled {
        let viewer = Viewer::connect(tab, false);
        let id = self.viewer_id(tab.tab);
        self.action(tab.tab, tab_operation::Action::Control, id);
        self.editor(tab.tab, |s| s.viewer == id && s.editor == 0);
        let page = self.devtools(tab.tab);
        Controlled {
            tab: tab.tab,
            id,
            viewer,
            page,
        }
    }
    #[track_caller]
    pub fn editor(&mut self, tab: u64, matches: impl Fn(&EditorState) -> bool) -> EditorState {
        match self
            .event(|e| matches!(e, event::Event::EditorState(s) if s.tab == tab && matches(s)))
        {
            event::Event::EditorState(s) => s,
            _ => unreachable!(),
        }
    }
    pub fn fence(&mut self, tab: u64) -> Result<u64, String> {
        match self.request(request::Command::DisplayFence(DisplayFence { tab })) {
            response::Result::DisplayFenced(f) => Ok(f.sequence),
            response::Result::Error(e) => Err(e.message),
            r => panic!("{r:?}"),
        }
    }
    /// Starts recording a tab, with a reader taking its stream.
    pub fn record(&mut self, tab: u64) -> Stream {
        let result = self.request(request::Command::StartRecording(StartRecording { tab }));
        let response::Result::Recording(recording) = result else {
            panic!("{result:?}");
        };
        Stream::read(&recording)
    }
    /// Asks a recording to stop, and returns how it ended.
    #[track_caller]
    pub fn stop_recording(&mut self, tab: u64) -> RecordingEnded {
        self.ok(request::Command::StopRecording(StopRecording { tab }));
        self.recording_ended(tab)
    }
    #[track_caller]
    pub fn recording_ended(&mut self, tab: u64) -> RecordingEnded {
        match self.event(|e| matches!(e, event::Event::RecordingEnded(r) if r.tab == tab)) {
            event::Event::RecordingEnded(r) => r,
            _ => unreachable!(),
        }
    }
    /// A sealed upload of `text`.
    #[track_caller]
    pub fn upload(&mut self, tab: u64, viewer: u64, text: &[u8]) -> u64 {
        use clipboard_transfer::Action;
        let result = self.request(request::Command::ClipboardTransfer(ClipboardTransfer {
            size: text.len() as u64,
            ..transfer(tab, viewer, Action::Create, 0)
        }));
        let response::Result::ClipboardTransfer(created) = result else {
            panic!("{result:?}");
        };
        for (i, chunk) in text.chunks(49152).enumerate() {
            self.ok(request::Command::ClipboardTransfer(ClipboardTransfer {
                offset: (i * 49152) as u64,
                data: chunk.to_vec(),
                ..transfer(tab, viewer, Action::Write, created.transfer)
            }));
        }
        self.ok(request::Command::ClipboardTransfer(transfer(
            tab,
            viewer,
            Action::Seal,
            created.transfer,
        )));
        created.transfer
    }
    fn read_transfer(&mut self, tab: u64, viewer: u64, id: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let result = self.request(request::Command::ReadClipboardTransfer(
                ReadClipboardTransfer {
                    tab,
                    viewer,
                    transfer: id,
                    offset: out.len() as u64,
                    length: 0,
                },
            ));
            let response::Result::ClipboardChunk(chunk) = result else {
                panic!("{result:?}");
            };
            out.extend(chunk.data);
            if chunk.end {
                return out;
            }
        }
    }
    /// How operation `id` ended, and the text it copied.
    #[track_caller]
    pub fn outcome(
        &mut self,
        tab: u64,
        viewer: u64,
        id: u64,
    ) -> (clipboard_finished::Status, Option<Vec<u8>>) {
        let event::Event::ClipboardFinished(finished) = self.event(
            |e| matches!(e, event::Event::ClipboardFinished(f) if f.tab == tab && f.operation == id),
        ) else {
            unreachable!()
        };
        let copied = self.events.iter().position(
            |e| matches!(e, event::Event::ClipboardCopied(c) if c.tab == tab && c.operation == id),
        );
        let text = copied.map(|i| {
            let event::Event::ClipboardCopied(copied) = self.events.remove(i) else {
                unreachable!()
            };
            let id = copied.representations[0].transfer;
            let text = self.read_transfer(tab, viewer, id);
            self.ok(request::Command::ClipboardTransfer(transfer(
                tab,
                viewer,
                clipboard_transfer::Action::Release,
                id,
            )));
            text
        });
        (finished.status(), text)
    }
    /// Runs an operation to its end.
    #[track_caller]
    pub fn operate(
        &mut self,
        tab: u64,
        viewer: u64,
        kind: clipboard_operation::Kind,
        transfers: Vec<u64>,
    ) -> (clipboard_finished::Status, Option<Vec<u8>>) {
        let id = self.send(request::Command::ClipboardOperation(ClipboardOperation {
            tab,
            viewer,
            kind: kind as i32,
            accept: vec!["text/plain".into()],
            transfers,
            ..Default::default()
        }));
        let result = self.response(id);
        assert!(matches!(result, response::Result::Ok(_)), "{result:?}");
        self.outcome(tab, viewer, id)
    }
    pub fn stop(&mut self) {
        self.ok(request::Command::Shutdown(Empty {}));
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "shutdown: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "shutdown timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }
    /// The host's direct children: its renderers and other helpers.
    pub fn helpers(&self) -> Vec<i32> {
        let out = Command::new("pgrep")
            .args(["-P", &self.child.id().to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn transfer(
    tab: u64,
    viewer: u64,
    action: clipboard_transfer::Action,
    transfer: u64,
) -> ClipboardTransfer {
    ClipboardTransfer {
        tab,
        viewer,
        action: action as i32,
        transfer,
        format: "text/plain".into(),
        ..Default::default()
    }
}

/// An IME request with its selection a caret at the text's end.
pub fn ime(c: &Controlled, editor: u64, action: ime_input::Action, text: &str) -> request::Command {
    let end = text.encode_utf16().count() as u32;
    request::Command::ImeInput(ImeInput {
        tab: c.tab,
        viewer: c.id,
        editor,
        action: action as i32,
        text: text.into(),
        selection_start: end,
        selection_end: end,
        ..Default::default()
    })
}

/// A recording's reader, taking everything until the stream ends.
pub struct Stream {
    reader: thread::JoinHandle<Vec<u8>>,
}
impl Stream {
    fn read(recording: &Recording) -> Self {
        Self::read_after(recording, Duration::ZERO)
    }
    /// A reader that connects at once and then waits before reading anything.
    pub fn read_after(recording: &Recording, stall: Duration) -> Self {
        let mut socket = UnixStream::connect(&recording.socket).unwrap();
        let reader = thread::spawn(move || {
            thread::sleep(stall);
            let mut bytes = Vec::new();
            socket.read_to_end(&mut bytes).unwrap();
            bytes
        });
        Self { reader }
    }
    /// All the bytes, once the stream has ended.
    pub fn finish(self) -> Vec<u8> {
        self.reader.join().unwrap()
    }
}

pub struct Controlled {
    pub tab: u64,
    pub id: u64,
    pub viewer: Viewer,
    pub page: Cdp,
}

/// Reads an RFB display as a viewer does, in raw pixels or ZRLE.
pub struct Viewer {
    pub socket: UnixStream,
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
    decompressor: flate2::Decompress,
    pub encoding: i32,
    pub sequence: u64,
    /// The rectangles of the last update, and the bytes it took on the wire.
    pub rects: Vec<(usize, usize, usize, usize)>,
    pub wire_bytes: usize,
}
/// The pseudo-encoding that numbers updates.
const FRAME_SEQUENCE: i32 = 0x4C53_4653;
#[track_caller]
fn exact(s: &mut UnixStream, n: usize) -> Vec<u8> {
    let mut b = vec![0; n];
    s.read_exact(&mut b)
        .unwrap_or_else(|e| panic!("RFB read {n} bytes: {e}"));
    b
}
impl Viewer {
    pub fn connect(tab: &Tab, zrle: bool) -> Self {
        let mut socket = UnixStream::connect(&tab.display).unwrap();
        socket.set_read_timeout(Some(PATIENCE)).unwrap();
        assert_eq!(exact(&mut socket, 12), b"RFB 003.008\n");
        socket.write_all(b"RFB 003.008\n").unwrap();
        assert_eq!(exact(&mut socket, 2), [1, 1]);
        socket.write_all(&[1]).unwrap();
        assert_eq!(exact(&mut socket, 4), [0; 4]);
        socket.write_all(&[1]).unwrap();
        let init = exact(&mut socket, 24);
        let width = u16::from_be_bytes(init[..2].try_into().unwrap());
        let height = u16::from_be_bytes(init[2..4].try_into().unwrap());
        let length = u32::from_be_bytes(init[20..24].try_into().unwrap()) as usize;
        exact(&mut socket, length);
        let mut viewer = Self {
            socket,
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
            decompressor: flate2::Decompress::new(true),
            encoding: 0,
            sequence: 0,
            rects: Vec::new(),
            wire_bytes: 0,
        };
        viewer.set_encodings(&[if zrle { 16 } else { 0 }, -223, FRAME_SEQUENCE]);
        viewer
    }
    pub fn set_encodings(&mut self, list: &[i32]) {
        let mut b = vec![2, 0];
        b.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for encoding in list {
            b.extend_from_slice(&encoding.to_be_bytes());
        }
        self.socket.write_all(&b).unwrap();
    }
    pub fn key(&mut self, down: bool, key: u32) {
        let mut b = vec![4, u8::from(down), 0, 0];
        b.extend_from_slice(&key.to_be_bytes());
        self.socket.write_all(&b).unwrap();
    }
    /// Presses and releases a key.
    pub fn tap(&mut self, key: u32) {
        self.key(true, key);
        self.key(false, key);
    }
    pub fn pointer(&mut self, mask: u8, x: u16, y: u16) {
        let mut b = vec![5, mask];
        b.extend_from_slice(&x.to_be_bytes());
        b.extend_from_slice(&y.to_be_bytes());
        self.socket.write_all(&b).unwrap();
    }
    pub fn click(&mut self, x: u16, y: u16) {
        self.pointer(1, x, y);
        self.pointer(0, x, y);
    }
    pub fn request(&mut self, incremental: bool) {
        let mut b = vec![3, u8::from(incremental), 0, 0, 0, 0];
        b.extend_from_slice(&self.width.to_be_bytes());
        b.extend_from_slice(&self.height.to_be_bytes());
        self.socket.write_all(&b).unwrap();
    }
    /// Reads one update into `pixels`.
    pub fn update(&mut self) {
        self.rects.clear();
        self.wire_bytes = 4;
        let header = exact(&mut self.socket, 4);
        assert_eq!(&header[..2], &[0, 0]);
        let count = u16::from_be_bytes(header[2..4].try_into().unwrap());
        for _ in 0..count {
            let r = exact(&mut self.socket, 12);
            self.wire_bytes += 12;
            let x = u16::from_be_bytes(r[..2].try_into().unwrap()) as usize;
            let y = u16::from_be_bytes(r[2..4].try_into().unwrap()) as usize;
            let w = u16::from_be_bytes(r[4..6].try_into().unwrap()) as usize;
            let h = u16::from_be_bytes(r[6..8].try_into().unwrap()) as usize;
            let encoding = i32::from_be_bytes(r[8..].try_into().unwrap());
            if encoding == FRAME_SEQUENCE {
                self.sequence = u64::from_be_bytes(exact(&mut self.socket, 8).try_into().unwrap());
                self.wire_bytes += 8;
                continue;
            }
            if encoding == -223 {
                self.width = w as u16;
                self.height = h as u16;
                self.pixels = vec![0; w * h * 4];
                continue;
            }
            self.encoding = encoding;
            self.rects.push((x, y, w, h));
            if encoding == 0 {
                self.wire_bytes += w * h * 4;
                for row in y..y + h {
                    let i = (row * self.width as usize + x) * 4;
                    self.pixels[i..i + w * 4].copy_from_slice(&exact(&mut self.socket, w * 4));
                }
                continue;
            }
            assert_eq!(encoding, 16);
            let n = u32::from_be_bytes(exact(&mut self.socket, 4).try_into().unwrap()) as usize;
            self.wire_bytes += 4 + n;
            let compressed = exact(&mut self.socket, n);
            let mut decoded =
                Vec::with_capacity(w * h * 3 + w.div_ceil(64) * h.div_ceil(64) + 1024);
            self.decompressor
                .decompress_vec(&compressed, &mut decoded, flate2::FlushDecompress::Sync)
                .unwrap();
            let mut offset = 0;
            for ty in (y..y + h).step_by(64) {
                for tx in (x..x + w).step_by(64) {
                    let kind = decoded[offset];
                    offset += 1;
                    // A solid tile is one pixel; a raw tile has them all.
                    if kind == 1 {
                        for row in ty..(ty + 64).min(y + h) {
                            for col in tx..(tx + 64).min(x + w) {
                                let i = (row * self.width as usize + col) * 4;
                                self.pixels[i..i + 3].copy_from_slice(&decoded[offset..offset + 3]);
                            }
                        }
                        offset += 3;
                        continue;
                    }
                    assert_eq!(kind, 0, "only solid and raw tiles are sent");
                    for row in ty..(ty + 64).min(y + h) {
                        for col in tx..(tx + 64).min(x + w) {
                            let i = (row * self.width as usize + col) * 4;
                            self.pixels[i..i + 3].copy_from_slice(&decoded[offset..offset + 3]);
                            offset += 3;
                        }
                    }
                }
            }
        }
    }
    /// The colour at a pixel, as red, green, blue.
    pub fn at(&self, x: usize, y: usize) -> [u8; 3] {
        let i = (y * self.width as usize + x) * 4;
        // The display's pixel format is blue, green, red in memory.
        [self.pixels[i + 2], self.pixels[i + 1], self.pixels[i]]
    }
    /// Waits for the pixel to show `color`.
    #[track_caller]
    pub fn expect(&mut self, x: usize, y: usize, color: [u8; 3]) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.request(false);
            self.update();
            if self.at(x, y) == color {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "at ({x}, {y}): {:?}, expected {color:?}",
                self.at(x, y)
            );
            thread::sleep(Duration::from_millis(16));
        }
    }
}

/// A DevTools session on a tab, framed like --remote-debugging-pipe.
pub struct Cdp {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next: u64,
    messages: Vec<Value>,
}
impl Cdp {
    fn connect(socket: &str) -> Self {
        let stream = UnixStream::connect(socket).unwrap();
        stream.set_read_timeout(Some(PATIENCE)).unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        Self {
            stream,
            reader,
            next: 0,
            messages: Vec::new(),
        }
    }
    fn read(&mut self) -> Value {
        let mut bytes = Vec::new();
        self.reader.read_until(0, &mut bytes).expect("CDP read");
        assert_eq!(bytes.pop(), Some(0), "the CDP session closed");
        serde_json::from_slice(&bytes).unwrap()
    }
    pub fn call_in(&mut self, session: Option<&str>, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = session.into();
        }
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(0);
        self.stream.write_all(&bytes).unwrap();
        if let Some(i) = self.messages.iter().position(|m| m["id"] == id) {
            return self.messages.remove(i);
        }
        loop {
            let message = self.read();
            if message["id"] == id {
                return message;
            }
            self.messages.push(message);
        }
    }
    /// Sends a command it won't wait for, such as one a dialog will hold up.
    pub fn send_without_answer(&self, method: &str, params: Value) {
        let mut bytes =
            serde_json::to_vec(&json!({"id": 1_000_000, "method": method, "params": params}))
                .unwrap();
        bytes.push(0);
        (&self.stream).write_all(&bytes).unwrap();
    }
    #[track_caller]
    pub fn eval_in(&mut self, session: Option<&str>, expression: &str, gesture: bool) -> Value {
        let response = self.call_in(
            session,
            "Runtime.evaluate",
            json!({"expression": expression, "awaitPromise": true, "returnByValue": true, "userGesture": gesture}),
        );
        let result = &response["result"];
        assert!(
            response.get("error").is_none() && result.get("exceptionDetails").is_none(),
            "{expression}: {response}"
        );
        result["result"]["value"].clone()
    }
    #[track_caller]
    pub fn eval(&mut self, expression: &str) -> Value {
        self.eval_in(None, expression, false)
    }
    /// Evaluates as if a person had just acted on the page.
    #[track_caller]
    pub fn gesture(&mut self, expression: &str) -> Value {
        self.eval_in(None, expression, true)
    }
    /// Waits until `expression` evaluates to `expected`.
    #[track_caller]
    pub fn until_in(&mut self, session: Option<&str>, expression: &str, expected: Value) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let value = self.eval_in(session, expression, false);
            if value == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{expression}: {value}, expected {expected}"
            );
            thread::sleep(Duration::from_millis(16));
        }
    }
    #[track_caller]
    pub fn until(&mut self, expression: &str, expected: Value) {
        self.until_in(None, expression, expected)
    }
    /// Reports iframes with their own targets from now on.
    pub fn attach_frames(&mut self) {
        let response = self.call_in(
            None,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true}),
        );
        assert!(response.get("error").is_none(), "{response}");
    }
    /// The session of the iframe whose document is at `url`, once it has loaded.
    #[track_caller]
    pub fn frame_session(&mut self, url: &str) -> String {
        let expected = json!(url);
        let deadline = Instant::now() + PATIENCE;
        let mut seen = 0;
        loop {
            let targets: Vec<String> = self
                .messages
                .iter()
                .filter(|m| {
                    m["method"] == "Target.attachedToTarget"
                        && m["params"]["targetInfo"]["type"] == "iframe"
                })
                .map(|m| m["params"]["sessionId"].as_str().unwrap().to_owned())
                .collect();
            for session in &targets {
                let response = self.call_in(
                    Some(session),
                    "Runtime.evaluate",
                    json!({"expression": "document.readyState == 'complete' ? location.href : ''", "returnByValue": true}),
                );
                if response["result"]["result"]["value"] == expected {
                    return session.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "no iframe at {url} among {targets:?}"
            );
            if targets.len() == seen {
                self.stream
                    .set_read_timeout(Some(Duration::from_millis(50)))
                    .unwrap();
                let mut bytes = Vec::new();
                if self.reader.read_until(0, &mut bytes).is_ok() && bytes.pop() == Some(0) {
                    self.messages.push(serde_json::from_slice(&bytes).unwrap());
                }
                self.stream.set_read_timeout(Some(PATIENCE)).unwrap();
            }
            seen = targets.len();
        }
    }
}

/// `[x, y, width, height]` as a page's expression measures it, compared with what the host reports.
#[track_caller]
pub fn assert_near(reported: &Rect, page: &Value) {
    let page: Vec<f64> = page
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect();
    let got = [reported.x, reported.y, reported.width, reported.height];
    for (a, b) in got.iter().zip(&page) {
        assert!(
            (f64::from(*a) - b).abs() <= 1.0,
            "reported {got:?}, the page measures {page:?}"
        );
    }
}
