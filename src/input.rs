//! RFB keysyms and pointer state.
use std::{collections::HashMap, time::Duration, time::Instant};

/// RFB carries no click count, so presses of one button close in time and place count up here,
/// as a platform's input layer does for a local browser.
const CLICK_INTERVAL: Duration = Duration::from_millis(500);
const CLICK_DISTANCE: i32 = 4;

#[derive(Default)]
pub struct Clicks {
    last: Option<(u8, (i32, i32), Instant)>,
    pub count: i32,
}

impl Clicks {
    pub fn press(&mut self, button: u8, at: (i32, i32), now: Instant) {
        let again = self.last.is_some_and(|(b, (x, y), t)| {
            b == button
                && (x - at.0).abs() <= CLICK_DISTANCE
                && (y - at.1).abs() <= CLICK_DISTANCE
                && now.duration_since(t) <= CLICK_INTERVAL
        });
        // A fourth press starts over.
        self.count = if again && self.count < 3 {
            self.count + 1
        } else {
            1
        };
        self.last = Some((button, at, now));
    }
}

/// Right modifier keys share their left key's code, but are pressed and released on their own.
const RIGHT_KEY: i32 = 1 << 16;

/// The input state of one tab.
#[derive(Default)]
pub struct Input {
    /// Pressed keys by slot, with their virtual key.
    pub keys: HashMap<i32, i32>,
    pub buttons: u8,
    pub pointer: (i32, i32),
    pub clicks: Clicks,
    pub owner: u64,
}

impl Input {
    pub fn slot(key: u64, code: i32) -> i32 {
        if matches!(key, 0xffe2 | 0xffe4 | 0xffea | 0xffec) {
            code | RIGHT_KEY
        } else {
            code
        }
    }

    fn held(&self, code: i32) -> bool {
        self.keys.contains_key(&code) || self.keys.contains_key(&(code | RIGHT_KEY))
    }

    /// Ctrl or Command with V pastes the engine clipboard, which no viewer owns; it never comes
    /// from a key. (Shift+Insert cannot: Insert is not mapped.)
    pub fn is_paste(&self, key: u64, down: bool) -> bool {
        down && matches!(key, 0x56 | 0x76) && (self.held(17) || self.held(91))
    }

    pub fn modifiers(&self) -> u32 {
        let mut flags = 0;
        for (key, flag) in [(16, 2), (17, 4), (18, 8), (91, 128)] {
            if self.held(key) {
                flags |= flag;
            }
        }
        for (button, flag) in [(1, 16), (2, 32), (4, 64)] {
            if self.buttons & button != 0 {
                flags |= flag;
            }
        }
        flags
    }
}

pub fn character(key: u64) -> Option<char> {
    match key {
        0xff08 => Some('\u{8}'),
        0xff09 | 0xff89 | 0xfe20 => Some('\t'),
        0xff0d | 0xff8d => Some('\r'),
        0xffaa..=0xffaf | 0xffb0..=0xffb9 | 0xffbd => char::from_u32((key - 0xff80) as u32),
        32..=126 | 160..=255 => char::from_u32(key as u32),
        0x01000000..=0x0110ffff => char::from_u32((key - 0x01000000) as u32),
        _ => None,
    }
}

pub fn virtual_key(key: u64) -> i32 {
    match key {
        0xff08 => 8,
        // Tab, keypad Tab and the Shift+Tab keysym X11 clients send.
        0xff09 | 0xff89 | 0xfe20 => 9,
        0xff0d | 0xff8d => 13,
        0xffaa..=0xffaf => 106 + (key - 0xffaa) as i32,
        0xffb0..=0xffb9 => 96 + (key - 0xffb0) as i32,
        0xffbd => 187,
        0xff95 => 36,
        0xff96 => 37,
        0xff97 => 38,
        0xff98 => 39,
        0xff99 => 40,
        0xff9a => 33,
        0xff9b => 34,
        0xff9c => 35,
        0xff9f => 46,
        0xff1b => 27,
        0xff50 => 36,
        0xff51 => 37,
        0xff52 => 38,
        0xff53 => 39,
        0xff54 => 40,
        0xff55 => 33,
        0xff56 => 34,
        0xff57 => 35,
        0xffff => 46,
        0xffe1 | 0xffe2 => 16,
        0xffe3 | 0xffe4 => 17,
        0xffe9 | 0xffea => 18,
        0xffeb | 0xffec => 91,
        0xffbe..=0xffd5 => 112 + (key - 0xffbe) as i32,
        97..=122 => key as i32 - 32,
        59 | 58 => 186,
        61 | 43 => 187,
        44 | 60 => 188,
        45 | 95 => 189,
        46 | 62 => 190,
        47 | 63 => 191,
        96 | 126 => 192,
        91 | 123 => 219,
        92 | 124 => 220,
        93 | 125 => 221,
        39 | 34 => 222,
        // Shifted digits are their digit's key. As key codes they would be Home, the arrows and
        // others, which edit the field before the character lands.
        41 => 48,
        33 => 49,
        64 => 50,
        35 => 51,
        36 => 52,
        37 => 53,
        94 => 54,
        38 => 55,
        42 => 56,
        40 => 57,
        32 | 48..=57 | 65..=90 => key as i32,
        _ => 0,
    }
}
