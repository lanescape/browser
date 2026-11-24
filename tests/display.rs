//! What a viewer sees over RFB is exactly what the page drew, and costs only what changed.
mod support;
use serde_json::json;
use std::time::{Duration, Instant};
use support::*;

const QUADRANTS: &str = "<!doctype html><body style='margin:0'><div style='position:absolute;left:0;top:0;width:200px;height:150px;background:rgb(200,20,40)'></div><div style='position:absolute;left:200px;top:0;width:200px;height:150px;background:rgb(40,200,20)'></div><div style='position:absolute;left:0;top:150px;width:200px;height:150px;background:rgb(20,40,200)'></div><div style='position:absolute;left:200px;top:150px;width:200px;height:150px;background:rgb(7,8,9)'></div></body>";

#[test]
fn pixels_are_exact_in_every_encoding() {
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/", QUADRANTS)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    for zrle in [false, true] {
        let mut viewer = Viewer::connect(&tab, zrle);
        viewer.expect(399, 299, [7, 8, 9]);
        assert_eq!(viewer.encoding, if zrle { 16 } else { 0 });
        assert_eq!(viewer.at(0, 0), [200, 20, 40]);
        assert_eq!(viewer.at(399, 0), [40, 200, 20]);
        assert_eq!(viewer.at(0, 299), [20, 40, 200]);
        // Corners of the quadrants, so nothing is scaled or blurred.
        assert_eq!(viewer.at(199, 149), [200, 20, 40]);
        assert_eq!(viewer.at(200, 150), [7, 8, 9]);
    }
}

/// A small change costs a small update.
#[test]
fn an_update_carries_only_what_changed() {
    let page = "<!doctype html><body style='margin:0;background:rgb(10,10,10)'><div id=d style='position:absolute;left:100px;top:100px;width:20px;height:20px;background:rgb(200,0,0)'></div></body>";
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab_sized("display", &url, 1280, 720);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let mut viewer = Viewer::connect(&tab, false);
    viewer.expect(110, 110, [200, 0, 0]);
    cdp.eval("d.style.background = 'rgb(0,200,0)'");
    let deadline = Instant::now() + PATIENCE;
    while viewer.at(110, 110) != [0, 200, 0] {
        assert!(Instant::now() < deadline, "the change never came");
        viewer.request(true);
        viewer.update();
    }
    let area: usize = viewer.rects.iter().map(|(_, _, w, h)| w * h).sum();
    assert!(
        area <= 64 * 64 * 4,
        "changing 20x20 updated {area} pixels: {:?}",
        viewer.rects
    );
}

/// A `<select>`'s popup is a separate widget, and part of the picture.
#[test]
fn a_select_popup_is_in_the_picture() {
    let page = "<!doctype html><body style='margin:0;background:rgb(10,10,10)'><select id=s style='position:absolute;left:10px;top:10px;width:150px'><option style='background:rgb(1,200,3)'>one</option><option style='background:rgb(1,200,3)'>two</option><option style='background:rgb(1,200,3)'>three</option></select></body>";
    let mut c = Client::start(&[DEVTOOLS, IME, FENCE]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.viewer.expect(300, 200, [10, 10, 10]);
    c.fence(t.tab).unwrap();
    t.viewer.click(50, 18);
    let deadline = Instant::now() + PATIENCE;
    loop {
        t.viewer.request(false);
        t.viewer.update();
        let shown = (30..120).any(|y| t.viewer.at(50, y) == [1, 200, 3]);
        if shown {
            break;
        }
        assert!(Instant::now() < deadline, "no popup in the picture");
        std::thread::sleep(Duration::from_millis(30));
    }
    // Choosing from it, with the pointer on the popup.
    let y = (30..120)
        .rev()
        .find(|y| t.viewer.at(50, *y) == [1, 200, 3])
        .unwrap() as u16;
    t.viewer.click(50, y - 4);
    t.page.until("s.selectedIndex", json!(2));
}

/// One gesture chooses too: press on the `<select>`, drag onto an option in its popup, release.
#[test]
fn a_select_popup_takes_the_pointer_while_a_button_is_held() {
    let page = "<!doctype html><body style='margin:0;background:rgb(10,10,10)'><select id=s style='position:absolute;left:11px;top:13px;width:150px'><option style='background:rgb(1,200,3)'>one</option><option style='background:rgb(1,200,3)'>two</option><option style='background:rgb(1,200,3)'>three</option></select></body>";
    let mut c = Client::start(&[DEVTOOLS, IME, FENCE]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    let mut t = c.controlled(&tab);
    t.viewer.expect(300, 200, [10, 10, 10]);
    c.fence(t.tab).unwrap();
    t.viewer.pointer(0, 50, 20);
    t.viewer.pointer(1, 50, 20);
    let deadline = Instant::now() + PATIENCE;
    let y = loop {
        t.viewer.request(false);
        t.viewer.update();
        if let Some(y) = (30..140).rev().find(|y| t.viewer.at(50, *y) == [1, 200, 3]) {
            break y as u16;
        }
        assert!(Instant::now() < deadline, "no popup in the picture");
        std::thread::sleep(Duration::from_millis(30));
    };
    t.viewer.pointer(1, 50, y - 4);
    t.viewer.pointer(0, 50, y - 4);
    t.page.until("s.selectedIndex", json!(2));
}

#[test]
fn a_viewer_follows_a_resize() {
    let page = "<!doctype html><body style='margin:0;background:rgb(40,200,20)'></body>";
    let mut c = Client::start(&[]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    let mut viewer = Viewer::connect(&tab, false);
    viewer.expect(399, 299, [40, 200, 20]);
    c.ok(request::Command::TabOperation(TabOperation {
        tab: tab.tab,
        action: tab_operation::Action::Resize as i32,
        width: 640,
        height: 200,
        ..Default::default()
    }));
    let deadline = Instant::now() + PATIENCE;
    while (viewer.width, viewer.height) != (640, 200) || viewer.at(639, 199) != [40, 200, 20] {
        assert!(
            Instant::now() < deadline,
            "at {}x{}",
            viewer.width,
            viewer.height
        );
        viewer.request(false);
        viewer.update();
    }
}

/// After a fence, the frame it names shows what the page had when it was asked.
#[test]
fn a_fence_means_the_picture_is_current() {
    let page = "<!doctype html><body style='margin:0;background:rgb(0,0,0)'></body>";
    let mut c = Client::start(&[DEVTOOLS, FENCE]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let mut viewer = Viewer::connect(&tab, false);
    viewer.expect(10, 10, [0, 0, 0]);
    for shade in [50u8, 100, 150, 200, 250] {
        cdp.eval(&format!(
            "document.body.style.background = 'rgb({shade},0,0)'"
        ));
        let sequence = c.fence(tab.tab).unwrap();
        loop {
            viewer.request(false);
            viewer.update();
            if viewer.sequence >= sequence {
                break;
            }
        }
        assert_eq!(
            viewer.at(10, 10),
            [shade, 0, 0],
            "frame {}",
            viewer.sequence
        );
    }
}

/// A fence on a page a dialog has blocked says what a person sees, rather than hanging.
#[test]
fn a_fence_survives_a_dialog() {
    let page = "<!doctype html><body style='margin:0;background:rgb(0,90,0)'></body>";
    let mut c = Client::start(&[DEVTOOLS, FENCE]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    let mut viewer = Viewer::connect(&tab, false);
    viewer.expect(10, 10, [0, 90, 0]);
    let cdp = c.devtools(tab.tab);
    cdp.send_without_answer("Runtime.evaluate", json!({"expression": "alert('hello')"}));
    c.event(|e| matches!(e, event::Event::Dialog(_)));
    c.fence(tab.tab).unwrap();
}

/// A viewer that stops reading holds up nobody else.
#[test]
fn a_slow_viewer_holds_up_nobody() {
    let page = "<!doctype html><body style='margin:0;background:rgb(0,0,0)'></body>";
    let mut c = Client::start(&[DEVTOOLS]);
    let pages = Pages::serve(&[("/", page)]);
    let url = pages.url("/");
    let tab = c.tab_sized("display", &url, 1280, 720);
    c.loaded(tab.tab, &url);
    let mut cdp = c.devtools(tab.tab);
    let mut stuck = Viewer::connect(&tab, false);
    for _ in 0..50 {
        stuck.request(false);
    }
    let mut viewer = Viewer::connect(&tab, false);
    for shade in [50u8, 150, 250] {
        cdp.eval(&format!(
            "document.body.style.background = 'rgb({shade},0,0)'"
        ));
        viewer.expect(10, 10, [shade, 0, 0]);
    }
}

#[test]
fn loading_state_follows_a_load() {
    let mut c = Client::start(&[LOADING]);
    let pages = Pages::serve(&[("/", QUADRANTS), ("/next", QUADRANTS)]);
    let url = pages.url("/");
    let tab = c.tab("display", &url);
    c.loaded(tab.tab, &url);
    c.event(|e| matches!(e, event::Event::LoadingState(s) if s.tab == tab.tab && !s.is_loading));
    c.navigate(tab.tab, &pages.url("/next"));
    c.event(|e| matches!(e, event::Event::LoadingState(s) if s.tab == tab.tab && !s.is_loading && s.can_go_back));
}
