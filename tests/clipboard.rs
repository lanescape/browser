//! The clipboard respects browsing contexts and private tabs, so one process serves an environment:
//! a context's operations reach only that context's pages, and nothing outlasts its operation.
mod support;
use clipboard_finished::Status::*;
use clipboard_operation::Kind;
use serde_json::json;
use std::{thread, time::Duration};
use support::*;

const CAPABILITIES: &[&str] = &[IME, DEVTOOLS, CLIPBOARD];
const PAGE: &str = "<!doctype html><body style='margin:0'><input id=i style='position:absolute;left:0;top:0;width:150px;height:20px'><input id=j style='position:absolute;left:0;top:40px;width:150px;height:20px'><p id=p style='position:absolute;top:80px'>static text</p><script>window.log=[];window.mode={};for(const t of ['copy','cut','paste'])addEventListener(t,e=>{log.push(t+':'+e.clipboardData.getData('text/plain'));if(mode[t])mode[t](e)},true);window.keys=[];addEventListener('keydown',e=>keys.push(e.key),true)</script></body>";

struct Page {
    t: Controlled,
    editor: u64,
}
fn open(c: &mut Client, profile: &str, private: bool, url: &str) -> Page {
    let tab = c.tab(profile, private, url);
    c.loaded(tab.tab, url);
    let mut t = c.controlled(&tab);
    t.page.eval("i.focus()");
    let editor = c.editor(t.tab, |s| s.editor != 0).editor;
    Page { t, editor }
}
fn start() -> (Client, Pages, Page) {
    let mut c = Client::start(CAPABILITIES);
    let pages = Pages::serve(&[("/", PAGE)]);
    let page = open(&mut c, "clipboard", false, &pages.url("/"));
    (c, pages, page)
}

/// Profiles and private tabs each have their own clipboard, in one host, at the same time.
#[test]
fn every_context_has_its_own_clipboard() {
    let mut c = Client::start(CAPABILITIES);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let mut one = open(&mut c, "one", false, &url);
    let mut two = open(&mut c, "two", false, &url);
    let mut private = open(&mut c, "one", true, &url);
    let mut private_too = open(&mut c, "one", true, &url);

    for (page, text) in [
        (&mut one, "from one"),
        (&mut two, "from two"),
        (&mut private, "from private"),
        (&mut private_too, "from the other private"),
    ] {
        page.t.page.eval(&format!("i.value = '{text}'; i.select()"));
    }
    for (page, text) in [
        (&one, "from one"),
        (&two, "from two"),
        (&private, "from private"),
        (&private_too, "from the other private"),
    ] {
        assert_eq!(
            c.operate(page.t.tab, page.t.id, Kind::Copy, vec![]),
            (Done, Some(text.as_bytes().to_vec()))
        );
    }

    // A paste offers only its own upload, whatever the others copied or their pages wrote.
    assert_eq!(
        two.t.page.gesture(
            "navigator.clipboard.writeText('PAGE WRITE').then(() => 'ok', e => String(e))"
        ),
        json!("ok")
    );
    one.t.page.eval("i.value = ''; log.length = 0");
    let upload = c.upload(one.t.tab, one.t.id, b"upload for one");
    assert_eq!(
        c.operate(one.t.tab, one.t.id, Kind::Paste, vec![upload]),
        (Done, None)
    );
    one.t.page.until("i.value", json!("upload for one"));
    assert_eq!(one.t.page.eval("log.join()"), json!("paste:upload for one"));

    // One context's operations don't touch another's page.
    for page in [&mut two, &mut private, &mut private_too] {
        assert_eq!(
            page.t
                .page
                .eval("log.filter(l => l.startsWith('paste')).length"),
            json!(0)
        );
    }
}

/// One context's slow handler holds up neither another context's operations nor new tabs.
#[test]
fn a_slow_page_holds_up_only_its_own_context() {
    let mut c = Client::start(CAPABILITIES);
    let pages = Pages::serve(&[("/", PAGE)]);
    let mut slow = open(&mut c, "slow", false, &pages.url("/"));
    // Another site, so it has another renderer while this one is busy.
    let mut quick = open(&mut c, "quick", false, &pages.other_site("/"));
    slow.t.page.eval("i.value = 'slow'; i.select(); mode.copy = () => { const s = Date.now(); while (Date.now() - s < 2000); }");
    quick.t.page.eval("i.value = 'quick'; i.select()");
    let held = c.send(request::Command::ClipboardOperation(ClipboardOperation {
        tab: slow.t.tab,
        viewer: slow.t.id,
        kind: Kind::Copy as i32,
        accept: vec!["text/plain".into()],
        ..Default::default()
    }));
    assert!(matches!(c.response(held), response::Result::Ok(_)));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        c.operate(quick.t.tab, quick.t.id, Kind::Copy, vec![]),
        (Done, Some(b"quick".to_vec()))
    );
    assert!(
        !c.events
            .iter()
            .any(|e| matches!(e, event::Event::ClipboardFinished(f) if f.operation == held)),
        "{:?}",
        c.events
    );
    let other = c.tab("third", false, &pages.url("/"));
    c.action(other.tab, tab_operation::Action::Close, 0);
    assert_eq!(
        c.outcome(slow.t.tab, slow.t.id, held),
        (Done, Some(b"slow".to_vec()))
    );
}

#[test]
fn what_copying_reports() {
    let (mut c, _pages, mut page) = start();
    let (tab, viewer) = (page.t.tab, page.t.id);
    // Nothing selected.
    assert_eq!(
        c.operate(tab, viewer, Kind::Copy, vec![]),
        (NothingSelected, None)
    );
    // A selection, and the page refuses it: it took the copy for itself and wrote nothing.
    page.t
        .page
        .eval("i.value = 'secret'; i.select(); mode.copy = e => e.preventDefault()");
    assert_eq!(c.operate(tab, viewer, Kind::Copy, vec![]), (Denied, None));
    // The page's handler can choose the text.
    page.t.page.eval(
        "mode.copy = e => { e.clipboardData.setData('text/plain', 'chosen'); e.preventDefault() }",
    );
    assert_eq!(
        c.operate(tab, viewer, Kind::Copy, vec![]),
        (Done, Some(b"chosen".to_vec()))
    );
    // Formats other than text don't cross.
    page.t.page.eval(
        "mode.copy = e => { e.clipboardData.setData('text/html', '<b>x</b>'); e.preventDefault() }",
    );
    assert_eq!(
        c.operate(tab, viewer, Kind::Copy, vec![]),
        (UnsupportedFormat, None)
    );
    // Text past the limit.
    page.t
        .page
        .eval("mode.copy = null; i.value = 'x'.repeat(1048577); i.select()");
    assert_eq!(c.operate(tab, viewer, Kind::Copy, vec![]), (TooLarge, None));
    // Selected text outside any editor.
    page.t
        .page
        .eval("i.value = ''; i.blur(); getSelection().selectAllChildren(p)");
    c.editor(tab, |s| s.editor == 0);
    assert_eq!(
        c.operate(tab, viewer, Kind::Copy, vec![]),
        (Done, Some(b"static text".to_vec()))
    );
}

#[test]
fn cutting_takes_the_text() {
    let (mut c, _pages, mut page) = start();
    page.t
        .page
        .eval("i.value = 'abcdef'; i.setSelectionRange(2, 4)");
    assert_eq!(
        c.operate(page.t.tab, page.t.id, Kind::Cut, vec![]),
        (Done, Some(b"cd".to_vec()))
    );
    page.t.page.until("i.value", json!("abef"));
}

/// A viewer's paste is a person's: the page has activation, and sees no key it wasn't sent.
#[test]
fn a_paste_is_a_person_s() {
    let (mut c, _pages, mut page) = start();
    page.t
        .page
        .eval("mode.paste = () => { window.active = navigator.userActivation.isActive }");
    let upload = c.upload(page.t.tab, page.t.id, "héllo wörld".as_bytes());
    assert_eq!(
        c.operate(page.t.tab, page.t.id, Kind::Paste, vec![upload]),
        (Done, None)
    );
    page.t.page.until("i.value", json!("héllo wörld"));
    assert_eq!(page.t.page.eval("window.active"), json!(true));
    assert_eq!(page.t.page.eval("keys.join()"), json!(""));

    // Plain paste too, and its text is all the page can read.
    page.t.page.eval("i.value = ''; log.length = 0");
    let upload = c.upload(page.t.tab, page.t.id, b"plain");
    assert_eq!(
        c.operate(page.t.tab, page.t.id, Kind::PastePlain, vec![upload]),
        (Done, None)
    );
    page.t.page.until("i.value", json!("plain"));
    assert_eq!(page.t.page.eval("log.join()"), json!("paste:plain"));
}

/// Typing and pasting land in the order a person did them.
#[test]
fn keys_and_pastes_stay_in_order() {
    let (mut c, _pages, mut page) = start();
    let (tab, viewer) = (page.t.tab, page.t.id);
    page.t.page.eval("i.addEventListener('input', () => { const s = performance.now(); while (performance.now() - s < 20); })");
    let mut sent = 0;
    let mut expected = String::new();
    for n in 0..10u32 {
        let typed = char::from(b'a' + n as u8);
        page.t.viewer.tap(u32::from(typed));
        sent += 2;
        expected.push(typed);
        let word = format!("{n}");
        let upload = c.upload(tab, viewer, word.as_bytes());
        let id = c.send(request::Command::ClipboardOperation(ClipboardOperation {
            tab,
            viewer,
            kind: Kind::Paste as i32,
            transfers: vec![upload],
            after_rfb_input: sent,
            ..Default::default()
        }));
        assert!(matches!(c.response(id), response::Result::Ok(_)));
        assert_eq!(c.outcome(tab, viewer, id), (Done, None));
        expected.push_str(&word);
    }
    page.t.page.until("i.value", json!(expected));
}

/// An operation cancelled while it waits behind earlier input never reaches the page.
#[test]
fn a_cancelled_operation_never_runs() {
    let (mut c, _pages, mut page) = start();
    let (tab, viewer) = (page.t.tab, page.t.id);
    page.t.page.eval("i.value = 'keep me'; i.select(); window.cuts = 0; addEventListener('cut', () => cuts++, true)");
    // The renderer is busy, so a key has no answer yet and the cut waits behind it.
    page.t.page.send_without_answer(
        "Runtime.evaluate",
        json!({"expression": "const s = Date.now(); while (Date.now() - s < 1500);"}),
    );
    thread::sleep(Duration::from_millis(100));
    page.t.viewer.tap(0xffe1);
    let id = c.send(request::Command::ClipboardOperation(ClipboardOperation {
        tab,
        viewer,
        kind: Kind::Cut as i32,
        accept: vec!["text/plain".into()],
        after_rfb_input: 2,
        ..Default::default()
    }));
    assert!(matches!(c.response(id), response::Result::Ok(_)));
    c.action(tab, tab_operation::Action::Control, 0);
    assert_eq!(c.outcome(tab, viewer, id).0, Cancelled);
    // Well after the renderer is back.
    thread::sleep(Duration::from_millis(2000));
    assert_eq!(page.t.page.eval("[i.value, cuts]"), json!(["keep me", 0]));
}

/// The data exists only for the frame running the operation, and only while it runs.
#[test]
fn nothing_outlasts_an_operation() {
    let (mut c, _pages, mut page) = start();
    let upload = c.upload(page.t.tab, page.t.id, b"once");
    assert_eq!(
        c.operate(page.t.tab, page.t.id, Kind::Paste, vec![upload]),
        (Done, None)
    );
    page.t.page.until("i.value", json!("once"));
    // The page's own attempts afterwards find nothing.
    let read = page
        .t
        .page
        .gesture("navigator.clipboard.readText().then(t => 'read ' + t, e => e.message)");
    assert!(
        read.as_str().unwrap().contains("permission denied"),
        "{read}"
    );
    page.t.page.eval("log.length = 0; i.value = ''");
    assert_eq!(
        page.t.page.gesture("document.execCommand('paste')"),
        json!(false)
    );
    assert_eq!(page.t.page.eval("i.value"), json!(""));
    // Ctrl+V over RFB pastes nothing, since no viewer owns a clipboard here; a marker after it
    // shows it has been handled.
    page.t.viewer.key(true, 0xffe3);
    page.t.viewer.tap(0x76);
    page.t.viewer.key(false, 0xffe3);
    page.t.viewer.tap(0x7a);
    page.t.page.until("i.value.endsWith('z')", json!(true));
    assert_eq!(
        page.t.page.eval("i.value + log.join() + '/' + keys.join()"),
        json!("z/Control,z")
    );
    // A write outside any operation isn't kept for the next one.
    assert_eq!(
        page.t
            .page
            .gesture("navigator.clipboard.writeText('stray').then(() => 'ok', e => String(e))"),
        json!("ok")
    );
    assert_eq!(
        c.operate(page.t.tab, page.t.id, Kind::Copy, vec![]),
        (NothingSelected, None)
    );
}

/// An upload belongs to the focus it was made for.
#[test]
fn an_upload_dies_with_its_focus() {
    let (mut c, _pages, mut page) = start();
    let (tab, viewer) = (page.t.tab, page.t.id);
    let upload = c.upload(tab, viewer, b"for i");
    page.t.page.eval("j.focus()");
    c.editor(tab, |s| s.editor != 0 && s.editor != page.editor);
    let operation = request::Command::ClipboardOperation(ClipboardOperation {
        tab,
        viewer,
        kind: Kind::Paste as i32,
        transfers: vec![upload],
        ..Default::default()
    });
    c.refused(operation, "unknown clipboard transfer");
    assert_eq!(page.t.page.eval("j.value"), json!(""));
}

/// Pasting into an iframe reaches that frame alone, across a process boundary.
#[test]
fn a_paste_reaches_only_the_focused_frame() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><input id=i><iframe id=f src='http://localhost:{port}/frame' style='width:200px;height:100px'></iframe><script>window.log=[];addEventListener('paste',e=>log.push(e.clipboardData.getData('text/plain')),true)</script></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("clipboard", false, &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.attach_frames();
    let frame = t.page.frame_session(&pages.other_site("/frame"));
    t.page.eval_in(Some(&frame), "i.focus()", false);
    c.editor(t.tab, |s| s.editor != 0);
    let upload = c.upload(t.tab, t.id, b"into the frame");
    assert_eq!(
        c.operate(t.tab, t.id, Kind::Paste, vec![upload]),
        (Done, None)
    );
    t.page
        .until_in(Some(&frame), "i.value", json!("into the frame"));
    assert_eq!(t.page.eval("log.join() + i.value"), json!(""));
    // Copying there comes back too.
    t.page.eval_in(Some(&frame), "i.select()", false);
    assert_eq!(
        c.operate(t.tab, t.id, Kind::Copy, vec![]),
        (Done, Some(b"into the frame".to_vec()))
    );
}

/// Likewise in a frame sharing its parent's process.
#[test]
fn a_paste_reaches_a_same_process_iframe_alone() {
    let mut c = Client::start(CAPABILITIES);
    let main = "<!doctype html><body style='margin:0'><input id=i><iframe id=f src='/frame' style='width:200px;height:100px'></iframe></body>";
    let pages = Pages::serve(&[("/", main), ("/frame", PAGE)]);
    let url = pages.url("/");
    let tab = c.tab("clipboard", false, &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.page.until(
        "f.contentDocument && f.contentDocument.getElementById('i') !== null",
        json!(true),
    );
    let inner = "f.contentDocument.getElementById('i')";
    t.page
        .eval(&format!("f.contentWindow.focus(); {inner}.focus()"));
    c.editor(t.tab, |s| s.editor != 0);
    let upload = c.upload(t.tab, t.id, b"into the frame");
    assert_eq!(
        c.operate(t.tab, t.id, Kind::Paste, vec![upload]),
        (Done, None)
    );
    t.page
        .until(&format!("{inner}.value"), json!("into the frame"));
    assert_eq!(t.page.eval("i.value"), json!(""));
    t.page.eval(&format!("{inner}.select()"));
    assert_eq!(
        c.operate(t.tab, t.id, Kind::Copy, vec![]),
        (Done, Some(b"into the frame".to_vec()))
    );
}

/// Selecting text puts it nowhere: a middle click finds nothing to paste, in that tab or another,
/// though on Linux a selection usually goes to the system's own selection clipboard.
#[test]
fn a_selection_reaches_no_other_page() {
    let mut c = Client::start(CAPABILITIES);
    let pages = Pages::serve(&[("/", PAGE)]);
    let url = pages.url("/");
    let mut one = open(&mut c, "one", false, &url);
    let mut two = open(&mut c, "two", false, &url);
    one.t.page.eval("i.value = 'selected'; i.select()");
    assert_eq!(
        one.t.page.eval("getSelection().toString()"),
        json!("selected")
    );
    for page in [&mut one, &mut two] {
        page.t.page.eval("j.focus()");
        // A middle click on the second input, then a marker to show it has been handled.
        page.t.viewer.pointer(2, 10, 50);
        page.t.viewer.pointer(0, 10, 50);
        page.t.viewer.tap(0x7a);
        page.t.page.until("j.value", json!("z"));
    }
}

/// Only the viewer holding CONTROL can run operations, and losing it ends them.
#[test]
fn only_the_controller_operates() {
    let (mut c, _pages, page) = start();
    let (tab, viewer) = (page.t.tab, page.t.id);
    let operation = |viewer| {
        request::Command::ClipboardOperation(ClipboardOperation {
            tab,
            viewer,
            kind: Kind::Copy as i32,
            accept: vec!["text/plain".into()],
            ..Default::default()
        })
    };
    c.refused(operation(viewer + 100), "CONTROL");
    let upload = c.upload(tab, viewer, b"x");
    c.action(tab, tab_operation::Action::Control, 0);
    c.refused(operation(viewer), "CONTROL");
    // Its uploads went with it: it holds CONTROL again, and they're gone.
    c.action(tab, tab_operation::Action::Control, viewer);
    c.refused(
        request::Command::ClipboardOperation(ClipboardOperation {
            tab,
            viewer,
            kind: Kind::Paste as i32,
            transfers: vec![upload],
            ..Default::default()
        }),
        "unknown clipboard transfer",
    );
}
