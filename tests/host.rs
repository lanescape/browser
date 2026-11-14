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
    c.tab("stdout", false, &pages.url("/"));
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
    let tab = c.tab("launch", false, &url);
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
        let tab = c.tab("rendering", false, &url);
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
        let tab = c.tab("webgl", false, &url);
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
    let tab = c.tab("sandbox", false, &url);
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

/// Tabs share storage only within a profile. Private tabs keep theirs to themselves and leave
/// nothing behind, and a profile's outlasts the process.
#[test]
fn contexts_keep_their_storage_apart() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let open = |c: &mut Client, profile: &str, private: bool| {
        let tab = c.tab(profile, private, &url);
        c.loaded(tab.tab, &url);
        (tab.tab, c.devtools(tab.tab))
    };
    let (_, mut one) = open(&mut c, "one", false);
    let (_, mut one_again) = open(&mut c, "one", false);
    let (_, mut two) = open(&mut c, "two", false);
    let (_, mut private) = open(&mut c, "one", true);
    let (_, mut private_too) = open(&mut c, "one", true);
    one.eval("localStorage.who = 'one'");
    private.eval("localStorage.who = 'private'");
    one_again.until("localStorage.who ?? null", json!("one"));
    assert_eq!(two.eval("localStorage.who ?? null"), json!(null));
    assert_eq!(private_too.eval("localStorage.who ?? null"), json!(null));
    assert_eq!(private.eval("localStorage.who"), json!("private"));
    one.eval("new Promise(r => { document.cookie = 'who=one; max-age=3600'; r() })");
    assert_eq!(one_again.eval("document.cookie"), json!("who=one"));
    assert_eq!(two.eval("document.cookie"), json!(""));
    assert_eq!(private.eval("document.cookie"), json!(""));
    drop((one, one_again, two, private, private_too));

    c.restart(&[DEVTOOLS]);
    let tab = c.tab("one", false, &url);
    c.loaded(tab.tab, &url);
    let mut again = c.devtools(tab.tab);
    assert_eq!(again.eval("document.cookie"), json!("who=one"));
    let tab = c.tab("one", true, &url);
    c.loaded(tab.tab, &url);
    let mut private = c.devtools(tab.tab);
    assert_eq!(private.eval("document.cookie"), json!(""));
}

/// A private tab's popup is in that private context, and no other.
#[test]
fn a_popup_stays_in_its_opener_s_context() {
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", PAGE), ("/popup", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("one", true, &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.eval("localStorage.who = 'private'");
    opener.gesture(&format!("void window.open('{}')", pages.url("/popup")));
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    assert!(popup.private);
    let mut popup = c.devtools(popup.tab);
    popup.until("localStorage.who ?? null", json!("private"));
}

/// A popup asking for more pixels than a tab may have is shrunk to the largest that fits, its shape kept, and is
/// as usable as any tab: its viewer connects and it draws.
#[test]
fn an_oversized_popup_fits_the_display_limits() {
    let mut c = Client::start(&[DEVTOOLS]);
    let popup_page = "<!doctype html><body style='margin:0;background:rgb(20,120,200)'></body>";
    let pages = Pages::serve(&[("/", PAGE), ("/popup", popup_page)]);
    let url = pages.url("/");
    let tab = c.tab("popups", false, &url);
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
