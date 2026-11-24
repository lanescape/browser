//! One process serves an environment: any number of browsing contexts side by side, sandboxed, and
//! speaking only the control protocol on stdout.
mod support;
use serde_json::json;
use std::{
    io::{Read, Write},
    os::unix::io::AsRawFd,
};
use support::*;

const PAGE: &str = "<!doctype html><body>page</body>";

#[test]
fn malformed_control_closes_the_host_cleanly() {
    for frame in [
        vec![2, 0, 0, 0, 8, 42], // no command
        vec![1, 0, 1, 0],        // oversized, before any payload
        vec![1, 0],              // cut short
    ] {
        let mut client = Client::start(&[]);
        let mut input = client.child.stdin.take().unwrap();
        input.write_all(&frame).unwrap();
        drop(input);
        let out = client.child.stdout.as_mut().unwrap();
        let mut poll = libc::pollfd {
            fd: out.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut poll, 1, 15000) } > 0,
            "it didn't close"
        );
        assert_eq!(out.read(&mut [0]).unwrap(), 0, "it wrote output");
        assert!(client.child.wait().unwrap().success());
    }
}

/// Nothing but the protocol reaches stdout, from the host or any of its processes.
#[test]
fn stdout_is_only_the_protocol() {
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/", PAGE)]);
    c.tab("stdout", &pages.url("/"));
    let host = c.child.id() as i32;
    let stderr = processes::open_file(host, 2).unwrap();
    let helpers = c.helpers();
    assert!(!helpers.is_empty());
    for pid in std::iter::once(host).chain(helpers) {
        if let Some(stdout) = processes::open_file(pid, 1) {
            assert_eq!(stdout, stderr, "process {pid}");
        }
    }
}

/// The launch check run on every release build, including those the suites don't run on: Chromium
/// starts, draws a page and shuts down cleanly.
#[test]
fn starts_draws_and_stops() {
    let page = "<!doctype html><body style='margin:0;background:rgb(20,120,200)'></body>";
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let mut c = Client::start(&[]);
    let tab = c.tab("launch", &url);
    c.loaded(tab.tab, &url);
    Viewer::connect(&tab, false).expect(200, 150, [20, 120, 200]);
    c.stop();
}

/// Left to choose, the host uses the GPU when it can, says which it's using, and works either way.
#[test]
fn rendering_prefers_the_gpu_and_works_without_one() {
    let page = "<!doctype html><body style='margin:0;background:rgb(200,20,40)'></body>";
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let check = |c: &mut Client| {
        let tab = c.tab("rendering", &url);
        c.loaded(tab.tab, &url);
        Viewer::connect(&tab, false).expect(399, 299, [200, 20, 40]);
        let result = c.request(request::Command::Status(Empty {}));
        let response::Result::Status(status) = result else {
            panic!("{result:?}");
        };
        status
    };
    // Only asked for on a machine known to have one; compositing is what Chromium actually runs.
    if std::env::var_os("LANESCAPE_BROWSER_TEST_GPU").is_some() {
        let mut c = Client::start_mode("--rendering=auto", &[]);
        let status = check(&mut c);
        assert!(status.gpu_accelerated, "{status:?}");
        assert_eq!(status.backend, "gpu");
    }
    // No Vulkan driver to find, as on a machine with no GPU. Every Mac has Metal, and nothing outside
    // the process can take it away, so there the fallback can't be arranged.
    if cfg!(target_os = "linux") {
        let mut c = Client::start_env(
            "--rendering=auto",
            &[
                ("VK_ICD_FILENAMES", "/nonexistent.json"),
                ("VK_DRIVER_FILES", "/nonexistent.json"),
            ],
        );
        let status = check(&mut c);
        assert!(!status.gpu_accelerated, "{status:?}");
        assert_eq!(status.backend, "cpu");
        assert!(!status.fallback_reason.is_empty(), "{status:?}");
    }
    // Asked for the CPU, it stays there, whatever the machine has.
    let mut c = Client::start(&[]);
    let status = check(&mut c);
    assert!(!status.gpu_accelerated, "{status:?}");
}

/// WebGL runs on a GPU or not at all: a software fallback would compile a page's shaders to machine
/// code in the GPU process, so a page without a GPU is told there's no WebGL and carries on.
#[test]
fn webgl_needs_a_gpu() {
    let page = "<!doctype html><body style='margin:0'><canvas id=c width=400 height=300></canvas>
      <script>
        const gl = c.getContext('webgl2', {preserveDrawingBuffer: true});
        window.renderer = null;
        if (gl) {
          const info = gl.getExtension('WEBGL_debug_renderer_info');
          window.renderer = gl.getParameter(info.UNMASKED_RENDERER_WEBGL);
          gl.clearColor(30 / 255, 200 / 255, 60 / 255, 1);
          gl.clear(gl.COLOR_BUFFER_BIT);
        } else {
          c.style.display = 'none';
          document.body.style.background = 'rgb(200,20,40)';
        }
      </script></body>";
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let check = |c: &mut Client, color: [u8; 3]| {
        c.initialize(&[DEVTOOLS]);
        let tab = c.tab("webgl", &url);
        c.loaded(tab.tab, &url);
        Viewer::connect(&tab, false).expect(100, 100, color);
        c.devtools(tab.tab).eval("window.renderer")
    };
    if std::env::var_os("LANESCAPE_BROWSER_TEST_GPU").is_some() {
        let mut c = Client::start_mode("--rendering=auto", &[]);
        let renderer = check(&mut c, [30, 200, 60]);
        let renderer = renderer.as_str().unwrap().to_lowercase();
        for software in ["swiftshader", "llvmpipe", "softpipe", "lavapipe"] {
            assert!(!renderer.contains(software), "{renderer}");
        }
    }
    let mut c = Client::start(&[]);
    assert_eq!(check(&mut c, [200, 20, 40]), json!(null));
    // As above, a Mac's GPU can't be taken away.
    if cfg!(target_os = "linux") {
        let mut c = Client::start_env(
            "--rendering=auto",
            &[
                ("VK_ICD_FILENAMES", "/nonexistent.json"),
                ("VK_DRIVER_FILES", "/nonexistent.json"),
            ],
        );
        assert_eq!(check(&mut c, [200, 20, 40]), json!(null));
    }
}

/// With no user namespace to sandbox renderers in, the host doesn't start at all. Where the test can't
/// take them away, there's nothing to learn.
#[cfg(target_os = "linux")]
#[test]
fn no_sandbox_no_host() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    const CANNOT_ARRANGE: i32 = 77;
    let binary = std::env::var("LANESCAPE_BROWSER_BINARY").unwrap();
    let data = std::env::temp_dir().join(format!(
        "lanescape-browser-nosandbox-{}",
        std::process::id()
    ));
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let uid = String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout).unwrap();
    // Room for one namespace below this one, the one the host then runs in, as an ordinary user.
    let inner = format!(
        r#"unshare -U true 2>/dev/null && exit {CANNOT_ARRANGE}; exec "$0" --data-dir "$1" --rendering=cpu </dev/null"#
    );
    let outer = format!(
        r#"echo 1 2>/dev/null > /proc/sys/user/max_user_namespaces || exit {CANNOT_ARRANGE}
exec unshare -U --map-user="$0" --map-group="$0" sh -c "$3" "$1" "$2""#
    );
    let out = Command::new("unshare")
        .args(["-U", "-r", "sh", "-c", &outer, uid.trim(), &binary])
        .arg(&data)
        .arg(&inner)
        .output();
    let _ = std::fs::remove_dir_all(&data);
    let Ok(out) = out else {
        return;
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The first unshare itself refusing, or this arrangement being out of reach.
    if out.status.code() == Some(CANNOT_ARRANGE) || stderr.starts_with("unshare:") {
        return;
    }
    assert!(stderr.contains("No usable sandbox"), "{stderr}");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

/// A process already in a sandbox can't put its renderers in one, as macOS allows only one, so the host
/// doesn't start at all. Where the test can't sandbox anything itself, there's nothing to learn.
#[cfg(target_os = "macos")]
#[test]
fn no_sandbox_no_host() {
    use std::{os::unix::fs::PermissionsExt, process::Command};
    let binary = std::env::var("LANESCAPE_BROWSER_BINARY").unwrap();
    let outer = ["-p", "(version 1)(allow default)"];
    if !Command::new("sandbox-exec")
        .args(outer)
        .arg("/usr/bin/true")
        .status()
        .is_ok_and(|s| s.success())
    {
        return;
    }
    let data = std::env::temp_dir().join(format!(
        "lanescape-browser-nosandbox-{}",
        std::process::id()
    ));
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let out = Command::new("sandbox-exec")
        .args(outer)
        .arg(&binary)
        .arg("--data-dir")
        .arg(&data)
        .arg("--rendering=cpu")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&data);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("No usable sandbox"), "{stderr}");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}

/// Every renderer runs under Chromium's own sandbox: seccomp on Linux, Seatbelt on macOS.
#[test]
fn renderers_are_sandboxed() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("sandbox", &url);
    c.loaded(tab.tab, &url);
    let mut page = c.devtools(tab.tab);
    assert_eq!(page.eval("1 + 1"), json!(2));
    // Renderers descend from the zygote, itself a child of the host, on Linux, and are the host's
    // own children on macOS.
    let mut queue = c.helpers();
    let mut renderers = 0;
    while let Some(pid) = queue.pop() {
        let Some(command) = processes::command_line(pid) else {
            continue;
        };
        if command.contains("--type=renderer") {
            if let Err(why) = processes::renderer_sandbox(pid, &c.root) {
                panic!("renderer {pid}: {why}");
            }
            renderers += 1;
        }
        let out = std::process::Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output()
            .unwrap();
        queue.extend(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<i32>().ok()),
        );
    }
    assert!(renderers > 0, "no renderer found");
}

/// Tabs share storage only within a profile. Each unnamed profile keeps its storage to itself, and a
/// named profile's outlasts the process.
#[test]
fn contexts_keep_their_storage_apart() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let open = |c: &mut Client, profile: &str| {
        let tab = c.tab(profile, &url);
        c.loaded(tab.tab, &url);
        let cdp = c.devtools(tab.tab);
        (tab, cdp)
    };
    let (one_tab, mut one) = open(&mut c, "one");
    assert_eq!(one_tab.profile, "one");
    let (_, mut one_again) = open(&mut c, "one");
    let (_, mut two) = open(&mut c, "two");
    let (unnamed_tab, mut unnamed) = open(&mut c, "");
    let (unnamed_too_tab, mut unnamed_too) = open(&mut c, "");
    for tab in [&unnamed_tab, &unnamed_too_tab] {
        let hex = tab.profile.strip_prefix('~').unwrap();
        assert!(
            hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{tab:?}"
        );
    }
    // Only the host makes unnamed profiles, and only their tabs end them.
    c.refused(
        request::Command::CreateTab(CreateTab {
            profile: unnamed_tab.profile.clone(),
            url: url.clone(),
            ..Default::default()
        }),
        "invalid identifier",
    );
    c.refused(
        request::Command::ReleaseProfile(ReleaseProfile {
            profile: unnamed_tab.profile.clone(),
        }),
        "invalid identifier",
    );
    // A client from before unnamed profiles can't get a named profile's storage for its "private" tab.
    c.refused(
        request::Command::CreateTab(CreateTab {
            profile: "one".into(),
            private: true,
            url: url.clone(),
            ..Default::default()
        }),
        "unnamed profiles",
    );
    assert_ne!(unnamed_tab.profile, unnamed_too_tab.profile);
    one.eval("localStorage.who = 'one'");
    unnamed.eval("localStorage.who = 'unnamed'");
    one_again.until("localStorage.who ?? null", json!("one"));
    assert_eq!(two.eval("localStorage.who ?? null"), json!(null));
    assert_eq!(unnamed_too.eval("localStorage.who ?? null"), json!(null));
    assert_eq!(unnamed.eval("localStorage.who"), json!("unnamed"));
    one.eval("new Promise(r => { document.cookie = 'who=one; max-age=3600'; r() })");
    assert_eq!(one_again.eval("document.cookie"), json!("who=one"));
    assert_eq!(two.eval("document.cookie"), json!(""));
    assert_eq!(unnamed.eval("document.cookie"), json!(""));
    drop((one, one_again, two, unnamed, unnamed_too));

    // Shutting down closes no tab, so unnamed profiles stay stored.
    c.restart(&[DEVTOOLS]);
    for tab in [&unnamed_tab, &unnamed_too_tab] {
        let directory = c.root.join(format!("unnamed-{}", &tab.profile[1..]));
        assert!(directory.join("Local Storage").is_dir(), "{directory:?}");
    }
    let tab = c.tab("one", &url);
    c.loaded(tab.tab, &url);
    let mut again = c.devtools(tab.tab);
    assert_eq!(again.eval("document.cookie"), json!("who=one"));
    let tab = c.tab("", &url);
    c.loaded(tab.tab, &url);
    let mut unnamed = c.devtools(tab.tab);
    assert_eq!(unnamed.eval("document.cookie"), json!(""));
}

/// An unnamed profile is stored like any profile while it has tabs, and its storage goes with its
/// last tab, popups included.
#[test]
fn an_unnamed_profile_goes_with_its_last_tab() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE), ("/popup", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("", &url);
    c.loaded(tab.tab, &url);
    let directory = c.root.join(format!("unnamed-{}", &tab.profile[1..]));
    assert!(directory.is_dir(), "{directory:?}");
    let mut opener = c.devtools(tab.tab);
    opener.gesture(&format!("void window.open('{}')", pages.url("/popup")));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    drop(opener);
    let close = |c: &mut Client, tab: u64| {
        c.action(tab, tab_operation::Action::Close, 0);
        c.event(|e| matches!(e, event::Event::Closed(closed) if closed.tab == tab));
    };
    close(&mut c, tab.tab);
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(directory.is_dir(), "the popup still uses it");
    close(&mut c, popup.tab);
    let deadline = std::time::Instant::now() + PATIENCE;
    while directory.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{directory:?} remains"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

const FORM: &str = "<!doctype html><body><input id=i></body>";

fn closed(c: &mut Client, tab: u64) {
    c.action(tab, tab_operation::Action::Close, 0);
    c.event(|e| matches!(e, event::Event::Closed(closed) if closed.tab == tab));
}

fn unnamed_directory(c: &Client, tab: &Tab) -> std::path::PathBuf {
    c.root.join(format!("unnamed-{}", &tab.profile[1..]))
}

/// A restarted host brings every tab back as it was: its ID and profile, its history, what was
/// typed into it and its history state, and an unnamed profile's storage, session cookies included.
#[test]
fn tabs_resume_after_a_restart() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/a", FORM), ("/b", FORM)]);
    let (a, b) = (pages.url("/a"), pages.url("/b"));
    let named = c.tab("one", &a);
    c.loaded(named.tab, &a);
    c.navigate(named.tab, &b);
    let unnamed = c.tab("", &a);
    c.loaded(unnamed.tab, &a);
    let mut page = c.devtools(named.tab);
    page.eval("i.focus()");
    page.call_in(None, "Input.insertText", json!({"text": "typed"}));
    page.eval("history.replaceState({kept: true}, '')");
    let mut other = c.devtools(unnamed.tab);
    other.eval(
        "new Promise(r => { document.cookie = 'session=1'; localStorage.who = 'unnamed'; r() })",
    );
    drop((page, other));
    // A renderer reports its page state within a second or so, and the host saves a second later.
    std::thread::sleep(std::time::Duration::from_secs(3));

    c.restart(&[]);
    let status = c.initialize(&[DEVTOOLS]);
    assert_eq!(status.previous_exit(), status::PreviousExit::Clean);
    let tabs: Vec<_> = c.tabs().into_iter().map(|t| (t.tab, t.profile)).collect();
    assert_eq!(
        tabs,
        [
            (named.tab, "one".into()),
            (unnamed.tab, unnamed.profile.clone())
        ]
    );
    c.loaded(named.tab, &b);
    let mut page = c.devtools(named.tab);
    page.until("i.value", json!("typed"));
    assert_eq!(page.eval("history.state"), json!({"kept": true}));
    assert_eq!(page.eval("history.length"), json!(2));
    c.action(named.tab, tab_operation::Action::Back, 0);
    c.loaded(named.tab, &a);
    c.loaded(unnamed.tab, &a);
    let mut other = c.devtools(unnamed.tab);
    assert_eq!(other.eval("document.cookie"), json!("session=1"));
    assert_eq!(other.eval("localStorage.who"), json!("unnamed"));
}

/// Shutting down saves every tab as it is then, without waiting for its next save, and a restored
/// tab's display is the one ListTabs names.
#[test]
fn shutdown_saves_the_latest() {
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/a", FORM)]);
    let a = pages.url("/a");
    let tab = c.tab("one", &a);
    c.loaded(tab.tab, &a);
    c.ok(request::Command::TabOperation(TabOperation {
        tab: tab.tab,
        action: tab_operation::Action::Resize as i32,
        width: 640,
        height: 200,
        ..Default::default()
    }));

    c.restart(&[]);
    c.initialize(&[]);
    let tabs = c.tabs();
    assert_eq!(tabs.len(), 1);
    c.loaded(tab.tab, &a);
    let viewer = Viewer::connect(&tabs[0], false);
    assert_eq!((viewer.width, viewer.height), (640, 200));
}

/// After a crash the tabs come back too, and Status says the last host didn't stop cleanly.
#[test]
fn a_crash_resumes_and_says_so() {
    let mut c = Client::start(&[]);
    assert_eq!(
        c.initialize(&[]).previous_exit(),
        status::PreviousExit::FirstRun
    );
    let pages = Pages::serve(&[("/a", FORM)]);
    let a = pages.url("/a");
    let tab = c.tab("", &a);
    c.loaded(tab.tab, &a);
    std::thread::sleep(std::time::Duration::from_secs(2));

    c.crash();
    assert_eq!(
        c.initialize(&[]).previous_exit(),
        status::PreviousExit::Crashed
    );
    let tabs: Vec<_> = c.tabs().into_iter().map(|t| (t.tab, t.profile)).collect();
    assert_eq!(tabs, [(tab.tab, tab.profile)]);
    c.loaded(tab.tab, &a);
}

/// A closed tab is gone for good, with its unnamed profile, and its ID is never handed out again.
#[test]
fn closed_tabs_stay_closed() {
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/a", FORM)]);
    let a = pages.url("/a");
    let kept = c.tab("one", &a);
    c.loaded(kept.tab, &a);
    let gone = c.tab("", &a);
    c.loaded(gone.tab, &a);
    closed(&mut c, gone.tab);

    // Straight after a close, so the next host is the one to remove the unnamed profile.
    c.restart(&[]);
    c.initialize(&[]);
    let tabs: Vec<_> = c.tabs().into_iter().map(|t| t.tab).collect();
    assert_eq!(tabs, [kept.tab]);
    assert!(!unnamed_directory(&c, &gone).exists());
    let new = c.tab("one", &a);
    assert!(new.tab > gone.tab, "{new:?} after {gone:?}");
}

/// Lanescape can start afresh instead, say after crashes on restoring: saved tabs and their unnamed
/// profiles go.
#[test]
fn discarding_saved_tabs_starts_empty() {
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/a", FORM)]);
    let a = pages.url("/a");
    let tab = c.tab("", &a);
    c.loaded(tab.tab, &a);

    c.restart(&[]);
    c.initialize_with(&[], true);
    assert!(c.tabs().is_empty());
    let directory = unnamed_directory(&c, &tab);
    let deadline = std::time::Instant::now() + PATIENCE;
    while directory.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "{directory:?} remains"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    c.restart(&[]);
    c.initialize(&[]);
    assert!(c.tabs().is_empty());
}

/// A popup comes back as a tab of its own, in its opener's profile.
#[test]
fn a_restored_popup_keeps_its_profile() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE), ("/popup", PAGE)]);
    let (url, popup_url) = (pages.url("/"), pages.url("/popup"));
    let tab = c.tab("", &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.eval("localStorage.who = 'opener'");
    opener.gesture(&format!("void window.open('{popup_url}')"));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    c.loaded(popup.tab, &popup_url);
    drop(opener);

    c.restart(&[]);
    c.initialize(&[DEVTOOLS]);
    let tabs: Vec<_> = c.tabs().into_iter().map(|t| (t.tab, t.profile)).collect();
    assert_eq!(
        tabs,
        [
            (tab.tab, tab.profile.clone()),
            (popup.tab, tab.profile.clone())
        ]
    );
    c.loaded(popup.tab, &popup_url);
    let mut popup = c.devtools(popup.tab);
    assert_eq!(popup.eval("localStorage.who"), json!("opener"));
}

/// Each tab's sessionStorage comes back with it, a popup's being its own copy of its opener's.
#[test]
fn session_storage_comes_back() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE), ("/popup", PAGE)]);
    let (url, popup_url) = (pages.url("/"), pages.url("/popup"));
    let tab = c.tab("", &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.eval("sessionStorage.who = 'opener'");
    opener.gesture(&format!("void window.open('{popup_url}')"));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    c.loaded(popup.tab, &popup_url);
    let mut copy = c.devtools(popup.tab);
    copy.until("sessionStorage.who ?? null", json!("opener"));
    copy.eval("sessionStorage.who = 'popup'");
    drop((opener, copy));
    // A page's storage write reaches the disk a moment after its script returns.
    std::thread::sleep(std::time::Duration::from_secs(2));

    c.restart(&[]);
    c.initialize(&[DEVTOOLS]);
    c.loaded(tab.tab, &url);
    c.loaded(popup.tab, &popup_url);
    let mut opener = c.devtools(tab.tab);
    assert_eq!(opener.eval("sessionStorage.who ?? null"), json!("opener"));
    let mut copy = c.devtools(popup.tab);
    assert_eq!(copy.eval("sessionStorage.who ?? null"), json!("popup"));
}

/// A popup is in its opener's profile, and no other.
#[test]
fn a_popup_stays_in_its_opener_s_context() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE), ("/popup", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("", &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.eval("localStorage.who = 'unnamed'");
    opener.gesture(&format!("void window.open('{}')", pages.url("/popup")));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    assert_eq!(popup.profile, tab.profile);
    let mut popup = c.devtools(popup.tab);
    popup.until("localStorage.who ?? null", json!("unnamed"));
}

/// A popup asking for more pixels than a tab may have is shrunk to the largest that fits, its shape kept, and is
/// as usable as any tab: its viewer connects and it draws.
#[test]
fn an_oversized_popup_fits_the_display_limits() {
    let mut c = Client::start(&[DEVTOOLS]);
    let popup_page = "<!doctype html><body style='margin:0;background:rgb(20,120,200)'></body>";
    let pages = Pages::serve(&[("/", PAGE), ("/popup", popup_page)]);
    let url = pages.url("/");
    let tab = c.tab("popups", &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.gesture(&format!(
        "void window.open('{}', '', 'width=5000,height=5000')",
        pages.url("/popup")
    ));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    c.loaded(popup.tab, &pages.url("/popup"));
    let mut viewer = Viewer::connect(&popup, false);
    assert_eq!((viewer.width, viewer.height), (4096, 4096));
    viewer.expect(4000, 4000, [20, 120, 200]);
}

/// Tabs load only web pages.
#[test]
fn tabs_load_only_web_pages() {
    let mut c = Client::start(&[]);
    for url in [
        "file:///etc/passwd",
        "chrome://settings",
        "chrome://crash",
        "javascript:1",
    ] {
        c.refused(
            request::Command::CreateTab(CreateTab {
                profile: "one".into(),
                url: url.into(),
                width: 100,
                height: 100,
                ..Default::default()
            }),
            "web pages",
        );
    }
}
