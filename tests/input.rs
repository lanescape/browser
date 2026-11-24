//! A viewer's keyboard and pointer reach the page as a person's would, in any frame.
mod support;
use serde_json::json;
use std::time::{Duration, Instant};
use support::*;

const SHIFT: u32 = 0xffe1;
const CONTROL: u32 = 0xffe3;
/// The modifier of editing shortcuts such as Select All: Control, or Command (Super) on macOS, where
/// they are the Edit menu's.
const SHORTCUT: u32 = if cfg!(target_os = "macos") {
    0xffeb
} else {
    CONTROL
};

fn start(page: &str) -> (Client, Pages, Controlled) {
    let mut c = Client::start(&[DEVTOOLS, IME]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("input", &url);
    c.loaded(tab.tab, &url);
    let t = c.controlled(&tab);
    (c, pages, t)
}

#[test]
fn typing_reaches_the_page() {
    let (_c, _pages, mut t) = start("<!doctype html><body><input id=i></body>");
    t.page.eval("i.focus()");
    for key in "ab1-=/.,;".chars() {
        t.viewer.tap(key as u32);
    }
    // Shifted, as keysyms carry them.
    t.viewer.key(true, SHIFT);
    for key in "A!_+?".chars() {
        t.viewer.tap(key as u32);
    }
    t.viewer.key(false, SHIFT);
    // A keysym with no key behind it.
    t.viewer.tap(0x0100_3042);
    t.page.until("i.value", json!("ab1-=/.,;A!_+?あ"));
    // Editing keys.
    t.viewer.tap(0xff08);
    t.page.until("i.value", json!("ab1-=/.,;A!_+?"));
    t.viewer.key(true, SHORTCUT);
    t.viewer.tap(0x61);
    t.viewer.key(false, SHORTCUT);
    t.viewer.tap(0x7a);
    t.page.until("i.value", json!("z"));
}

#[test]
fn clicks_count_as_a_person_s() {
    let page = "<!doctype html><body style='margin:0'><p id=p style='margin:0;font:20px monospace'>hello world</p><script>window.log=[];for(const t of ['click','dblclick'])addEventListener(t,e=>log.push(t+':'+e.detail))</script></body>";
    let (_c, _pages, mut t) = start(page);
    t.viewer.click(20, 10);
    t.page.until("log.join()", json!("click:1"));
    t.page.eval("log.length = 0");
    std::thread::sleep(Duration::from_millis(700));
    t.viewer.click(20, 10);
    t.viewer.click(20, 10);
    t.page
        .until("log.join()", json!("click:1,click:2,dblclick:2"));
    assert_eq!(
        t.page.eval("getSelection().toString().trim()"),
        json!("hello")
    );
}

#[test]
fn the_wheel_scrolls() {
    let page = "<!doctype html><body style='margin:0;width:5000px;height:5000px'></body>";
    let (_c, _pages, mut t) = start(page);
    // Wheel down is button 5, wheel right button 7.
    t.viewer.pointer(0, 50, 50);
    for _ in 0..3 {
        t.viewer.pointer(16, 50, 50);
        t.viewer.pointer(0, 50, 50);
    }
    let deadline = Instant::now() + PATIENCE;
    while t.page.eval("scrollY").as_f64().unwrap() <= 0.0 {
        assert!(Instant::now() < deadline, "no vertical scroll");
        std::thread::sleep(Duration::from_millis(16));
    }
    t.viewer.pointer(64, 50, 50);
    t.viewer.pointer(0, 50, 50);
    while t.page.eval("scrollX").as_f64().unwrap() <= 0.0 {
        assert!(Instant::now() < deadline, "no horizontal scroll");
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// A popup's first document takes input in order too, though its frame predates its tab.
#[test]
fn a_popup_s_first_document_keeps_input_in_order() {
    let mut c = Client::start(&[DEVTOOLS, IME]);
    let pages = Pages::serve(&[("/", "<!doctype html><body></body>")]);
    let url = pages.url("/");
    let tab = c.tab("input", &url);
    c.loaded(tab.tab, &url);
    let mut opener = c.devtools(tab.tab);
    opener.gesture(
        "window.popup = window.open(''); popup.document.body.innerHTML = '<input id=i>'; void 0",
    );
    let event::Event::Popup(created) = c.event(|e| matches!(e, event::Event::Popup(_))) else {
        unreachable!()
    };
    let popup = created.tab.unwrap();
    let mut t = c.controlled(&popup);
    t.page.eval("document.getElementById('i').focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    let mut sent = 0;
    let mut expected = String::new();
    for n in 0..10u32 {
        let typed = char::from(b'a' + n as u8);
        t.viewer.tap(u32::from(typed));
        sent += 2;
        expected.push(typed);
        let word = n.to_string();
        let id = c.send(request::Command::ImeInput(ImeInput {
            after_rfb_input: sent,
            ..match ime(&t, editor.editor, ime_input::Action::Commit, &word) {
                request::Command::ImeInput(input) => input,
                _ => unreachable!(),
            }
        }));
        expected.push_str(&word);
        assert!(matches!(c.response(id), response::Result::Ok(_)));
    }
    t.page
        .until("document.getElementById('i').value", json!(expected));
}

/// A click lands in an out-of-process iframe, and typing follows it there.
#[test]
fn input_reaches_an_out_of_process_iframe() {
    let mut c = Client::start(&[DEVTOOLS, IME, FENCE]);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='http://localhost:{port}/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe></body>";
    let frame = "<!doctype html><body style='margin:0'><input id=i style='position:absolute;left:0;top:0;width:200px;height:30px'></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", frame)]);
    let url = pages.url("/");
    let tab = c.tab("input", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let session = t.page.frame_session(&pages.other_site("/frame"));
    // Once the frame is drawn, as a person sees it before clicking, its hit test data is there.
    c.fence(t.tab).unwrap();
    t.viewer.click(70, 45);
    c.editor(t.tab, |s| s.editor != 0);
    assert_eq!(
        t.page
            .eval_in(Some(&session), "document.activeElement.id", false),
        json!("i")
    );
    t.viewer.tap(0x71);
    t.page.until_in(Some(&session), "i.value", json!("q"));
}

/// The pointer going into an out-of-process iframe, even straight from outside the page, hovers the
/// iframe's container as well, as the pointer is over both; moving within the container leaves only
/// the iframe, and moving out of both leaves neither.
#[test]
fn hover_follows_the_pointer_into_an_out_of_process_iframe() {
    let mut c = Client::start(&[DEVTOOLS, IME, FENCE]);
    let main = "<!doctype html><body style='margin:0'><div id=d style='position:absolute;left:20px;top:20px;width:300px;height:150px'><iframe src='http://localhost:{port}/frame' style='position:absolute;left:30px;top:30px;width:200px;height:80px;border:0'></iframe></div><script>window.left = 0; window.at = null; d.addEventListener('mouseleave', () => left++); addEventListener('mousemove', e => at = [e.clientX, e.clientY])</script></body>";
    let frame = "<!doctype html><body style='margin:0;height:100vh'></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", frame)]);
    let url = pages.url("/");
    let tab = c.tab("input", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let session = t.page.frame_session(&pages.other_site("/frame"));
    c.fence(t.tab).unwrap();
    // Straight into the iframe. The page itself sees the pointer arrive over it too.
    t.viewer.pointer(0, 100, 80);
    t.page.until_in(
        Some(&session),
        "document.body.matches(':hover')",
        json!(true),
    );
    t.page.until("at", json!([100, 80]));
    assert_eq!(t.page.eval("[d.matches(':hover'), left]"), json!([true, 0]));
    // Out of the iframe, still in its container.
    t.viewer.pointer(0, 30, 30);
    t.page.until("at", json!([30, 30]));
    t.page.until_in(
        Some(&session),
        "document.body.matches(':hover')",
        json!(false),
    );
    assert_eq!(t.page.eval("[d.matches(':hover'), left]"), json!([true, 0]));
    // Back in, then out of both.
    t.viewer.pointer(0, 110, 90);
    t.page.until_in(
        Some(&session),
        "document.body.matches(':hover')",
        json!(true),
    );
    t.page.until("at", json!([110, 90]));
    assert_eq!(t.page.eval("[d.matches(':hover'), left]"), json!([true, 0]));
    t.viewer.pointer(0, 500, 400);
    t.page
        .until("[d.matches(':hover'), left]", json!([false, 1]));
    t.page.until_in(
        Some(&session),
        "document.body.matches(':hover')",
        json!(false),
    );
}

/// macOS's editing keys, which its key bindings rather than Blink's give their meaning: Command with
/// the arrows and Backspace, and Control's Emacs bindings.
#[cfg(target_os = "macos")]
#[test]
fn macos_editing_keys_move_and_delete() {
    const COMMAND: u32 = 0xffeb;
    let (_c, _pages, mut t) = start("<!doctype html><body><input id=i></body>");
    t.page.eval("i.focus()");
    for key in "abc def".chars() {
        t.viewer.tap(key as u32);
    }
    t.page.until("i.value", json!("abc def"));
    // Command+Left: the start of the line.
    t.viewer.key(true, COMMAND);
    t.viewer.tap(0xff51);
    t.viewer.key(false, COMMAND);
    t.viewer.tap(0x78);
    t.page.until("i.value", json!("xabc def"));
    // Control+E: the end of the paragraph.
    t.viewer.key(true, CONTROL);
    t.viewer.tap(0x65);
    t.viewer.key(false, CONTROL);
    t.viewer.tap(0x79);
    t.page.until("i.value", json!("xabc defy"));
    // In right-to-left text, Command+Left goes to the line's left end, its logical end.
    t.page
        .eval("i.dir = 'rtl'; i.value = '\\u05d0\\u05d1\\u05d2'; i.setSelectionRange(0, 0)");
    t.viewer.key(true, COMMAND);
    t.viewer.tap(0xff51);
    t.viewer.key(false, COMMAND);
    t.viewer.tap(0x78);
    t.page.until("i.value", json!("\u{5d0}\u{5d1}\u{5d2}x"));
    t.page
        .eval("i.dir = 'ltr'; i.value = 'xabc defy'; i.setSelectionRange(9, 9)");
    // Command+Backspace: everything back to the start of the line.
    t.viewer.key(true, COMMAND);
    t.viewer.tap(0xff08);
    t.viewer.key(false, COMMAND);
    t.viewer.tap(0x7a);
    t.page.until("i.value", json!("z"));
}

/// A shortcut its renderer is too busy to run in time is dropped, never run after later typing: either
/// it selects everything before the typing replaces it, or the typing follows the old text. So too
/// when one late shortcut follows another.
#[cfg(target_os = "macos")]
#[test]
fn a_late_shortcut_never_overtakes_later_typing() {
    const COMMAND: u32 = 0xffeb;
    let (_c, _pages, mut t) = start("<!doctype html><body><input id=i></body>");
    let select_all = |t: &mut Controlled| {
        t.viewer.key(true, COMMAND);
        t.viewer.tap(0x61);
        t.viewer.key(false, COMMAND);
    };
    // Each Command+A's keydown makes the renderer busy for the next stall, in milliseconds.
    for stalls in ["[3000]", "[3000, 500]"] {
        t.page.eval(&format!(
            "i.value = 'abc'; i.focus(); i.setSelectionRange(3, 3); window.stalls = {stalls};
             i.onkeydown = e => {{ if (e.metaKey && e.key == 'a') {{ const n = stalls.shift() ?? 0;
               setTimeout(() => {{ const s = Date.now(); while (Date.now() - s < n); }}, 0) }} }}"
        ));
        for _ in 0..stalls.matches(',').count() + 1 {
            select_all(&mut t);
        }
        t.viewer.tap(0x78);
        t.viewer.tap(0x79);
        // The text typed last, with the caret after it and nothing selected.
        let done = |value: &serde_json::Value| {
            value == &json!(["abcxy", 5, 5]) || value == &json!(["xy", 2, 2])
        };
        let deadline = Instant::now() + PATIENCE;
        let read = "[i.value, i.selectionStart, i.selectionEnd]";
        loop {
            let value = t.page.eval(read);
            if done(&value) {
                break;
            }
            assert!(Instant::now() < deadline, "{stalls}: {value}");
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(3500));
        let value = t.page.eval(read);
        assert!(done(&value), "{stalls}: {value}");
    }
}

/// Likewise in a frame sharing its parent's process, typing and all.
#[test]
fn input_reaches_a_same_process_iframe() {
    let mut c = Client::start(&[DEVTOOLS, IME, FENCE]);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe></body>";
    let frame = "<!doctype html><body style='margin:0'><input id=i style='position:absolute;left:0;top:0;width:200px;height:30px'></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", frame)]);
    let url = pages.url("/");
    let tab = c.tab("input", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.until(
        "f.contentDocument && f.contentDocument.getElementById('i') !== null",
        json!(true),
    );
    c.fence(t.tab).unwrap();
    t.viewer.click(70, 45);
    c.editor(t.tab, |s| s.editor != 0);
    t.viewer.tap(0x71);
    t.page
        .until("f.contentDocument.getElementById('i').value", json!("q"));
}
