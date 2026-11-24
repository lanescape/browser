//! A tab can be recorded as a stream of VP8 between two commands, one stream to a tab and any number
//! of tabs at once, and what comes out plays elsewhere with what the tab showed.
mod support;
use serde_json::Value;
use std::time::Duration;
use support::*;

/// An IVF file's frames: their timestamps and data.
fn frames(file: &[u8]) -> Vec<(u64, &[u8])> {
    assert_eq!(&file[..4], b"DKIF");
    assert_eq!(&file[8..12], b"VP80");
    let mut frames = Vec::new();
    let mut at = u16::from_le_bytes([file[6], file[7]]) as usize;
    while at + 12 <= file.len() {
        let size = u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
        let pts = u64::from_le_bytes(file[at + 4..at + 12].try_into().unwrap());
        frames.push((pts, &file[at + 12..at + 12 + size]));
        at += 12 + size;
    }
    assert_eq!(at, file.len(), "the file ends on a frame");
    frames
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - i * 6)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes a recording in the browser itself, and returns the colour at a point of its last frame.
const DECODE: &str = r#"async (data, x, y) => {
  const bytes = Uint8Array.from(atob(data), c => c.charCodeAt(0));
  const view = new DataView(bytes.buffer);
  let colour = null;
  let last = null;
  const decoder = new VideoDecoder({
    output: f => { if (last) last.close(); last = f; },
    error: e => { colour = String(e); },
  });
  decoder.configure({ codec: 'vp8' });
  let at = view.getUint16(6, true);
  let first = true;
  while (at + 12 <= bytes.length) {
    const size = view.getUint32(at, true);
    const pts = Number(view.getBigUint64(at + 4, true));
    decoder.decode(new EncodedVideoChunk({
      type: first ? 'key' : ((bytes[at + 12] & 1) ? 'delta' : 'key'),
      timestamp: pts * 1000,
      data: bytes.subarray(at + 12, at + 12 + size),
    }));
    first = false;
    at += 12 + size;
  }
  await decoder.flush();
  const canvas = new OffscreenCanvas(last.displayWidth, last.displayHeight);
  const g = canvas.getContext('2d');
  g.drawImage(last, 0, 0);
  const p = g.getImageData(x, y, 1, 1).data;
  return colour ?? [last.displayWidth, last.displayHeight, p[0], p[1], p[2]];
}"#;

/// Decodes a stream in a browser, and returns the size and a colour of its last frame.
fn decode(c: &mut Client, pages: &Pages, stream: &[u8], x: u32, y: u32) -> Vec<i64> {
    let url = pages.url("/player");
    let player = c.tab_sized("player", &url, 200, 200);
    c.loaded(player.tab, &url);
    let mut cdp = c.devtools(player.tab);
    let decoded: Value = cdp.eval(&format!("({DECODE})('{}', {x}, {y})", base64(stream)));
    decoded
        .as_array()
        .unwrap_or_else(|| panic!("{decoded}"))
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect()
}

/// `path`'s last frame as RGB from ffmpeg: its width and height from ffprobe, and its pixels.
fn in_ffmpeg(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let probe = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args([
            "-show_entries",
            "stream=codec_name,width,height,nb_read_frames",
        ])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&probe.stdout);
    assert!(
        probe.status.success(),
        "{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let fields: Vec<&str> = text.trim().split(',').collect();
    assert_eq!(fields[0], "vp8", "{text}");
    let (width, height): (u32, u32) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let frames: u32 = fields[3].parse().unwrap();
    let picture = path.with_extension("rgb");
    let convert = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-vf", &format!("select=eq(n\\,{})", frames - 1)])
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .arg(&picture)
        .output()
        .unwrap();
    assert!(
        convert.status.success(),
        "{}",
        String::from_utf8_lossy(&convert.stderr)
    );
    (width, height, std::fs::read(&picture).unwrap())
}

fn ffmpeg_installed() -> bool {
    let installed = std::process::Command::new("ffprobe")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !installed {
        eprintln!("skipped: no ffprobe");
    }
    installed
}

const PAGE: &str = "<!doctype html><body style='margin:0;background:rgb(0,0,0)'></body>";
const PLAYER: &str = "<!doctype html><body></body>";

#[test]
fn a_recording_shows_what_the_tab_showed() {
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", PAGE), ("/player", PLAYER)]);
    let url = pages.url("/");
    let tab = c.tab_sized("recording", &url, 640, 360);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let stream = c.record(tab.tab);
    for shade in (0..=200u8).step_by(5) {
        cdp.eval(&format!(
            "document.body.style.background = 'rgb({shade},{},20)'",
            200 - shade
        ));
        std::thread::sleep(Duration::from_millis(40));
    }
    cdp.eval("document.body.style.background = 'rgb(200,20,40)'");
    std::thread::sleep(Duration::from_millis(300));
    let ended = c.stop_recording(tab.tab);
    assert_eq!(ended.reason(), recording_ended::Reason::Stopped);
    assert_eq!(ended.dropped, 0);
    let bytes = stream.finish();
    let frames = frames(&bytes);
    assert_eq!(u16::from_le_bytes([bytes[12], bytes[13]]), 640);
    assert_eq!(u16::from_le_bytes([bytes[14], bytes[15]]), 360);
    assert_eq!(ended.frames as usize, frames.len());
    assert!(frames.len() >= 20, "{} frames", frames.len());
    assert_eq!(frames[0].1[0] & 1, 0, "it starts on a key frame");
    assert!(
        frames.windows(2).all(|w| w[0].0 < w[1].0),
        "timestamps only go forward"
    );

    let decoded = decode(&mut c, &pages, &bytes, 320, 200);
    assert_eq!(&decoded[..2], &[640, 360]);
    for (got, expected) in decoded[2..].iter().zip([200, 20, 40]) {
        assert!(
            (got - expected).abs() <= 24,
            "decoded {decoded:?}, expected {expected}"
        );
    }
}

/// Another decoder agrees, colours included. Runs when ffmpeg is installed.
#[test]
fn a_recording_plays_in_ffmpeg_with_the_right_colours() {
    if !ffmpeg_installed() {
        return;
    }
    const BARS: [[u8; 3]; 6] = [
        [200, 20, 40],
        [40, 200, 20],
        [20, 40, 200],
        [230, 230, 230],
        [128, 128, 128],
        [30, 30, 30],
    ];
    let divs: String = BARS
        .iter()
        .enumerate()
        .map(|(i, [r, g, b])| {
            format!(
                "<div style='position:absolute;left:{}px;top:0;width:100px;height:360px;background:rgb({r},{g},{b})'></div>",
                i * 100
            )
        })
        .collect();
    let page = format!("<!doctype html><body style='margin:0;background:#000'>{divs}</body>");
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", &page)]);
    let url = pages.url("/");
    let tab = c.tab_sized("recording", &url, 600, 360);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let stream = c.record(tab.tab);
    // Enough frames for the encoder's quality to settle on the picture.
    cdp.eval("const t = document.createElement('div'); t.style = 'position:absolute;left:0;top:340px;width:20px;height:20px;background:#fff'; document.body.append(t); window.n = 0; setInterval(() => { t.style.left = (window.n++ % 500) + 'px' }, 33)");
    std::thread::sleep(Duration::from_secs(3));
    c.stop_recording(tab.tab);
    let path = c.root.join("colours.ivf");
    std::fs::write(&path, stream.finish()).unwrap();
    let (width, height, pixels) = in_ffmpeg(&path);
    assert_eq!((width, height), (600, 360));
    let mut worst = 0;
    for (i, expected) in BARS.iter().enumerate() {
        let at = (100 * 600 + i * 100 + 50) * 3;
        for (got, drew) in pixels[at..at + 3].iter().zip(expected) {
            worst = worst.max((i32::from(*got) - i32::from(*drew)).abs());
        }
    }
    assert!(worst <= 12, "colours are off by {worst}");
}

/// Several tabs at once, across contexts: each stream has its own tab's picture, and stopping one
/// leaves the rest going.
#[test]
fn tabs_record_at_the_same_time() {
    if !ffmpeg_installed() {
        return;
    }
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let colours = [
        ("one", [200u8, 20, 40]),
        ("two", [40, 200, 20]),
        ("", [20, 40, 200]),
        ("", [230, 230, 60]),
    ];
    let mut recordings = Vec::new();
    for (profile, colour) in colours {
        let tab = c.tab_sized(profile, &url, 320, 180);
        c.loaded(tab.tab, &url);
        let mut cdp = c.devtools(tab.tab);
        let stream = c.record(tab.tab);
        cdp.eval(&format!(
            "document.body.style.background = 'rgb({},{},{})'; window.n = 0; const d = document.body.appendChild(document.createElement('div')); d.style = 'width:10px;height:10px;background:#fff;position:absolute;top:0'; setInterval(() => d.style.left = (window.n++ % 300) + 'px', 33)",
            colour[0], colour[1], colour[2]
        ));
        recordings.push((tab, cdp, stream, colour));
    }
    std::thread::sleep(Duration::from_secs(1));
    // The first stops, and the others carry on past it: what their pages draw after it has stopped
    // still reaches their streams.
    let first = recordings.remove(0);
    let first_ended = c.stop_recording(first.0.tab);
    assert_eq!(first_ended.reason(), recording_ended::Reason::Stopped);
    let mut finished = vec![(first.2, first.3, c.root.join("first.ivf"))];
    let recordings: Vec<_> = recordings
        .into_iter()
        .map(|(tab, mut cdp, stream, colour)| {
            let later = colour.map(|c| 255 - c);
            cdp.eval(&format!(
                "document.body.style.background = 'rgb({},{},{})'",
                later[0], later[1], later[2]
            ));
            (tab, stream, later)
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    for (n, (tab, stream, colour)) in recordings.into_iter().enumerate() {
        let ended = c.stop_recording(tab.tab);
        assert_eq!(ended.dropped, 0);
        finished.push((stream, colour, c.root.join(format!("{n}.ivf"))));
    }
    for (stream, colour, path) in finished {
        let bytes = stream.finish();
        std::fs::write(&path, &bytes).unwrap();
        let (width, height, pixels) = in_ffmpeg(&path);
        assert_eq!((width, height), (320, 180));
        let at = (100 * 320 + 160) * 3;
        for (got, drew) in pixels[at..at + 3].iter().zip(colour) {
            assert!(
                (i32::from(*got) - i32::from(drew)).abs() <= 16,
                "{colour:?}: {:?}",
                &pixels[at..at + 3]
            );
        }
    }
}

#[test]
fn one_recording_to_a_tab() {
    let mut c = Client::start(&[RECORDING]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("recording", &url);
    c.loaded(tab.tab, &url);
    let start = |tab| request::Command::StartRecording(StartRecording { tab });
    let stop = |tab| request::Command::StopRecording(StopRecording { tab });
    c.refused(stop(tab.tab), "not recording");
    c.refused(start(tab.tab + 100), "unknown tab");
    let stream = c.record(tab.tab);
    c.refused(start(tab.tab), "already recording");
    c.stop_recording(tab.tab);
    stream.finish();
    // Once it has ended, another can start, and it starts on a key frame.
    let stream = c.record(tab.tab);
    std::thread::sleep(Duration::from_millis(300));
    c.stop_recording(tab.tab);
    let bytes = stream.finish();
    assert_eq!(frames(&bytes)[0].1[0] & 1, 0);
}

#[test]
fn recording_needs_its_capability() {
    let mut c = Client::start(&[DEVTOOLS]);
    let tab = c.tab("recording", "about:blank");
    c.refused(
        request::Command::StartRecording(StartRecording { tab: tab.tab }),
        "not negotiated",
    );
}

/// There's a limit, and it has room for every tab that's likely to need it.
#[test]
fn recordings_at_once_are_limited() {
    let mut c = Client::start(&[RECORDING]);
    let start = |tab| request::Command::StartRecording(StartRecording { tab });
    let mut tabs = Vec::new();
    for _ in 0..33 {
        tabs.push(c.tab("limit", "about:blank").tab);
    }
    for tab in &tabs[..32] {
        assert!(matches!(
            c.request(start(*tab)),
            response::Result::Recording(_)
        ));
    }
    c.refused(start(tabs[32]), "LIMIT");
    c.ok(request::Command::StopRecording(StopRecording {
        tab: tabs[0],
    }));
    c.recording_ended(tabs[0]);
    assert!(matches!(
        c.request(start(tabs[32])),
        response::Result::Recording(_)
    ));
}

/// A reader that isn't reading holds up neither the page nor another recording. It loses frames
/// past the queue's bound, and what it then gets still decodes, from a key frame on.
#[test]
fn a_stalled_reader_loses_frames_and_holds_up_nobody() {
    // Noise at this size runs past the queue's 8 MiB within the stall.
    let noise = "<!doctype html><body style='margin:0;overflow:hidden'><canvas id=c width=480 height=270 style='width:3840px;height:2160px;image-rendering:pixelated'></canvas><script>const g=c.getContext('2d');const d=g.createImageData(480,270);(function f(){for(let i=0;i<d.data.length;i+=4){d.data[i]=Math.random()*256;d.data[i+1]=Math.random()*256;d.data[i+2]=Math.random()*256;d.data[i+3]=255}g.putImageData(d,0,0);requestAnimationFrame(f)})()</script></body>";
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", noise), ("/calm", PAGE)]);
    let url = pages.url("/");
    let stalled_tab = c.tab_sized("recording", &url, 3840, 2160);
    c.loaded(stalled_tab.tab, &url);
    let mut stalled_page = c.devtools(stalled_tab.tab);
    let result = c.request(request::Command::StartRecording(StartRecording {
        tab: stalled_tab.tab,
    }));
    let response::Result::Recording(recording) = result else {
        panic!("{result:?}");
    };
    let stalled = Stream::read_after(&recording, Duration::from_secs(25));
    let calm = pages.url("/calm");
    let other = c.tab_sized("recording", &calm, 320, 180);
    c.loaded(other.tab, &calm);
    let mut other_page = c.devtools(other.tab);
    let stream = c.record(other.tab);
    other_page.eval("window.n = 0; setInterval(() => { document.body.style.background = `rgb(${window.n++ % 256},0,0)` }, 33)");
    for _ in 0..40 {
        assert_eq!(stalled_page.eval("1 + 1"), serde_json::json!(2));
        std::thread::sleep(Duration::from_millis(500));
    }
    let ended = c.stop_recording(other.tab);
    assert_eq!(ended.dropped, 0);
    assert!(frames(&stream.finish()).len() as u64 == ended.frames);
    // It catches up on what's left once it reads, and comes to the end.
    std::thread::sleep(Duration::from_secs(6));
    let ended = c.stop_recording(stalled_tab.tab);
    assert!(ended.dropped > 0, "{ended:?}");
    let bytes = stalled.finish();
    let frames = frames(&bytes);
    assert!(frames[0].1[0] & 1 == 0);
    assert!(frames.windows(2).all(|w| w[0].0 < w[1].0));
    if ffmpeg_installed() {
        let path = c.root.join("stalled.ivf");
        std::fs::write(&path, &bytes).unwrap();
        let decode = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-xerror", "-i"])
            .arg(&path)
            // Decoding is what's on trial, not ffmpeg's rounding of timestamps on the way out.
            .args(["-fps_mode", "passthrough", "-enc_time_base", "1:1000"])
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        assert!(
            decode.status.success() && decode.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&decode.stderr)
        );
    }
}

/// A resize carries on with key frames of the new size.
#[test]
fn a_recording_follows_a_resize() {
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", PAGE), ("/player", PLAYER)]);
    let url = pages.url("/");
    let tab = c.tab_sized("recording", &url, 640, 360);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let stream = c.record(tab.tab);
    cdp.eval("document.body.style.background = 'rgb(200,20,40)'");
    std::thread::sleep(Duration::from_millis(300));
    c.ok(request::Command::TabOperation(TabOperation {
        tab: tab.tab,
        action: tab_operation::Action::Resize as i32,
        width: 400,
        height: 300,
        ..Default::default()
    }));
    std::thread::sleep(Duration::from_millis(300));
    cdp.eval("document.body.style.background = 'rgb(40,200,20)'");
    std::thread::sleep(Duration::from_millis(300));
    c.stop_recording(tab.tab);
    let bytes = stream.finish();
    assert_eq!(&decode(&mut c, &pages, &bytes, 200, 150)[..2], &[400, 300]);
}

/// However it ends, its reader gets the end of the stream, and the client hears why.
#[test]
fn every_way_a_recording_ends_is_reported() {
    use recording_ended::Reason;
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("recording", &url);
    c.loaded(tab.tab, &url);

    // Its reader goes away, and another can start.
    let result = c.request(request::Command::StartRecording(StartRecording {
        tab: tab.tab,
    }));
    let response::Result::Recording(recording) = result else {
        panic!("{result:?}");
    };
    let reader = std::os::unix::net::UnixStream::connect(&recording.socket).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    drop(reader);
    // The page stands still, so no write finds this out.
    assert_eq!(c.recording_ended(tab.tab).reason(), Reason::ReaderClosed);

    // Its tab closes.
    let stream = c.record(tab.tab);
    std::thread::sleep(Duration::from_millis(100));
    c.action(tab.tab, tab_operation::Action::Close, 0);
    assert_eq!(c.recording_ended(tab.tab).reason(), Reason::TabClosed);
    assert!(!frames(&stream.finish()).is_empty());

    // Nobody connects in time: the bound is long, so this one comes last.
    let tab = c.tab("recording", &url);
    c.loaded(tab.tab, &url);
    let result = c.request(request::Command::StartRecording(StartRecording {
        tab: tab.tab,
    }));
    assert!(
        matches!(result, response::Result::Recording(_)),
        "{result:?}"
    );
    assert_eq!(c.recording_ended(tab.tab).reason(), Reason::ConnectTimeout);
}

/// The host going away finishes its streams.
#[test]
fn shutting_down_finishes_a_recording() {
    let mut c = Client::start(&[RECORDING]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("recording", &url);
    c.loaded(tab.tab, &url);
    let stream = c.record(tab.tab);
    std::thread::sleep(Duration::from_millis(200));
    c.stop();
    assert!(!frames(&stream.finish()).is_empty());
    // Its last word comes before the host's end.
    let ended = c.recording_ended(tab.tab);
    assert_eq!(ended.reason(), recording_ended::Reason::Shutdown);
}

/// Started before the first paint, it starts with the page.
#[test]
fn a_recording_can_start_with_its_tab() {
    let mut c = Client::start(&[DEVTOOLS, RECORDING]);
    let pages = Pages::serve(&[
        (
            "/",
            "<!doctype html><body style='margin:0;background:rgb(200,20,40)'></body>",
        ),
        ("/player", PLAYER),
    ]);
    let url = pages.url("/");
    let tab = c.tab_sized("recording", &url, 320, 180);
    let stream = c.record(tab.tab);
    c.loaded(tab.tab, &url);
    std::thread::sleep(Duration::from_millis(400));
    c.stop_recording(tab.tab);
    let bytes = stream.finish();
    let decoded = decode(&mut c, &pages, &bytes, 160, 90);
    for (got, expected) in decoded[2..].iter().zip([200, 20, 40]) {
        assert!((got - expected).abs() <= 24, "decoded {decoded:?}");
    }
}
