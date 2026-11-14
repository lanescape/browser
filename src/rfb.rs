//! RFB 3.8 over an owner-private Unix socket; one bounded framebuffer per tab. Chromium says which
//! part of each captured frame changed, so a frame copies only that part, and a viewer learns what
//! changed since it last looked without comparing any pixels. A viewer sleeps until a frame comes.
use flate2::{Compress, Compression, FlushCompress};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    io::{self, Read, Write},
    os::unix::{
        io::AsRawFd,
        net::{UnixListener, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
#[derive(Clone)]
pub struct Frame {
    pub width: u16,
    pub height: u16,
    pub pixels: Arc<Vec<u8>>,
    pub sequence: u64,
}
pub struct Display {
    pub frame: Mutex<Option<Frame>>,
    /// The sequence of the latest frame holding a new view paint. Popup paints reuse the last one.
    pub view: AtomicU64,
    pub closed: AtomicBool,
    pub controller: AtomicU64,
    pub viewers: Mutex<std::collections::HashSet<u64>>,
    /// A handle on each viewer's connection, to close it from another thread.
    pub sockets: Mutex<std::collections::HashMap<u64, UnixStream>>,
    /// What each recent frame changed. Written with `frame` held, so it's never behind it.
    changes: Mutex<VecDeque<(u64, Option<Region>)>>,
    /// Wakes each viewer's thread for a new frame.
    wakers: Mutex<HashMap<u64, UnixStream>>,
}
/// Frames of history for viewers: one further behind than this gets its whole request.
const CHANGES: usize = 64;
/// A captured frame, valid for the call that hands it over.
pub struct Capture {
    pub width: u16,
    pub height: u16,
    pub stride: usize,
    pub pixels: *const u8,
    /// What changed since the last frame published; None for all of it.
    pub update: Option<(u16, u16, u16, u16)>,
    /// The bytes are R, G, B, A rather than B, G, R, A.
    pub swapped: bool,
}
impl Capture {
    /// Copies a rectangle of the capture into a frame's pixels.
    fn copy(&self, r: Region, width: u16, out: &mut [u8]) {
        for y in r.y as usize..r.y as usize + r.h as usize {
            let w = r.w as usize * 4;
            let from = y * self.stride + r.x as usize * 4;
            let to = (y * width as usize + r.x as usize) * 4;
            // SAFETY: the engine's stride and size bound every row inside the mapped frame, and
            // the rectangle lies inside the frame.
            let source = unsafe { std::slice::from_raw_parts(self.pixels.add(from), w) };
            let row = &mut out[to..to + w];
            row.copy_from_slice(source);
            if self.swapped {
                for pixel in row.as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
            }
        }
    }
    /// The part of `r` that really differs from `old`, which Chromium rounds out; None if nothing
    /// does. The cost follows the size of `r`, not of the frame.
    fn tighten(&self, r: Region, width: u16, old: &[u8]) -> Option<Region> {
        let row_differs = |y: usize, x0: usize, x1: usize| {
            let (from, to) = (y * self.stride + x0 * 4, (y * width as usize + x0) * 4);
            let n = (x1 - x0) * 4;
            // SAFETY: as in `copy`.
            let source = unsafe { std::slice::from_raw_parts(self.pixels.add(from), n) };
            source != &old[to..to + n]
        };
        let (left, right) = (r.x as usize, r.x as usize + r.w as usize);
        let (top, bottom) = (r.y as usize, r.y as usize + r.h as usize);
        let first = (top..bottom).find(|y| row_differs(*y, left, right))?;
        let last = (first..bottom)
            .rev()
            .find(|y| row_differs(*y, left, right))?;
        let column_differs = |x: usize| (first..=last).any(|y| row_differs(y, x, x + 1));
        let x0 = (left..right).find(|x| column_differs(*x))?;
        let x1 = (x0..right).rev().find(|x| column_differs(*x))? + 1;
        Some(Region {
            x: x0 as u16,
            y: first as u16,
            w: (x1 - x0) as u16,
            h: (last + 1 - first) as u16,
        })
    }
    /// A frame holding the whole capture.
    pub fn frame(&self, sequence: u64) -> Frame {
        let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
        let all = Region {
            x: 0,
            y: 0,
            w: self.width,
            h: self.height,
        };
        self.copy(all, self.width, &mut pixels);
        Frame {
            width: self.width,
            height: self.height,
            pixels: Arc::new(pixels),
            sequence,
        }
    }
}
impl Display {
    /// Makes a capture the latest frame. Only what changed is copied, unless a viewer is reading the
    /// last frame right now, which then keeps it while this one goes elsewhere.
    pub fn publish(&self, capture: &Capture) {
        let mut latest = self.frame.lock().unwrap();
        let sequence = latest.as_ref().map_or(1, |f| f.sequence + 1);
        let same_size = latest
            .as_ref()
            .is_some_and(|f| f.width == capture.width && f.height == capture.height);
        let update = capture
            .update
            .filter(|_| same_size)
            .map(|(x, y, w, h)| Region { x, y, w, h });
        // What changed: tighter than Chromium's rectangle, and None when nothing did.
        let changed = match (update, latest.as_ref()) {
            (Some(r), _) if r.w == 0 || r.h == 0 => None,
            (Some(r), Some(f)) if !capture.swapped => capture.tighten(r, f.width, &f.pixels),
            (r, _) => r,
        };
        match latest.as_mut() {
            Some(f) if update.is_some() => {
                let width = f.width;
                if let Some(pixels) = Arc::get_mut(&mut f.pixels) {
                    if let Some(r) = changed {
                        capture.copy(r, width, pixels);
                    }
                    f.sequence = sequence;
                } else {
                    *f = capture.frame(sequence);
                }
            }
            _ => *latest = Some(capture.frame(sequence)),
        }
        let whole = Region::full(latest.as_ref().unwrap());
        let mut changes = self.changes.lock().unwrap();
        if changes.len() == CHANGES {
            changes.pop_front();
        }
        changes.push_back((
            sequence,
            if update.is_some() {
                changed
            } else {
                Some(whole)
            },
        ));
        drop(changes);
        drop(latest);
        self.view.store(sequence, Ordering::Release);
        self.wake();
    }
    fn wake(&self) {
        for waker in self.wakers.lock().unwrap().values() {
            // A full pipe means its viewer is already due to wake.
            let _ = (&*waker).write(&[0]);
        }
    }
    /// What changed after `known` up to `sequence`: Some(None) for nothing, and None when that's
    /// too long ago to say.
    fn changed_since(&self, known: u64, sequence: u64) -> Option<Option<Region>> {
        let changes = self.changes.lock().unwrap();
        // The oldest entry has to be the frame right after `known` or earlier.
        if changes.front().is_none_or(|(s, _)| *s > known + 1) {
            return None;
        }
        Some(
            changes
                .iter()
                .filter(|(s, _)| *s > known && *s <= sequence)
                .filter_map(|(_, r)| *r)
                .reduce(Region::union),
        )
    }

    /// Closes a viewer's connection; its thread then ends it as usual, with internal.release.
    pub fn disconnect(&self, viewer: u64) {
        if let Some(s) = self.sockets.lock().unwrap().get(&viewer) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}
pub type Shared = Arc<Display>;
/// Pseudo-encoding numbering each update; see specs/0003-display.md.
const FRAME_SEQUENCE: i32 = 0x4C53_4653;
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid RFB message")
}
fn bytes<const N: usize>(s: &mut UnixStream) -> io::Result<[u8; N]> {
    let mut b = [0; N];
    s.read_exact(&mut b)?;
    Ok(b)
}
fn event(v: Value) {
    crate::event(v)
}
pub fn listen(
    path: PathBuf,
    tab: u64,
    commands: std::sync::mpsc::SyncSender<Value>,
) -> io::Result<Shared> {
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let display = Arc::new(Display {
        frame: Mutex::new(None),
        view: AtomicU64::new(0),
        closed: AtomicBool::new(false),
        controller: AtomicU64::new(0),
        viewers: Mutex::new(std::collections::HashSet::new()),
        sockets: Mutex::default(),
        changes: Mutex::default(),
        wakers: Mutex::default(),
    });
    let d = display.clone();
    thread::spawn(move || {
        let mut next = 1;
        while !d.closed.load(Ordering::Acquire) {
            let mut poll = libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd. The timeout is only to notice the display closing.
            if unsafe { libc::poll(&mut poll, 1, 250) } == 0 {
                continue;
            }
            match listener.accept() {
                Ok((s, _)) => {
                    let id = next;
                    if d.viewers.lock().unwrap().len() >= 8 {
                        continue;
                    }
                    let Ok((wake, waker)) = UnixStream::pair() else {
                        continue;
                    };
                    if wake.set_nonblocking(true).is_err() || waker.set_nonblocking(true).is_err() {
                        continue;
                    }
                    // Every registered viewer must be closable from the main thread, which
                    // disconnects one that floods a tab; without a handle the connection is
                    // refused, closed here by dropping it. (Only descriptor exhaustion makes
                    // try_clone fail, which no unit test can cheaply arrange.)
                    let Ok(handle) = s.try_clone() else {
                        continue;
                    };
                    d.sockets.lock().unwrap().insert(id, handle);
                    d.wakers.lock().unwrap().insert(id, waker);
                    {
                        // Posted under the lock, so a viewer leaving as another arrives can't
                        // leave the tab believing nobody is looking.
                        let mut viewers = d.viewers.lock().unwrap();
                        viewers.insert(id);
                        if viewers.len() == 1 {
                            crate::post(json!({"op":"viewed","tab":tab,"viewed":true}));
                        }
                    }
                    next += 1;
                    let d = d.clone();
                    let tx = commands.clone();
                    thread::spawn(move || {
                        event(
                            json!({"method":"viewer.connected","params":{"tab":tab,"viewer":id}}),
                        );
                        if let Err(error) = serve(s, &d, tab, id, &tx, wake)
                            && !matches!(
                                error.kind(),
                                io::ErrorKind::UnexpectedEof
                                    | io::ErrorKind::ConnectionReset
                                    | io::ErrorKind::BrokenPipe
                            )
                        {
                            eprintln!("RFB tab {tab} viewer {id}: {error}");
                        }
                        {
                            let mut viewers = d.viewers.lock().unwrap();
                            viewers.remove(&id);
                            if viewers.is_empty() {
                                crate::post(json!({"op":"viewed","tab":tab,"viewed":false}));
                            }
                        }
                        d.sockets.lock().unwrap().remove(&id);
                        d.wakers.lock().unwrap().remove(&id);
                        let _ = d.controller.compare_exchange(
                            id,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        let _ = tx.send(
                            json!({"method":"internal.release","params":{"tab":tab,"viewer":id}}),
                        );
                        event(
                            json!({"method":"viewer.disconnected","params":{"tab":tab,"viewer":id}}),
                        );
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_file(path);
    });
    Ok(display)
}
#[derive(Clone, Copy)]
struct PixelFormat {
    bpp: u8,
    depth: u8,
    big: bool,
    max: [u16; 3],
    shift: [u8; 3],
}
impl Default for PixelFormat {
    fn default() -> Self {
        Self {
            bpp: 32,
            depth: 24,
            big: false,
            max: [255; 3],
            shift: [16, 8, 0],
        }
    }
}
impl PixelFormat {
    /// Whether frames' own bytes are already this format, as with the format offered by default.
    fn is_native(&self) -> bool {
        self.bpp == 32
            && self.depth <= 24
            && !self.big
            && self.max == [255; 3]
            && self.shift == [16, 8, 0]
    }
    fn parse(b: &[u8]) -> io::Result<Self> {
        if b.len() != 16
            || ![8, 16, 32].contains(&b[0])
            || b[1] == 0
            || b[1] > b[0]
            || b[2] > 1
            || b[3] != 1
        {
            return Err(invalid());
        }
        let format = Self {
            bpp: b[0],
            depth: b[1],
            big: b[2] != 0,
            max: [
                u16::from_be_bytes([b[4], b[5]]),
                u16::from_be_bytes([b[6], b[7]]),
                u16::from_be_bytes([b[8], b[9]]),
            ],
            shift: [b[10], b[11], b[12]],
        };
        let mut used = 0u64;
        for (max, shift) in format.max.into_iter().zip(format.shift) {
            if max == 0 || !(max as u32 + 1).is_power_of_two() || shift >= format.bpp {
                return Err(invalid());
            }
            let mask = (max as u64) << shift;
            if mask >> format.bpp != 0 || used & mask != 0 {
                return Err(invalid());
            }
            used |= mask;
        }
        if used.count_ones() > b[1] as u32 {
            return Err(invalid());
        }
        Ok(format)
    }
    fn pixel(self, bgra: &[u8], compact: bool, out: &mut Vec<u8>) {
        let mut value = 0u32;
        for ((channel, max), shift) in [bgra[2], bgra[1], bgra[0]]
            .into_iter()
            .zip(self.max)
            .zip(self.shift)
        {
            value |= ((channel as u32 * max as u32 + 127) / 255) << shift;
        }
        let data = if self.big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        let mut start = if self.big {
            4 - self.bpp as usize / 8
        } else {
            0
        };
        let mut end = if self.big { 4 } else { self.bpp as usize / 8 };
        // RFC 6143 7.7.6: a CPIXEL drops a byte only at a depth of 24 or less.
        if compact && self.bpp == 32 && self.depth <= 24 {
            let mask = self
                .max
                .into_iter()
                .zip(self.shift)
                .fold(0u32, |a, (m, s)| a | ((m as u32) << s));
            if mask & 0xff000000 == 0 {
                if self.big { start += 1 } else { end -= 1 }
            } else if mask & 0x000000ff == 0 {
                if self.big { end -= 1 } else { start += 1 }
            }
        }
        out.extend_from_slice(&data[start..end]);
    }
}
fn serve(
    mut s: UnixStream,
    d: &Display,
    tab: u64,
    id: u64,
    tx: &std::sync::mpsc::SyncSender<Value>,
    mut wake: UnixStream,
) -> io::Result<()> {
    s.set_nonblocking(false)?;
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    s.write_all(b"RFB 003.008\n")?;
    if bytes::<12>(&mut s)? != *b"RFB 003.008\n" {
        return Err(invalid());
    }
    s.write_all(&[1, 1])?;
    if bytes::<1>(&mut s)?[0] != 1 {
        return Err(invalid());
    }
    s.write_all(&[0; 4])?;
    bytes::<1>(&mut s)?;
    let first = loop {
        if d.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        if let Some(f) = d.frame.lock().unwrap().clone() {
            break f;
        }
        thread::sleep(Duration::from_millis(10))
    };
    s.write_all(&first.width.to_be_bytes())?;
    s.write_all(&first.height.to_be_bytes())?;
    s.write_all(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0])?;
    const NAME: &[u8] = b"Lanescape browser";
    s.write_all(&(NAME.len() as u32).to_be_bytes())?;
    s.write_all(NAME)?;
    let mut format = PixelFormat::default();
    let mut zrle = false;
    let mut resize = false;
    let mut numbered = false;
    let mut compressor = Compress::new(Compression::fast(), true);
    // The size the viewer has, and the frame it's up to date with; zero when it isn't known to be.
    let mut size = (first.width, first.height);
    let mut known = 0u64;
    drop(first);
    let mut pending: Option<[u8; 9]> = None;
    // The frame and request last found to need nothing, so it's looked at only once.
    let mut scanned: Option<(u64, [u8; 9])> = None;
    while !d.closed.load(Ordering::Acquire) {
        // Keep accepting input while an incremental update waits for paint.
        if let Some(req) = pending {
            let snapshot = d.frame.lock().unwrap().clone();
            // A request outside the frame is answered even when the viewer is up to date.
            if let Some(f) = snapshot
                && let requested = Region::request(req, &f)?
                && (req[0] == 0 || requested.is_none() || f.sequence != known)
                && scanned != Some((f.sequence, req))
            {
                let changed = (f.width, f.height) != size;
                if changed && !resize {
                    return Err(invalid());
                }
                let region = match requested {
                    Some(r) if req[0] != 0 && known != 0 && !changed => {
                        match d.changed_since(known, f.sequence) {
                            Some(region) => region.and_then(|c| c.intersect(r)),
                            None => Some(r),
                        }
                    }
                    other => other,
                };
                // A request outside the frame, as one sent before it shrank, has nothing to draw
                // and is still answered.
                if region.is_some() || requested.is_none() {
                    let rectangles =
                        u16::from(region.is_some()) + u16::from(changed) + u16::from(numbered);
                    s.write_all(&[0, 0])?;
                    s.write_all(&rectangles.to_be_bytes())?;
                    if numbered {
                        s.write_all(&[0; 8])?;
                        s.write_all(&FRAME_SEQUENCE.to_be_bytes())?;
                        s.write_all(&f.sequence.to_be_bytes())?;
                    }
                    if changed {
                        s.write_all(&[0; 4])?;
                        s.write_all(&f.width.to_be_bytes())?;
                        s.write_all(&f.height.to_be_bytes())?;
                        s.write_all(&(-223i32).to_be_bytes())?;
                    }
                    if let Some(region) = region {
                        for v in [region.x, region.y, region.w, region.h] {
                            s.write_all(&v.to_be_bytes())?;
                        }
                        s.write_all(&(if zrle { 16i32 } else { 0 }).to_be_bytes())?;
                        if zrle {
                            let input = tiles(&f, region, format);
                            let mut output =
                                Vec::with_capacity(input.len() + input.len() / 10 + 1024);
                            let before = compressor.total_in();
                            compressor
                                .compress_vec(&input, &mut output, FlushCompress::Sync)
                                .map_err(|_| invalid())?;
                            if compressor.total_in() - before != input.len() as u64 {
                                return Err(invalid());
                            }
                            s.write_all(&(output.len() as u32).to_be_bytes())?;
                            s.write_all(&output)?;
                        } else {
                            if format.is_native() {
                                raw_rows(&mut s, &f, region)?;
                            } else {
                                for y in region.y..region.y + region.h {
                                    let i = (y as usize * f.width as usize + region.x as usize) * 4;
                                    let mut row = Vec::with_capacity(region.w as usize * 4);
                                    for pixel in
                                        f.pixels[i..i + region.w as usize * 4].as_chunks::<4>().0
                                    {
                                        format.pixel(pixel, false, &mut row);
                                    }
                                    s.write_all(&row)?;
                                }
                            }
                        }
                    }
                    // A partial request must not acknowledge unseen changes elsewhere.
                    size = (f.width, f.height);
                    known = if requested == Some(Region::full(&f)) {
                        f.sequence
                    } else {
                        0
                    };
                    pending = None;
                } else {
                    scanned = Some((f.sequence, req));
                }
            }
        }
        let mut polls = [s.as_raw_fd(), wake.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: two valid pollfds. The timeout is only to notice the display closing.
        let ready = unsafe { libc::poll(polls.as_mut_ptr(), 2, 250) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if polls[1].revents != 0 {
            let mut drained = [0; 64];
            let _ = wake.read(&mut drained);
        }
        if polls[0].revents == 0 {
            continue;
        }
        match bytes::<1>(&mut s)?[0] {
            0 => {
                let f = bytes::<19>(&mut s)?;
                format = PixelFormat::parse(&f[3..])?;
                known = 0;
                scanned = None;
            }
            2 => {
                let b = bytes::<3>(&mut s)?;
                let n = u16::from_be_bytes([b[1], b[2]]);
                if n > 64 {
                    return Err(invalid());
                }
                zrle = false;
                resize = false;
                numbered = false;
                for _ in 0..n {
                    match i32::from_be_bytes(bytes::<4>(&mut s)?) {
                        16 => zrle = true,
                        -223 => resize = true,
                        FRAME_SEQUENCE => numbered = true,
                        _ => (),
                    }
                }
            }
            3 => {
                let req = bytes::<9>(&mut s)?;
                pending = Some(if let Some(previous) = pending {
                    merge_requests(previous, req)
                } else {
                    req
                });
            }
            4 => {
                let b = bytes::<7>(&mut s)?;
                // Every message counts toward request ordering; only input received from the
                // controller may reach the page.
                tx.send(json!({"method":"internal.key","params":{"tab":tab,"viewer":id,"authorized":d.controller.load(Ordering::Acquire)==id,"down":b[0]!=0,"key":u32::from_be_bytes(b[3..7].try_into().unwrap())}})).map_err(|_|invalid())?
            }
            5 => {
                let b = bytes::<5>(&mut s)?;
                tx.send(json!({"method":"internal.pointer","params":{"tab":tab,"viewer":id,"authorized":d.controller.load(Ordering::Acquire)==id,"buttons":b[0],"x":u16::from_be_bytes([b[1],b[2]]),"y":u16::from_be_bytes([b[3],b[4]])}})).map_err(|_|invalid())?
            }
            6 => {
                let b = bytes::<7>(&mut s)?;
                let n = u32::from_be_bytes(b[3..7].try_into().unwrap());
                if n > 65536 {
                    return Err(invalid());
                }
                let mut discard = vec![0; n as usize];
                s.read_exact(&mut discard)?;
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}
fn merge_requests(a: [u8; 9], b: [u8; 9]) -> [u8; 9] {
    let values = |r: [u8; 9]| -> (u32, u32, u32, u32) {
        (
            u16::from_be_bytes([r[1], r[2]]) as u32,
            u16::from_be_bytes([r[3], r[4]]) as u32,
            u16::from_be_bytes([r[5], r[6]]) as u32,
            u16::from_be_bytes([r[7], r[8]]) as u32,
        )
    };
    let (ax, ay, aw, ah) = values(a);
    let (bx, by, bw, bh) = values(b);
    let x = ax.min(bx);
    let y = ay.min(by);
    let w = (ax + aw).max(bx + bw) - x;
    let h = (ay + ah).max(by + bh) - y;
    let mut r = [0; 9];
    r[0] = u8::from(a[0] != 0 && b[0] != 0);
    for (i, n) in [x, y, w.min(65535), h.min(65535)].into_iter().enumerate() {
        r[1 + i * 2..3 + i * 2].copy_from_slice(&(n as u16).to_be_bytes());
    }
    r
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Region {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}
impl Region {
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        Self {
            x,
            y,
            w: (self.x + self.w).max(other.x + other.w) - x,
            h: (self.y + self.h).max(other.y + other.h) - y,
        }
    }
    fn intersect(self, other: Self) -> Option<Self> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = (self.x + self.w).min(other.x + other.w);
        let bottom = (self.y + self.h).min(other.y + other.h);
        // Disjoint rectangles have nothing to subtract.
        if right <= x || bottom <= y {
            return None;
        }
        Some(Self {
            x,
            y,
            w: right - x,
            h: bottom - y,
        })
    }
    fn full(f: &Frame) -> Self {
        Self {
            x: 0,
            y: 0,
            w: f.width,
            h: f.height,
        }
    }
    /// The part of the frame a request covers; None when it lies outside the frame.
    fn request(b: [u8; 9], f: &Frame) -> io::Result<Option<Self>> {
        let x = u16::from_be_bytes([b[1], b[2]]);
        let y = u16::from_be_bytes([b[3], b[4]]);
        let w = u16::from_be_bytes([b[5], b[6]]);
        let h = u16::from_be_bytes([b[7], b[8]]);
        if w == 0 || h == 0 {
            return Err(invalid());
        }
        if x >= f.width || y >= f.height {
            return Ok(None);
        }
        Ok(Some(Self {
            x,
            y,
            w: w.min(f.width - x),
            h: h.min(f.height - y),
        }))
    }
}
/// Writes a rectangle of a frame's own bytes, in one write when it covers whole rows.
fn raw_rows(s: &mut UnixStream, f: &Frame, r: Region) -> io::Result<()> {
    let width = f.width as usize;
    let start = (r.y as usize * width + r.x as usize) * 4;
    if r.w == f.width {
        return s.write_all(&f.pixels[start..start + width * r.h as usize * 4]);
    }
    let w = r.w as usize * 4;
    let mut rows = Vec::with_capacity(w * r.h as usize);
    for row in 0..r.h as usize {
        let i = start + row * width * 4;
        rows.extend_from_slice(&f.pixels[i..i + w]);
    }
    s.write_all(&rows)
}
/// A ZRLE rectangle before compression: each tile raw, or solid when it's all one colour.
fn tiles(f: &Frame, r: Region, format: PixelFormat) -> Vec<u8> {
    let (left, right) = (r.x as usize, (r.x + r.w) as usize);
    let (top, bottom) = (r.y as usize, (r.y + r.h) as usize);
    let width = f.width as usize;
    let native = format.is_native();
    let mut b = Vec::with_capacity(r.w as usize * r.h as usize * 3 + 1024);
    for y in (top..bottom).step_by(64) {
        let end_y = (y + 64).min(bottom);
        for x in (left..right).step_by(64) {
            let end_x = (x + 64).min(right);
            let first = &f.pixels[(y * width + x) * 4..][..3];
            let solid = (y..end_y).all(|row| {
                f.pixels[(row * width + x) * 4..(row * width + end_x) * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|p| p[..3] == *first)
            });
            b.push(u8::from(solid));
            if solid {
                format.pixel(&f.pixels[(y * width + x) * 4..][..4], true, &mut b);
                continue;
            }
            for row in y..end_y {
                let pixels = f.pixels[(row * width + x) * 4..(row * width + end_x) * 4]
                    .as_chunks::<4>()
                    .0;
                if native {
                    for p in pixels {
                        b.extend_from_slice(&p[..3]);
                    }
                } else {
                    for p in pixels {
                        format.pixel(p, true, &mut b);
                    }
                }
            }
        }
    }
    b
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_requests_are_coalesced() {
        assert_eq!(
            merge_requests([1, 0, 0, 0, 0, 0, 10, 0, 10], [0, 0, 5, 0, 5, 0, 10, 0, 10]),
            [0, 0, 0, 0, 0, 0, 15, 0, 15]
        );
    }
    #[test]
    fn pixel_formats_and_damage() {
        let rgb565 = [16, 16, 0, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0];
        let mut bytes = Vec::new();
        PixelFormat::parse(&rgb565)
            .unwrap()
            .pixel(&[0, 0, 255, 255], false, &mut bytes);
        assert_eq!(bytes, [0, 248]);
        let mut big = rgb565;
        big[2] = 1;
        bytes.clear();
        PixelFormat::parse(&big)
            .unwrap()
            .pixel(&[0, 0, 255, 255], true, &mut bytes);
        assert_eq!(bytes, [248, 0]);
        let mut invalid = rgb565;
        invalid[11] = 11;
        assert!(PixelFormat::parse(&invalid).is_err());
    }
    #[test]
    fn rectangles_apart_have_no_intersection() {
        let rect = |x, y, w, h| Region { x, y, w, h };
        assert_eq!(rect(0, 0, 1, 1).intersect(rect(5, 5, 5, 5)), None);
        assert_eq!(rect(5, 5, 5, 5).intersect(rect(0, 0, 1, 1)), None);
        assert_eq!(rect(0, 0, 5, 5).intersect(rect(5, 0, 5, 5)), None);
        assert_eq!(
            rect(0, 0, 5, 5).intersect(rect(3, 4, 5, 5)),
            Some(rect(3, 4, 2, 1))
        );
    }
    #[test]
    fn changes_add_up_and_run_out() {
        let display = bare_display();
        let mut pixels = vec![9u8; 400];
        let publish = |display: &Display, pixels: &[u8], update| {
            display.publish(&Capture {
                width: 10,
                height: 10,
                stride: 40,
                pixels: pixels.as_ptr(),
                update,
                swapped: false,
            })
        };
        let rect = |x, y, w, h| Region { x, y, w, h };
        publish(&display, &pixels, None);
        // Two pixels change inside a rectangle said to be larger, which tightens to them.
        pixels[(2 * 10 + 2) * 4] = 1;
        pixels[(2 * 10 + 3) * 4] = 1;
        publish(&display, &pixels, Some((0, 0, 8, 8)));
        // A rectangle with nothing really changed in it, and an empty one.
        publish(&display, &pixels, Some((0, 0, 10, 10)));
        publish(&display, &pixels, Some((0, 0, 0, 0)));
        pixels[(6 * 10 + 6) * 4] = 1;
        publish(&display, &pixels, Some((5, 5, 2, 2)));
        assert_eq!(display.changed_since(1, 2), Some(Some(rect(2, 2, 2, 1))));
        assert_eq!(display.changed_since(2, 4), Some(None));
        assert_eq!(display.changed_since(1, 5), Some(Some(rect(2, 2, 5, 5))));
        for n in 0..CHANGES {
            pixels[0] = n as u8;
            publish(&display, &pixels, Some((0, 0, 1, 1)));
        }
        assert_eq!(display.changed_since(1, 5), None);
    }
    #[test]
    fn a_frame_copies_what_changed_unless_a_viewer_holds_it() {
        let display = bare_display();
        let mut pixels = vec![0u8; 400];
        let capture = |pixels: &[u8], update| Capture {
            width: 10,
            height: 10,
            stride: 40,
            pixels: pixels.as_ptr(),
            update,
            swapped: false,
        };
        display.publish(&capture(&pixels, None));
        pixels[0] = 1;
        pixels[44] = 2;
        // Only (1, 1) is said to have changed, so the rest stays as it was.
        display.publish(&capture(&pixels, Some((1, 1, 1, 1))));
        {
            let f = display.frame.lock().unwrap();
            let f = f.as_ref().unwrap();
            assert_eq!((f.pixels[0], f.pixels[44], f.sequence), (0, 2, 2));
        }
        // A viewer reading the last frame keeps it unchanged.
        let held = display.frame.lock().unwrap().clone().unwrap();
        pixels[44] = 3;
        display.publish(&capture(&pixels, Some((1, 1, 1, 1))));
        assert_eq!(held.pixels[44], 2);
        assert_eq!(
            display.frame.lock().unwrap().as_ref().unwrap().pixels[44],
            3
        );
    }
    fn bare_display() -> Shared {
        Arc::new(Display {
            frame: Mutex::new(None),
            view: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            controller: AtomicU64::new(0),
            viewers: Mutex::default(),
            sockets: Mutex::default(),
            changes: Mutex::default(),
            wakers: Mutex::default(),
        })
    }
    /// A display whose frame changes only when a test publishes one, and a connected viewer.
    fn fixed_display() -> (Shared, UnixStream) {
        let display = bare_display();
        publish(&display, 0, None);
        let (mut viewer, server) = UnixStream::pair().unwrap();
        viewer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (wake, waker) = UnixStream::pair().unwrap();
        wake.set_nonblocking(true).unwrap();
        waker.set_nonblocking(true).unwrap();
        display.wakers.lock().unwrap().insert(1, waker);
        let d = display.clone();
        thread::spawn(move || {
            let (tx, _rx) = std::sync::mpsc::sync_channel(8);
            let _ = serve(server, &d, 1, 1, &tx, wake);
        });
        bytes::<12>(&mut viewer).unwrap();
        viewer.write_all(b"RFB 003.008\n").unwrap();
        bytes::<2>(&mut viewer).unwrap();
        viewer.write_all(&[1]).unwrap();
        bytes::<4>(&mut viewer).unwrap();
        viewer.write_all(&[1]).unwrap();
        let init = bytes::<24>(&mut viewer).unwrap();
        let name = u32::from_be_bytes([init[20], init[21], init[22], init[23]]);
        viewer.read_exact(&mut vec![0; name as usize]).unwrap();
        (display, viewer)
    }
    fn publish(display: &Display, value: u8, update: Option<(u16, u16, u16, u16)>) {
        let pixels = vec![value; 400];
        display.publish(&Capture {
            width: 10,
            height: 10,
            stride: 40,
            pixels: pixels.as_ptr(),
            update,
            swapped: false,
        });
    }
    /// Reads an update's header and returns its rectangle count.
    fn update(viewer: &mut UnixStream) -> u16 {
        let header = bytes::<4>(viewer).unwrap();
        u16::from_be_bytes([header[2], header[3]])
    }
    #[test]
    fn a_request_outside_the_frame_is_answered_when_up_to_date() {
        let (display, mut viewer) = fixed_display();
        viewer.write_all(&[3, 0, 0, 0, 0, 0, 0, 10, 0, 10]).unwrap();
        assert_eq!(update(&mut viewer), 1);
        bytes::<{ 12 + 400 }>(&mut viewer).unwrap();
        // Incremental at x=20, with the viewer holding the only frame there is.
        viewer.write_all(&[3, 1, 0, 20, 0, 0, 0, 5, 0, 5]).unwrap();
        assert_eq!(update(&mut viewer), 0);
        display.closed.store(true, Ordering::Release);
    }
    #[test]
    fn a_frame_with_no_change_sends_nothing_and_a_later_change_arrives() {
        let (display, mut viewer) = fixed_display();
        viewer.write_all(&[3, 0, 0, 0, 0, 0, 0, 10, 0, 10]).unwrap();
        assert_eq!(update(&mut viewer), 1);
        bytes::<{ 12 + 400 }>(&mut viewer).unwrap();
        viewer.write_all(&[3, 1, 0, 0, 0, 0, 0, 10, 0, 10]).unwrap();
        // A frame that changed nothing produces no update...
        publish(&display, 0, Some((0, 0, 0, 0)));
        viewer
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut byte = [0];
        assert!(viewer.read_exact(&mut byte).is_err());
        viewer
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // ...and a real change still arrives, without its viewer waiting out a poll.
        publish(&display, 7, Some((2, 3, 1, 1)));
        assert_eq!(update(&mut viewer), 1);
        let rectangle = bytes::<12>(&mut viewer).unwrap();
        assert_eq!(&rectangle[..8], &[0, 2, 0, 3, 0, 1, 0, 1]);
        bytes::<4>(&mut viewer).unwrap();
        display.closed.store(true, Ordering::Release);
    }
    #[test]
    fn compact_pixels_need_a_depth_of_24_or_less() {
        let mut format = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
        for (depth, expected) in [(24, 3), (32, 4)] {
            format[1] = depth;
            let mut bytes = Vec::new();
            PixelFormat::parse(&format)
                .unwrap()
                .pixel(&[1, 2, 3, 255], true, &mut bytes);
            assert_eq!(bytes.len(), expected, "depth {depth}");
        }
    }
    #[test]
    fn requests_outside_the_frame_cover_nothing() {
        let f = Frame {
            width: 10,
            height: 10,
            pixels: Arc::new(vec![0; 400]),
            sequence: 1,
        };
        let request = |x: u8, y: u8, w: u8, h: u8| Region::request([1, 0, x, 0, y, 0, w, 0, h], &f);
        assert_eq!(request(10, 0, 5, 5).unwrap(), None);
        assert_eq!(request(0, 10, 5, 5).unwrap(), None);
        assert_eq!(
            request(8, 0, 5, 5).unwrap(),
            Some(Region {
                x: 8,
                y: 0,
                w: 2,
                h: 5
            })
        );
        assert!(request(0, 0, 0, 5).is_err());
    }
    #[test]
    fn zrle_tile_edges() {
        let mut pixels = vec![42; 65 * 2 * 4];
        // Not all one colour, in the first tile only.
        pixels[0] = 41;
        let f = Frame {
            width: 65,
            height: 2,
            pixels: Arc::new(pixels),
            sequence: 1,
        };
        let b = tiles(&f, Region::full(&f), PixelFormat::default());
        assert_eq!(b[0], 0, "a raw tile");
        assert_eq!(b.len(), 1 + 64 * 2 * 3 + 1 + 3);
        assert_eq!(b[1 + 64 * 2 * 3], 1, "a solid tile");
        assert_eq!(&b[1 + 64 * 2 * 3 + 1..], &[42, 42, 42]);
    }
}
