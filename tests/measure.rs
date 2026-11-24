//! What rendering costs, measured over real viewers. Run with LANESCAPE_BROWSER_MEASURE set to the modes
//! to measure, such as `cpu,auto`; it prints numbers and asserts nothing about them.
mod support;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use support::*;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const WINDOW: Duration = Duration::from_secs(4);

/// A small part of the page changes on every animation frame, as with a spinner or a clock.
const TICKER: &str = "<!doctype html><body style='margin:0;background:#223'><div id=d style='position:absolute;left:100px;top:100px;width:40px;height:40px'></div><p style='position:absolute;top:200px;left:100px;color:#ddd;font:16px sans-serif;width:900px'>Most of this page stands still while a small corner of it changes. Text like this is what most of a browser's pixels are.</p><script>let n=0;(function f(){d.style.background=`rgb(${n%256},${(n*3)%256},${(n*7)%256})`;n++;requestAnimationFrame(f)})()</script></body>";
/// Every pixel changes on every animation frame, as with a video or a full-window animation.
const FULL: &str = "<!doctype html><body style='margin:0;overflow:hidden'><canvas id=c width=1280 height=720 style='display:block'></canvas><script>const g=c.getContext('2d');let n=0;(function f(){const h=g.createLinearGradient(0,0,1280,720);h.addColorStop(0,`hsl(${n%360},80%,50%)`);h.addColorStop(1,`hsl(${(n+120)%360},80%,50%)`);g.fillStyle=h;g.fillRect(0,0,1280,720);n+=3;requestAnimationFrame(f)})()</script></body>";
const STILL: &str = "<!doctype html><body style='margin:0;background:#223'><p style='color:#ddd;font:16px sans-serif'>Nothing changes here.</p></body>";

/// CPU ticks of a process and all its descendants, by process type.
fn ticks(root: u32) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let mut queue = vec![(root as i32, true)];
    while let Some((pid, is_root)) = queue.pop() {
        let Some(used) = processes::cpu_ticks(pid) else {
            continue;
        };
        let command = processes::command_line(pid).unwrap_or_default();
        let kind = if is_root {
            "browser".to_owned()
        } else {
            // Forked processes rewrite their command line as one string.
            command
                .split(['\0', ' '])
                .find_map(|a| a.strip_prefix("--type="))
                .unwrap_or("other")
                .to_owned()
        };
        *out.entry(kind).or_insert(0) += used;
        let children = std::process::Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output()
            .unwrap();
        queue.extend(
            String::from_utf8_lossy(&children.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok())
                .map(|p| (p, false)),
        );
    }
    out
}

/// Percent of one CPU used by each process type since `before`.
fn used(before: &HashMap<String, u64>, root: u32, elapsed: Duration) -> String {
    let hz = 100.0;
    let after = ticks(root);
    let mut total = 0.0;
    let mut parts: Vec<String> = after
        .iter()
        .filter_map(|(kind, end)| {
            let spent = end.saturating_sub(*before.get(kind).unwrap_or(&0)) as f64;
            let percent = spent / hz / elapsed.as_secs_f64() * 100.0;
            total += percent;
            (percent >= 1.0).then(|| format!("{kind} {percent:.0}%"))
        })
        .collect();
    parts.sort();
    format!("{total:.0}% cpu ({})", parts.join(", "))
}

struct Run {
    c: Client,
    tab: Tab,
    _pages: Pages,
}
fn open(mode: &str, page: &str) -> Run {
    let mut c = Client::start_mode(&format!("--rendering={mode}"), &[DEVTOOLS, FENCE]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab_sized("measure", &url, WIDTH, HEIGHT);
    c.loaded(tab.tab, &url);
    Run {
        c,
        tab,
        _pages: pages,
    }
}

/// Reads updates as fast as they come for the window.
fn watch(mode: &str, name: &str, page: &str, zrle: bool) {
    let run = open(mode, page);
    let mut viewer = Viewer::connect(&run.tab, zrle);
    viewer.request(false);
    viewer.update();
    std::thread::sleep(Duration::from_millis(500));
    let before = ticks(run.c.child.id());
    let start = Instant::now();
    let (mut frames, mut bytes, mut area) = (0u64, 0usize, 0usize);
    while start.elapsed() < WINDOW {
        viewer.request(true);
        viewer.update();
        frames += 1;
        bytes += viewer.wire_bytes;
        area += viewer.rects.iter().map(|(_, _, w, h)| w * h).sum::<usize>();
    }
    let elapsed = start.elapsed();
    println!(
        "measure {mode:4} {name:30} {:5.1} fps  {:8.0} KB/s  {:7} px/frame  {}",
        frames as f64 / elapsed.as_secs_f64(),
        bytes as f64 / 1024.0 / elapsed.as_secs_f64(),
        area / frames.max(1) as usize,
        used(&before, run.c.child.id(), elapsed)
    );
}

/// How long a page's change takes to reach a viewer.
fn latency(mode: &str) {
    let mut run = open(mode, STILL);
    let mut cdp = run.c.devtools(run.tab.tab);
    let mut viewer = Viewer::connect(&run.tab, false);
    viewer.request(false);
    viewer.update();
    let mut samples = Vec::new();
    for n in 0..40u32 {
        let shade = 10 + (n % 200) as u8;
        cdp.eval(&format!(
            "document.body.style.background='rgb({shade},0,0)'"
        ));
        let start = Instant::now();
        while viewer.at(1000, 600) != [shade, 0, 0] {
            viewer.request(true);
            viewer.update();
        }
        samples.push(start.elapsed());
        std::thread::sleep(Duration::from_millis(40));
    }
    samples.sort();
    println!(
        "measure {mode:4} {:30} median {:?}, 90th {:?}, worst {:?}",
        "page change to viewer",
        samples[samples.len() / 2],
        samples[samples.len() * 9 / 10],
        samples[samples.len() - 1]
    );
}

/// What a tab costs when nothing is looking, or nothing is changing.
fn unwatched(mode: &str, name: &str, page: &str, connect: bool) {
    let run = open(mode, page);
    let mut viewer = connect.then(|| Viewer::connect(&run.tab, false));
    if let Some(v) = &mut viewer {
        v.request(false);
        v.update();
    }
    std::thread::sleep(Duration::from_millis(1500));
    let before = ticks(run.c.child.id());
    let start = Instant::now();
    std::thread::sleep(WINDOW);
    println!(
        "measure {mode:4} {name:30} {}",
        used(&before, run.c.child.id(), start.elapsed())
    );
}

/// What recordings cost with nobody watching, `tabs` of them at once, and how much they stream.
fn recording(mode: &str, page: &str, name: &str, tabs: usize) {
    let rendering = format!("--rendering={mode}");
    let mut c = Client::start_args(&[&rendering], &[RECORDING]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let mut running = Vec::new();
    for _ in 0..tabs {
        let tab = c.tab_sized("measure", &url, WIDTH, HEIGHT);
        c.loaded(tab.tab, &url);
        running.push(tab.tab);
    }
    let streams: Vec<Stream> = running.iter().map(|tab| c.record(*tab)).collect();
    std::thread::sleep(Duration::from_millis(1000));
    let before = ticks(c.child.id());
    let start = Instant::now();
    std::thread::sleep(WINDOW);
    let elapsed = start.elapsed();
    let cost = used(&before, c.child.id(), elapsed);
    let (mut frames, mut dropped) = (0, 0);
    for tab in &running {
        let ended = c.stop_recording(*tab);
        frames += ended.frames;
        dropped += ended.dropped;
    }
    let bytes: usize = streams.into_iter().map(|s| s.finish().len()).sum();
    println!(
        "measure {mode:4} {name:30} {tabs} tab(s)  {:6.1} fps each  {:7.0} KB/s each  {dropped} dropped  {cost}",
        frames as f64 / tabs as f64 / (elapsed.as_secs_f64() + 1.0),
        bytes as f64 / 1024.0 / tabs as f64 / (elapsed.as_secs_f64() + 1.0),
    );
}

#[test]
fn measure_rendering() {
    let Ok(modes) = std::env::var("LANESCAPE_BROWSER_MEASURE") else {
        return;
    };
    for mode in modes.split(',') {
        let mut c = Client::start_mode(&format!("--rendering={mode}"), &[]);
        let result = c.request(request::Command::Status(Empty {}));
        println!("measure {mode:4} status {result:?}");
        drop(c);
        if std::env::var("LANESCAPE_BROWSER_MEASURE_ONLY").is_ok_and(|o| o == "recording") {
            for tabs in [1, 4, 8] {
                recording(mode, FULL, "recording full animation", tabs);
            }
            recording(mode, TICKER, "recording small change", 1);
            recording(mode, STILL, "recording still page", 1);
            continue;
        }
        unwatched(mode, "still page, no viewer", STILL, false);
        unwatched(mode, "still page, one viewer", STILL, true);
        unwatched(mode, "full animation, no viewer", FULL, false);
        watch(mode, "small change, raw", TICKER, false);
        watch(mode, "small change, zrle", TICKER, true);
        watch(mode, "full animation, raw", FULL, false);
        watch(mode, "full animation, zrle", FULL, true);
        latency(mode);
        for tabs in [1, 4, 8] {
            recording(mode, FULL, "recording full animation", tabs);
        }
        recording(mode, TICKER, "recording small change", 1);
    }
}
