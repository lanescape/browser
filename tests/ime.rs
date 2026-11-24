//! IME must be exact: a composition appears in the page as with a local input method, in whichever
//! frame holds focus, and the host reports exactly where each of its characters is.
mod support;
use ime_input::Action;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use support::*;

const CAPABILITIES: &[&str] = &[IME, DEVTOOLS];
const EDITOR: &str = "<!doctype html><body style='margin:0'><input id=i style='position:absolute;left:10px;top:20px;width:200px;height:24px'><input id=j style='position:absolute;left:10px;top:60px;width:200px;height:24px'><script>window.log=[];for(const t of ['compositionstart','compositionupdate','compositionend'])i.addEventListener(t,e=>log.push(t+':'+e.data))</script></body>";
/// Monospace, so different text of one length lays out just the same.
const EDITABLE: &str = "<!doctype html><body style='margin:0'><div id=d contenteditable style='position:absolute;left:30px;top:40px;width:300px;height:40px;font:20px monospace'></div></body>";
/// A range over the text's nth UTF-16 unit, measured by the page.
const CHARACTER: &str = "(n => { const t = document.getElementById('d').firstChild; const r = document.createRange(); r.setStart(t, n); r.setEnd(t, n + 1); const b = r.getBoundingClientRect(); return [b.x, b.y, b.width, b.height] })";

fn start(pages: &[(&str, &str)]) -> (Client, Pages, Tab, Controlled) {
    let mut client = Client::start(CAPABILITIES);
    let pages = Pages::serve(pages);
    let url = pages.url("/");
    let tab = client.tab("ime", &url);
    client.loaded(tab.tab, &url);
    let controlled = client.controlled(&tab);
    (client, pages, tab, controlled)
}

#[test]
fn a_composition_runs_in_the_page() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    assert!(editor.inline_composition);

    c.ok(ime(&t, editor.editor, Action::Preedit, "かな"));
    t.page.until("i.value", json!("かな"));
    c.editor(t.tab, |s| s.composing);
    c.ok(ime(&t, editor.editor, Action::Commit, "漢字"));
    t.page.until("i.value", json!("漢字"));
    c.editor(t.tab, |s| !s.composing && s.editor == editor.editor);
    assert_eq!(
        t.page.eval("log.join()"),
        json!(
            "compositionstart:,compositionupdate:かな,compositionupdate:漢字,compositionend:漢字"
        )
    );

    // Finishing keeps what's there, and cancelling removes it.
    t.page.eval("log.length = 0; i.value = ''");
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    t.page.until("i.value", json!("abc"));
    c.ok(ime(&t, editor.editor, Action::Finish, ""));
    t.page
        .until("log[log.length - 1]", json!("compositionend:abc"));
    assert_eq!(t.page.eval("i.value"), json!("abc"));
    t.page.eval("i.value = ''");
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    t.page.until("i.value", json!("abc"));
    c.ok(ime(&t, editor.editor, Action::Cancel, ""));
    t.page.until("i.value", json!(""));
}

/// A person's caret inside the composition, not only at its end.
#[test]
fn a_composition_has_its_selection_and_underlines() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(request::Command::ImeInput(ImeInput {
        tab: t.tab,
        viewer: t.id,
        editor: editor.editor,
        action: Action::Preedit as i32,
        text: "abcd".into(),
        selection_start: 1,
        selection_end: 3,
        underlines: vec![ImeUnderline {
            start: 0,
            end: 4,
            thick: true,
            ..Default::default()
        }],
        ..Default::default()
    }));
    t.page.until(
        "[i.value, i.selectionStart, i.selectionEnd]",
        json!(["abcd", 1, 3]),
    );
    // Out of range for its own text: refused before anything reaches the page.
    c.refused(
        request::Command::ImeInput(ImeInput {
            tab: t.tab,
            viewer: t.id,
            editor: editor.editor,
            action: Action::Preedit as i32,
            text: "ab".into(),
            selection_start: 1,
            selection_end: 3,
            ..Default::default()
        }),
        "IME selection out of range",
    );
}

/// Each character's rectangle is the one the page measures.
#[test]
fn bounds_are_the_page_s_own_characters() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITABLE)]);
    t.page.eval("d.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let state = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    for (n, bounds) in state.character_bounds.iter().enumerate() {
        assert_near(bounds, &t.page.eval(&format!("{CHARACTER}({n})")));
    }
    // The caret is after the last character, as tall as it, and as wide as Blink draws it.
    let last = state.character_bounds[2];
    let caret = state.caret.unwrap();
    assert_eq!((caret.x, caret.y), (last.x + last.width, last.y));
    assert_eq!((caret.width, caret.height), (1, last.height));
}

/// Bounds come with each request's answer: the state is never left waiting on the page to repaint.
#[test]
fn bounds_follow_every_change_at_once() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITABLE)]);
    t.page.eval("d.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    for text in ["a", "ab", "abc", "abcd", "abc", "xyz", "x"] {
        let sent = Instant::now();
        c.ok(ime(&t, editor.editor, Action::Preedit, text));
        let n = text.len();
        // Text of the same length laid out the same, as here in monospace, changes no state at all.
        if text != "xyz" {
            let state = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == n);
            assert!(
                sent.elapsed() < Duration::from_secs(2),
                "{text}: {:?}",
                sent.elapsed()
            );
            for (i, bounds) in state.character_bounds.iter().enumerate() {
                assert_near(bounds, &t.page.eval(&format!("{CHARACTER}({i})")));
            }
        }
    }
}

/// The caret is where Blink paints it, in any direction and anywhere in the composition.
#[test]
fn the_caret_is_the_one_painted() {
    let page = "<!doctype html><body style='margin:0'><div id=d contenteditable style='position:absolute;left:30px;top:40px;width:300px;height:40px;padding:0 8px;font:20px monospace;caret-color:#ff0000'></div></body>";
    for (text, dir) in [
        ("abc", "ltr"),
        ("אבג", "rtl"),
        ("ab1", "ltr"),
        ("אב1", "rtl"),
    ] {
        let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
        t.page.eval(&format!("d.dir = '{dir}'; d.focus()"));
        let editor = c.editor(t.tab, |s| s.editor != 0);
        for at in [0, 1, 3] {
            c.ok(request::Command::ImeInput(ImeInput {
                tab: t.tab,
                viewer: t.id,
                editor: editor.editor,
                action: Action::Preedit as i32,
                text: text.into(),
                selection_start: at,
                selection_end: at,
                ..Default::default()
            }));
            t.page.until("getSelection().anchorOffset", json!(at));
            c.collect(Duration::from_millis(50));
            let state = c
                .events
                .iter()
                .rev()
                .find_map(|e| match e {
                    event::Event::EditorState(s) if s.composing => Some(s.clone()),
                    _ => None,
                })
                .unwrap();
            let caret = state.caret.unwrap();
            // The caret blinks, so it's in some frames.
            let row = (caret.y + caret.height / 2) as usize;
            let deadline = Instant::now() + PATIENCE;
            let painted = loop {
                t.viewer.request(false);
                t.viewer.update();
                let found = (0..t.viewer.width as usize).find(|x| {
                    let [r, g, b] = t.viewer.at(*x, row);
                    r > 200 && g < 60 && b < 60
                });
                if let Some(x) = found {
                    break x as i32;
                }
                assert!(
                    Instant::now() < deadline,
                    "{text} at {at}: no caret painted"
                );
                std::thread::sleep(Duration::from_millis(30));
            };
            assert!(
                (caret.x - painted).abs() <= 2,
                "{text} at {at}: reported {caret:?}, painted at x={painted}"
            );
        }
    }
}

/// Cancelling with nothing composing leaves what's selected alone.
#[test]
fn a_cancel_with_no_composition_changes_nothing() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.value = 'selected'; i.focus(); i.select()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Cancel, ""));
    // An empty preedit is the same thing.
    c.ok(ime(&t, editor.editor, Action::Preedit, ""));
    assert_eq!(t.page.eval("i.value"), json!("selected"));
    assert_eq!(
        t.page.eval("[i.selectionStart, i.selectionEnd]"),
        json!([0, 8])
    );
}

/// A renderer that never gets to a request gets an answer for it all the same, and what follows
/// isn't held up forever.
#[test]
fn a_hung_renderer_gets_an_answer() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    t.page.send_without_answer(
        "Runtime.evaluate",
        json!({"expression": "const s = Date.now(); while (Date.now() - s < 9000);"}),
    );
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    c.refused(
        ime(&t, editor.editor, Action::Commit, "late"),
        "not the current editor",
    );
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "{:?}",
        start.elapsed()
    );
    // Once it's back, what it was too late for hasn't happened, and it works again.
    t.page.until("i.value", json!(""));
    c.ok(ime(&t, editor.editor, Action::Commit, "yes"));
    t.page.until("i.value", json!("yes"));
}

/// Cleaning up a composition isn't a request that can come too late: however long the renderer
/// takes to get to it, it doesn't stay composing in the page.
#[test]
fn a_cleanup_cancel_outlasts_a_busy_renderer() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    t.page.until("i.value", json!("abc"));
    t.page.send_without_answer(
        "Runtime.evaluate",
        json!({"expression": "const s = Date.now(); while (Date.now() - s < 3000);"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    c.action(t.tab, tab_operation::Action::Control, 0);
    t.page.until("i.value", json!(""));
}

/// A request that comes too late leaves what's known of the composition alone, so it's still
/// followed afterwards.
#[test]
fn a_refused_request_leaves_the_composition_followed() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITABLE)]);
    t.page.eval("d.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let before = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    t.page.send_without_answer(
        "Runtime.evaluate",
        json!({"expression": "const s = Date.now(); while (Date.now() - s < 4000);"}),
    );
    std::thread::sleep(Duration::from_millis(100));
    // A key with no answer yet holds the request past its deadline.
    t.viewer.tap(0xffe1);
    let late = request::Command::ImeInput(ImeInput {
        after_rfb_input: 2,
        ..match ime(&t, editor.editor, Action::Preedit, "abcd") {
            request::Command::ImeInput(input) => input,
            _ => unreachable!(),
        }
    });
    c.refused(late, "not the current editor");
    t.page.until("d.textContent", json!("abc"));
    t.page.eval("d.style.left = '80px'");
    let after = c.editor(t.tab, |s| {
        s.composing
            && s.character_bounds.len() == 3
            && s.character_bounds[0].x != before.character_bounds[0].x
    });
    assert_eq!(
        after.character_bounds[0].x,
        before.character_bounds[0].x + 50
    );
}

/// A preedit the host gave up on, whose page was only slow, is still a composition to clean up.
#[test]
fn a_preedit_that_finishes_late_is_still_cleaned_up() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("j.focus(); j.addEventListener('compositionstart', () => { const s = Date.now(); while (Date.now() - s < 6000); })");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.refused(
        ime(&t, editor.editor, Action::Preedit, "abc"),
        "not the current editor",
    );
    t.page.until("j.value", json!("abc"));
    c.editor(t.tab, |s| s.composing);
    c.action(t.tab, tab_operation::Action::Control, 0);
    t.page.until("j.value", json!(""));
}

/// Likewise when CONTROL has gone before anything is known of the composition.
#[test]
fn control_leaving_cleans_up_a_preedit_still_starting() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("j.focus(); j.addEventListener('compositionstart', () => { const s = Date.now(); while (Date.now() - s < 6000); })");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.refused(
        ime(&t, editor.editor, Action::Preedit, "abc"),
        "not the current editor",
    );
    // The host has given up, and the handler is still running.
    c.action(t.tab, tab_operation::Action::Control, 0);
    std::thread::sleep(Duration::from_secs(2));
    t.page.until("j.value", json!(""));
}

/// The page's own layout moving a composition is followed too.
#[test]
fn bounds_follow_the_page_s_layout() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITABLE)]);
    t.page.eval("d.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let before = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    t.page.eval("d.style.left = '80px'");
    let after = c.editor(t.tab, |s| {
        s.composing
            && s.character_bounds.len() == 3
            && s.character_bounds[0].x != before.character_bounds[0].x
    });
    assert_eq!(
        after.character_bounds[0].x,
        before.character_bounds[0].x + 50
    );
    assert_near(
        &after.character_bounds[1],
        &t.page.eval(&format!("{CHARACTER}(1)")),
    );
}

/// A parent moving an out-of-process iframe moves its composition, with nothing said in the iframe.
#[test]
fn bounds_follow_an_iframe_its_parent_moves() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='http://localhost:{port}/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", FRAME_EDITOR)]);
    let url = pages.url("/");
    let tab = c.tab("ime", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let frame = t.page.frame_session(&pages.other_site("/frame"));
    t.page.eval_in(Some(&frame), "d.focus()", false);
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let before = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    t.page.eval("f.style.left = '100px'");
    let after = c.editor(t.tab, |s| {
        s.composing
            && s.character_bounds.len() == 3
            && s.character_bounds[0].x != before.character_bounds[0].x
    });
    assert_eq!(
        after.character_bounds[0].x,
        before.character_bounds[0].x + 50
    );
}

/// Scrolling moves a composition, and its state follows without another request.
#[test]
fn bounds_follow_the_page_when_it_moves() {
    let page = "<!doctype html><body style='margin:0;height:3000px'><div id=d contenteditable style='position:absolute;left:30px;top:40px;width:300px;height:40px;font:20px monospace'></div></body>";
    let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
    t.page.eval("d.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let before = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    t.page.eval("scrollTo(0, 25)");
    let after = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    assert_eq!(
        after.character_bounds[0].y,
        before.character_bounds[0].y - 25
    );
    assert_near(
        &after.character_bounds[2],
        &t.page.eval(&format!("{CHARACTER}(2)")),
    );
}

/// A person's typing and their input method's text land in the order they came, whatever the page
/// is doing meanwhile.
#[test]
fn keys_and_ime_stay_in_order() {
    let page = "<!doctype html><body><input id=i><script>let n=0;i.addEventListener('input',()=>{const s=performance.now();while(performance.now()-s<20);})</script></body>";
    let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    let mut sent = 0;
    let mut expected = String::new();
    for n in 0..20u32 {
        let typed = char::from(b'a' + (n % 26) as u8);
        t.viewer.tap(u32::from(typed));
        sent += 2;
        expected.push(typed);
        let word = format!("{n}");
        let id = c.send(request::Command::ImeInput(ImeInput {
            after_rfb_input: sent,
            ..match ime(&t, editor.editor, Action::Commit, &word) {
                request::Command::ImeInput(input) => input,
                _ => unreachable!(),
            }
        }));
        expected.push_str(&word);
        assert!(matches!(c.response(id), response::Result::Ok(_)));
    }
    t.page.until("i.value", json!(expected));
}

const FRAME_EDITOR: &str = "<!doctype html><body style='margin:0'><div id=d contenteditable style='position:absolute;left:5px;top:6px;width:200px;height:30px;font:20px monospace'></div><script>window.log=[];d.addEventListener('compositionstart',()=>log.push('start'))</script></body>";

/// An editor in an out-of-process iframe is as good as one in the main frame.
#[test]
fn a_composition_runs_in_an_out_of_process_iframe() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='http://localhost:{port}/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe><script>window.log=[];addEventListener('compositionstart',()=>log.push('start'),true)</script></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", FRAME_EDITOR)]);
    let url = pages.url("/");
    let tab = c.tab("ime", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let frame = t.page.frame_session(&pages.other_site("/frame"));
    t.page.eval_in(Some(&frame), "d.focus()", false);
    let editor = c.editor(t.tab, |s| s.editor != 0);
    assert!(editor.inline_composition);
    // The editor's rectangle is in the tab's viewport, not its own frame's.
    let caret = editor.caret.unwrap();
    assert_eq!(
        (caret.x, caret.y, caret.width, caret.height),
        (55, 36, 200, 30)
    );

    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    t.page.until_in(Some(&frame), "d.textContent", json!("abc"));
    let state = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    for (n, bounds) in state.character_bounds.iter().enumerate() {
        let inside = t
            .page
            .eval_in(Some(&frame), &format!("{CHARACTER}({n})"), false);
        let inside = inside.as_array().unwrap();
        let expected: Value = json!([
            inside[0].as_f64().unwrap() + 50.0,
            inside[1].as_f64().unwrap() + 30.0,
            inside[2],
            inside[3]
        ]);
        assert_near(bounds, &expected);
    }
    c.ok(ime(&t, editor.editor, Action::Commit, "漢字"));
    t.page
        .until_in(Some(&frame), "d.textContent", json!("漢字"));
    assert_eq!(
        t.page.eval_in(Some(&frame), "log.join()", false),
        json!("start")
    );
    assert_eq!(t.page.eval("log.join()"), json!(""));
}

/// An editor in a frame sharing its parent's process and widget is as good as any other.
#[test]
fn a_composition_runs_in_a_same_process_iframe() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", FRAME_EDITOR)]);
    let url = pages.url("/");
    let tab = c.tab("ime", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.until(
        "f.contentDocument && f.contentDocument.getElementById('d') !== null",
        json!(true),
    );
    t.page
        .eval("f.contentWindow.focus(); f.contentDocument.getElementById('d').focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    let caret = editor.caret.unwrap();
    assert_eq!(
        (caret.x, caret.y, caret.width, caret.height),
        (55, 36, 200, 30)
    );
    c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
    let inner = "f.contentDocument.getElementById('d').textContent";
    t.page.until(inner, json!("abc"));
    let state = c.editor(t.tab, |s| s.composing && s.character_bounds.len() == 3);
    let character = CHARACTER.replace("document", "f.contentDocument");
    for (n, bounds) in state.character_bounds.iter().enumerate() {
        let inside = t.page.eval(&format!("{character}({n})"));
        let inside = inside.as_array().unwrap();
        assert_near(
            bounds,
            &json!([
                inside[0].as_f64().unwrap() + 50.0,
                inside[1].as_f64().unwrap() + 30.0,
                inside[2],
                inside[3]
            ]),
        );
    }
    let caret = state.caret.unwrap();
    assert!(
        caret.x >= state.character_bounds[2].x && caret.y >= 30,
        "{caret:?}"
    );
    c.ok(ime(&t, editor.editor, Action::Commit, "漢字"));
    t.page.until(inner, json!("漢字"));
}

/// A frame gives up its focused element along with focus, in or out of process, so an editor retired
/// when focus left is gone for good: focus coming back finds nothing to take text.
#[test]
fn focus_leaving_a_frame_takes_its_editor_for_good() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><iframe id=f src='/frame' style='position:absolute;left:50px;top:30px;width:250px;height:100px;border:0'></iframe><iframe id=g src='http://localhost:{port}/frame' style='position:absolute;left:50px;top:200px;width:250px;height:100px;border:0'></iframe></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", FRAME_EDITOR)]);
    let url = pages.url("/");
    let tab = c.tab("ime", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let other = t.page.frame_session(&pages.other_site("/frame"));
    t.page.until(
        "f.contentDocument && f.contentDocument.getElementById('d') !== null",
        json!(true),
    );
    let active = "f.contentDocument.activeElement.id";
    let focus = "f.contentWindow.focus(); f.contentDocument.getElementById('d').focus()";

    // To the main frame, in the same process.
    t.page.eval(focus);
    let first = c.editor(t.tab, |s| s.editor != 0);
    t.page.eval("window.focus()");
    c.editor(t.tab, |s| s.editor == 0);
    assert_eq!(t.page.eval(active), json!(""));

    // To another process.
    t.page.eval(focus);
    let second = c.editor(t.tab, |s| s.editor != 0 && s.editor != first.editor);
    t.page.eval_in(Some(&other), "d.focus()", false);
    let third = c.editor(t.tab, |s| s.editor != 0 && s.editor != second.editor);
    assert_eq!(t.page.eval(active), json!(""));
    // Passing through no editor on the way there says nothing about what comes next.
    c.collect(Duration::from_millis(300));
    c.forget_events();

    // Returning takes nothing but focus.
    t.page.eval("f.contentWindow.focus()");
    c.editor(t.tab, |s| s.editor == 0);
    assert_eq!(t.page.eval(active), json!(""));
    assert_ne!(third.editor, first.editor);
}

/// A transformed out-of-process iframe's editor is reported where it's drawn, at the size it's drawn.
#[test]
fn bounds_follow_an_iframe_s_transform() {
    type Map = fn([f64; 4]) -> [f64; 4];
    let cases: [(&str, Map); 2] = [
        ("scale(2)", |[x, y, w, h]| {
            [300.0 + 2.0 * x, 30.0 + 2.0 * y, 2.0 * w, 2.0 * h]
        }),
        ("rotate(90deg)", |[x, y, w, h]| {
            [300.0 - y - h, 30.0 + x, h, w]
        }),
    ];
    // A pixel either way in the page's measure, scaled.
    let near = |r: &Rect, e: &[f64; 4]| {
        [r.x, r.y, r.width, r.height]
            .iter()
            .zip(e)
            .all(|(a, b)| (f64::from(*a) - b).abs() <= 2.5)
    };
    for (transform, map) in cases {
        let mut c = Client::start(CAPABILITIES);
        let main = format!(
            "<!doctype html><body style='margin:0'><iframe id=f src='http://localhost:{{port}}/frame' style='position:absolute;left:300px;top:30px;width:250px;height:100px;border:0;transform-origin:0 0;transform:{transform}'></iframe></body>"
        );
        let pages = Pages::serve(&[("/", main.as_str()), ("/frame", FRAME_EDITOR)]);
        let url = pages.url("/");
        let tab = c.tab("ime", &url);
        c.loaded(tab.tab, &url);
        let mut t = c.controlled(&tab);
        t.page.attach_frames();
        let frame = t.page.frame_session(&pages.other_site("/frame"));
        t.page.eval_in(Some(&frame), "d.focus()", false);
        let editor = c.editor(t.tab, |s| s.editor != 0);
        c.ok(ime(&t, editor.editor, Action::Preedit, "abc"));
        t.page.until_in(Some(&frame), "d.textContent", json!("abc"));
        let expected: Vec<[f64; 4]> = (0..3)
            .map(|n| {
                let inside = t
                    .page
                    .eval_in(Some(&frame), &format!("{CHARACTER}({n})"), false);
                let inside = inside.as_array().unwrap();
                map(std::array::from_fn(|i| inside[i].as_f64().unwrap()))
            })
            .collect();
        c.editor(t.tab, |s| {
            s.composing
                && s.character_bounds.len() == 3
                && s.character_bounds
                    .iter()
                    .zip(&expected)
                    .all(|(r, e)| near(r, e))
        });
        c.ok(ime(&t, editor.editor, Action::Commit, ""));

        // The iframe has been drawn by now, so its transform is known from the first report.
        t.page.eval_in(Some(&frame), "d.blur()", false);
        c.editor(t.tab, |s| s.editor == 0);
        t.page.eval_in(Some(&frame), "d.focus()", false);
        let again = c.editor(t.tab, |s| s.editor != 0 && s.editor != editor.editor);
        let caret = again.caret.unwrap();
        let drawn = map([5.0, 6.0, 200.0, 30.0]);
        assert!(
            near(&caret, &drawn),
            "{transform}: {caret:?}, not {drawn:?}"
        );
    }
}

/// An editor is what Chromium would give a local input method.
#[test]
fn an_editor_is_what_takes_text() {
    let page = "<!doctype html><body><input id=text><input id=password type=password><input id=search type=search><textarea id=area></textarea><div id=rich contenteditable>x</div><input id=readonly readonly><textarea id=readonlyarea readonly></textarea><input id=check type=checkbox><button id=button>b</button><div id=role role=textbox tabindex=0>r</div><div id=off contenteditable=false tabindex=0>o</div><a id=link href='#'>l</a></body>";
    let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
    for id in ["text", "password", "search", "area", "rich"] {
        t.page.eval(&format!("{id}.focus()"));
        c.editor(t.tab, |s| s.editor != 0);
        t.page.eval("document.activeElement.blur()");
        c.editor(t.tab, |s| s.editor == 0);
    }
    for id in [
        "readonly",
        "readonlyarea",
        "check",
        "button",
        "role",
        "off",
        "link",
    ] {
        // From a real editor, so a state follows whichever way the answer goes.
        t.page.eval("text.focus()");
        c.editor(t.tab, |s| s.editor != 0);
        t.page.eval(&format!("{id}.focus()"));
        let state = c.editor(t.tab, |_| true);
        let state = if state.editor != 0 {
            c.editor(t.tab, |_| true)
        } else {
            state
        };
        assert_eq!(state.editor, 0, "{id}");
        assert_eq!(t.page.eval("document.activeElement.id"), json!(id));
    }
}

/// In a document in design mode, a click makes the caret that takes text, with no focus change.
#[test]
fn a_document_in_design_mode_is_an_editor_once_it_has_a_caret() {
    let page = "<!doctype html><body style='margin:0;height:200px'><p id=p style='margin:0'>some text</p></body>";
    let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
    t.page.eval("document.designMode = 'on'");
    t.viewer.click(10, 8);
    let editor = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, editor.editor, Action::Commit, "X"));
    t.page.until("p.textContent.includes('X')", json!(true));
}

/// A field that stops taking text with focus still in it stops being an editor.
#[test]
fn a_field_made_readonly_ends_its_editor() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    t.page.eval("i.readOnly = true");
    c.editor(t.tab, |s| s.editor == 0);
    c.refused(
        ime(&t, editor.editor, Action::Commit, "no"),
        "not the current editor",
    );
    t.page.eval("i.readOnly = false");
    let again = c.editor(t.tab, |s| s.editor != 0);
    c.ok(ime(&t, again.editor, Action::Commit, "yes"));
    t.page.until("i.value", json!("yes"));
}

/// Deciding what's an editor runs none of the page's code.
#[test]
fn deciding_runs_no_page_script() {
    let page = "<!doctype html><body><div id=d contenteditable>x</div><input id=i><script>window.ran=0;Object.defineProperty(document,'designMode',{get(){window.ran++;return 'off'}})</script></body>";
    let (mut c, _pages, _tab, mut t) = start(&[("/", page)]);
    for id in ["d", "i", "d"] {
        t.page.eval(&format!("{id}.focus()"));
        c.editor(t.tab, |s| s.editor != 0);
    }
    assert_eq!(t.page.eval("window.ran"), json!(0));
}

/// A request names the editor it's for, and focus can move first.
#[test]
fn a_request_for_another_editor_is_refused() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let first = c.editor(t.tab, |s| s.editor != 0);
    t.page.eval("j.focus()");
    let second = c.editor(t.tab, |s| s.editor != 0 && s.editor != first.editor);
    c.refused(
        ime(&t, first.editor, Action::Commit, "no"),
        "not the current editor",
    );
    c.ok(ime(&t, second.editor, Action::Commit, "yes"));
    t.page.until("j.value", json!("yes"));
    assert_eq!(t.page.eval("i.value"), json!(""));

    // A script moving focus twice leaves one editor to talk to, wherever it ended.
    c.events.clear();
    t.page.eval("i.focus(); j.focus(); i.focus()");
    let state = c.editor(t.tab, |s| s.editor != 0 && s.editor != second.editor);
    c.ok(ime(&t, state.editor, Action::Commit, "i"));
    t.page.until("i.value", json!("i"));
}

/// Input over RFB comes before the IME request that names it, as a person's keys would.
#[test]
fn a_request_follows_the_keys_before_it() {
    let (mut c, _pages, _tab, mut t) = start(&[("/", EDITOR)]);
    t.page.eval("i.focus()");
    let first = c.editor(t.tab, |s| s.editor != 0);
    // Tab moves focus to the next input, so a request after it for the old editor has no target.
    t.viewer.tap(0xff09);
    let late = request::Command::ImeInput(ImeInput {
        after_rfb_input: 2,
        ..match ime(&t, first.editor, Action::Commit, "no") {
            request::Command::ImeInput(input) => input,
            _ => unreachable!(),
        }
    });
    c.refused(late, "not the current editor");
    assert_eq!(t.page.eval("i.value"), json!(""));
    t.page.until("document.activeElement.id", json!("j"));
}

/// Another frame taking focus ends the editor, though its own document still has it focused.
#[test]
fn focus_in_another_frame_ends_the_editor() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><input id=i><iframe id=f src='http://localhost:{port}/frame' style='width:200px;height:100px'></iframe></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", "<!doctype html><body>frame</body>")]);
    let url = pages.url("/");
    let tab = c.tab("ime", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let frame = t.page.frame_session(&pages.other_site("/frame"));
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0);
    t.page.eval_in(Some(&frame), "window.focus()", false);
    c.editor(t.tab, |s| s.editor == 0);
    c.refused(
        ime(&t, editor.editor, Action::Commit, "no"),
        "not the current editor",
    );
    assert_eq!(t.page.eval("i.value"), json!(""));
}

/// Navigating ends the editor; moving CONTROL takes it from its old holder.
#[test]
fn an_editor_ends_with_its_document_and_its_viewer() {
    let (mut c, pages, tab, mut t) = start(&[("/", EDITOR), ("/next", EDITOR)]);
    t.page.eval("i.focus()");
    let first = c.editor(t.tab, |s| s.editor != 0);
    let next = pages.url("/next");
    c.navigate(t.tab, &next);
    c.editor(t.tab, |s| s.editor == 0);
    c.refused(
        ime(&t, first.editor, Action::Commit, "no"),
        "not the current editor",
    );

    t.page.eval("i.focus()");
    let second = c.editor(t.tab, |s| s.editor != 0);
    let _other = Viewer::connect(&tab, false);
    let other = c.viewer_id(t.tab);
    let other = if other == t.id {
        c.viewer_id(t.tab)
    } else {
        other
    };
    c.action(t.tab, tab_operation::Action::Control, other);
    c.editor(t.tab, |s| s.viewer == t.id && s.editor == 0);
    c.refused(ime(&t, second.editor, Action::Commit, "no"), "CONTROL");
    let state = c.editor(t.tab, |s| s.viewer == other && s.editor != 0);
    assert_ne!(state.editor, second.editor);
}
