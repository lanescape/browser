//! The `recording` capability: one VP8 stream per tab, any number of tabs at once up to a limit.
//! A recording's bytes go over its own owner-private socket as an IVF stream, which needs no framing
//! of its own and which ffmpeg and others read as it is. Its reader is whoever connects first. A
//! reader that falls behind loses frames rather than holding up the page or growing the queue: the
//! queue is cut and restarts at a forced key frame, so what it does get still decodes.
use crate::{event, post};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Write},
    os::unix::{
        fs::PermissionsExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub const TOKEN: &str = "recording";
/// Recordings at once, across the host. Each is an encoder, and takes a share of a core.
const LIMIT: usize = 32;
/// Encoded bytes waiting for a reader; about half a minute of a busy page.
const QUEUE: usize = 8 << 20;
/// How long a reader has to connect.
const CONNECT: Duration = Duration::from_secs(15);
/// How long a stopped recording's reader has to take what's left, or the engine to finish.
const DRAIN: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Reason {
    Stopped = 1,
    TabClosed = 2,
    ConnectTimeout = 3,
    ReaderClosed = 4,
    Shutdown = 5,
}

/// A recording's frames on their way to its reader.
#[derive(Default)]
struct Queue {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    /// The stream's header has been queued.
    started: bool,
    /// Frames were lost: nothing until a key frame would decode.
    awaiting_key: bool,
    /// Its reader has been given the stream's header.
    header_taken: bool,
    /// Frames its reader has had, whole.
    written: u64,
    dropped: u64,
    /// The engine has sent its last frame.
    finished: bool,
    /// Why it's ending, once it is.
    reason: Option<Reason>,
    /// When it should give up on its reader or the engine.
    drain_by: Option<Instant>,
}
impl Queue {
    /// Takes a frame; true when the queue was cut and a key frame is needed.
    fn push(&mut self, data: &[u8], pts: i64, key: bool, width: u32, height: u32) -> bool {
        if !self.started {
            self.started = true;
            self.bytes += 32;
            self.frames.push_back(header(width, height));
        }
        if key {
            self.awaiting_key = false;
        } else if self.awaiting_key {
            self.dropped += 1;
            return false;
        }
        let size = data.len() + 12;
        if self.bytes + size > QUEUE {
            // The first entry may be the stream's header, which a reader can't do without.
            let keep = usize::from(!self.header_taken);
            let lost = self.frames.len() - keep;
            self.dropped += lost as u64;
            self.frames.truncate(keep);
            self.bytes = self.frames.iter().map(Vec::len).sum();
            if !key {
                self.dropped += 1;
                self.awaiting_key = true;
                return true;
            }
        }
        let mut frame = Vec::with_capacity(size);
        frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(pts as u64).to_le_bytes());
        frame.extend_from_slice(data);
        self.bytes += size;
        self.frames.push_back(frame);
        false
    }
}

/// An IVF header: its frame count isn't known in a stream, and is zero.
fn header(width: u32, height: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.extend_from_slice(b"DKIF");
    b.extend_from_slice(&0u16.to_le_bytes());
    b.extend_from_slice(&32u16.to_le_bytes());
    b.extend_from_slice(b"VP80");
    b.extend_from_slice(&(width as u16).to_le_bytes());
    b.extend_from_slice(&(height as u16).to_le_bytes());
    // Timestamps are in milliseconds.
    b.extend_from_slice(&1000u32.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

/// Recording threads still going, up to their last event.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

struct Recording {
    id: u64,
    tab: u64,
    queue: Mutex<Queue>,
    changed: Condvar,
}
impl Recording {
    /// Starts ending it, unless it already is.
    fn end(&self, reason: Reason) -> bool {
        let mut queue = self.queue.lock().unwrap();
        if queue.reason.is_some() {
            return false;
        }
        queue.reason = Some(reason);
        queue.drain_by = Some(Instant::now() + DRAIN);
        self.changed.notify_all();
        true
    }
}

fn live() -> &'static Mutex<HashMap<u64, Arc<Recording>>> {
    static LIVE: OnceLock<Mutex<HashMap<u64, Arc<Recording>>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// A frame from the engine, on one of its threads; `data` of None means it has sent its last.
pub fn frame(recording: u64, data: Option<&[u8]>, pts: i64, key: bool, width: u32, height: u32) {
    let Some(r) = live().lock().unwrap().get(&recording).cloned() else {
        return;
    };
    let mut queue = r.queue.lock().unwrap();
    match data {
        Some(data) => {
            if queue.push(data, pts, key, width, height) {
                post(json!({"op":"record_key_frame","tab":r.tab}));
            }
        }
        None => queue.finished = true,
    }
    r.changed.notify_all();
}

#[derive(Default)]
pub struct Recordings {
    next: u64,
}
impl Recordings {
    /// StartRecording.
    pub fn start(&mut self, runtime: &Path, tab: u64) -> Result<Value, String> {
        let mut recordings = live().lock().unwrap();
        if recordings.values().any(|r| r.tab == tab) {
            return Err("tab is already recording".into());
        }
        if recordings.len() >= LIMIT {
            return Err("LIMIT: too many recordings".into());
        }
        self.next += 1;
        let id = self.next;
        let socket = runtime.join(format!("recording-{id}.sock"));
        let listener = UnixListener::bind(&socket).map_err(|e| e.to_string())?;
        let ready = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .and_then(|()| listener.set_nonblocking(true));
        if let Err(e) = ready {
            let _ = std::fs::remove_file(&socket);
            return Err(e.to_string());
        }
        let recording = Arc::new(Recording {
            id,
            tab,
            queue: Mutex::default(),
            changed: Condvar::new(),
        });
        recordings.insert(id, recording.clone());
        RUNNING.fetch_add(1, Ordering::AcqRel);
        post(json!({"op":"record_start","tab":tab,"recording":id}));
        let path = socket.clone();
        thread::spawn(move || run(recording, listener, path));
        Ok(json!({"tab":tab,"recording":id,"socket":socket.to_string_lossy()}))
    }
    /// StopRecording.
    pub fn stop(&mut self, tab: u64) -> Result<Value, String> {
        let recording = live()
            .lock()
            .unwrap()
            .values()
            .find(|r| r.tab == tab)
            .cloned();
        match recording {
            Some(r) if r.end(Reason::Stopped) => {
                post(json!({"op":"record_stop","tab":tab}));
                Ok(json!({}))
            }
            _ => Err("tab is not recording".into()),
        }
    }
    pub fn closed_tab(&mut self, tab: u64) {
        let recording = live()
            .lock()
            .unwrap()
            .values()
            .find(|r| r.tab == tab)
            .cloned();
        if let Some(r) = recording {
            r.end(Reason::TabClosed);
        }
    }
    /// EOF: the engine finishes each recording as its tab goes.
    pub fn shutdown(&mut self) {
        for r in live().lock().unwrap().values() {
            r.end(Reason::Shutdown);
        }
    }
}

/// Once Chromium has shut down, waits for what's left to reach its readers.
pub fn finish() {
    let deadline = Instant::now() + DRAIN + Duration::from_secs(1);
    for r in live().lock().unwrap().values() {
        r.end(Reason::Shutdown);
        r.queue.lock().unwrap().finished = true;
        r.changed.notify_all();
    }
    while RUNNING.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
}

/// The recording's whole life from here: its reader connects, takes what comes, and it ends.
fn run(r: Arc<Recording>, listener: UnixListener, path: PathBuf) {
    let reader = connect(&r, &listener);
    // One reader for each recording: the path disappears once it's taken.
    let _ = std::fs::remove_file(&path);
    drop(listener);
    if let Some(mut reader) = reader
        && let Err(reason) = deliver(&r, &mut reader)
        && r.end(reason)
    {
        post(json!({"op":"record_stop","tab":r.tab}));
    }
    let (reason, written, dropped) = {
        let queue = r.queue.lock().unwrap();
        (
            queue.reason.unwrap_or(Reason::Stopped),
            queue.written,
            queue.dropped,
        )
    };
    live().lock().unwrap().remove(&r.id);
    event(
        json!({"method":"recording.ended","params":{"tab":r.tab,"recording":r.id,"reason":reason as i32,"frames":written,"dropped":dropped}}),
    );
    RUNNING.fetch_sub(1, Ordering::AcqRel);
}

fn connect(r: &Recording, listener: &UnixListener) -> Option<UnixStream> {
    let deadline = Instant::now() + CONNECT;
    loop {
        match listener.accept() {
            Ok((reader, _)) => return Some(reader),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if r.queue.lock().unwrap().reason.is_some() {
            return None;
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if r.end(Reason::ConnectTimeout) {
        post(json!({"op":"record_stop","tab":r.tab}));
    }
    None
}

/// Writes frames as they come until the recording has ended and its reader has the last of them.
/// Err when the reader has gone.
fn deliver(r: &Recording, reader: &mut UnixStream) -> Result<(), Reason> {
    // Short, so a stalled reader doesn't keep a stopped recording here past its bound.
    reader
        .set_write_timeout(Some(Duration::from_millis(100)))
        .map_err(|_| Reason::ReaderClosed)?;
    loop {
        let next = {
            let mut queue = r.queue.lock().unwrap();
            loop {
                if queue.drain_by.is_some_and(|t| Instant::now() >= t) {
                    break None;
                }
                if let Some(frame) = queue.frames.pop_front() {
                    queue.bytes -= frame.len();
                    let is_header = !queue.header_taken;
                    queue.header_taken = true;
                    break Some((frame, is_header));
                }
                if queue.reason.is_some() && queue.finished {
                    break None;
                }
                // A still page writes nothing, so nothing else would notice a reader going away.
                if hung_up(reader) {
                    return Err(Reason::ReaderClosed);
                }
                queue = r
                    .changed
                    .wait_timeout(queue, Duration::from_millis(250))
                    .unwrap()
                    .0;
            }
        };
        let Some((frame, is_header)) = next else {
            return Ok(());
        };
        let mut sent = 0;
        while sent < frame.len() {
            match reader.write(&frame[sent..]) {
                Ok(0) => return Err(Reason::ReaderClosed),
                Ok(n) => {
                    sent += n;
                    // Slow progress is no more of a reason to go past the bound than none.
                    if sent < frame.len()
                        && r.queue
                            .lock()
                            .unwrap()
                            .drain_by
                            .is_some_and(|t| Instant::now() >= t)
                    {
                        return Ok(());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    // A reader taking its time is its own business until the recording has ended.
                    let queue = r.queue.lock().unwrap();
                    if queue.drain_by.is_some_and(|t| Instant::now() >= t) {
                        return Ok(());
                    }
                }
                Err(_) => return Err(Reason::ReaderClosed),
            }
        }
        if !is_header {
            r.queue.lock().unwrap().written += 1;
        }
        let _ = reader.flush();
    }
}

fn hung_up(reader: &UnixStream) -> bool {
    // macOS reports a hang-up only to a poll asking for something; Linux reports it regardless.
    let mut poll = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd; a timeout of zero doesn't wait.
    unsafe {
        libc::poll(&mut poll, 1, 0) > 0 && poll.revents & (libc::POLLHUP | libc::POLLERR) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reader_hanging_up_is_seen() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        assert!(!hung_up(&ours));
        drop(theirs);
        assert!(hung_up(&ours));
    }

    #[test]
    fn the_stream_starts_with_its_header() {
        let mut queue = Queue::default();
        assert!(!queue.push(&[1, 2, 3], 7, true, 640, 360));
        let header = &queue.frames[0];
        assert_eq!(header.len(), 32);
        assert_eq!(&header[..4], b"DKIF");
        assert_eq!(&header[8..12], b"VP80");
        assert_eq!(u16::from_le_bytes([header[12], header[13]]), 640);
        assert_eq!(u16::from_le_bytes([header[14], header[15]]), 360);
        let frame = &queue.frames[1];
        assert_eq!(&frame[..4], &3u32.to_le_bytes());
        assert_eq!(&frame[4..12], &7u64.to_le_bytes());
        assert_eq!(&frame[12..], &[1, 2, 3]);
    }

    #[test]
    fn a_reader_falling_behind_loses_frames_up_to_a_key_frame() {
        let mut queue = Queue::default();
        let chunk = vec![0; 1 << 20];
        assert!(!queue.push(&chunk, 0, true, 10, 10));
        let mut pts = 1;
        let mut asked = false;
        while !asked {
            asked = queue.push(&chunk, pts, false, 10, 10);
            pts += 1;
            assert!(queue.bytes <= QUEUE + 32, "{} bytes queued", queue.bytes);
        }
        // The queue was cut, keeping only the header no reader has had.
        assert!(queue.awaiting_key);
        assert_eq!(queue.frames.len(), 1);
        assert_eq!(queue.frames[0].len(), 32);
        let dropped = queue.dropped;
        assert!(dropped >= 7);
        // Deltas now would not decode.
        assert!(!queue.push(&chunk, pts, false, 10, 10));
        assert_eq!(queue.frames.len(), 1);
        assert_eq!(queue.dropped, dropped + 1);
        // A key frame starts it again.
        assert!(!queue.push(&chunk, pts + 1, true, 10, 10));
        assert!(!queue.awaiting_key);
        assert_eq!(queue.frames.len(), 2);
        assert!(!queue.push(&chunk, pts + 2, false, 10, 10));
        assert_eq!(queue.frames.len(), 3);
    }

    #[test]
    fn a_reader_that_has_the_header_does_not_get_it_again() {
        let mut queue = Queue::default();
        let chunk = vec![0; 1 << 20];
        queue.push(&chunk, 0, true, 10, 10);
        // The reader takes the header and the first frame.
        queue.frames.pop_front();
        queue.frames.pop_front();
        queue.bytes = 0;
        queue.header_taken = true;
        let mut pts = 1;
        while !queue.push(&chunk, pts, false, 10, 10) {
            pts += 1;
        }
        assert!(queue.frames.is_empty());
    }
}
