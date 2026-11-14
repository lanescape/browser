//! Tab-session CDP policy: page parity, never browser authority.
use serde_json::{Map, Value, json};
use std::{
    collections::HashMap,
    io::{self, BufRead},
};

pub const MESSAGE: usize = 64 << 20;
/// Client ids stay below the engine's reserved range, ENGINE_IDS..=i32::MAX.
pub const CLIENT_IDS: u64 = 0x7FFE_FFFF;
pub const ENGINE_IDS: u64 = 0x7FFF_0000;

pub struct Command {
    pub id: u64,
    pub method: String,
    pub session: Option<String>,
    pub params: Map<String, Value>,
}

/// Reads one NUL-terminated frame. Ok(false) is a clean end of stream.
pub fn read_frame(r: &mut impl BufRead, out: &mut Vec<u8>) -> io::Result<bool> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid CDP frame");
    loop {
        let buf = r.fill_buf()?;
        if buf.is_empty() {
            return if out.is_empty() {
                Ok(false)
            } else {
                Err(invalid())
            };
        }
        if let Some(i) = buf.iter().position(|b| *b == 0) {
            out.extend_from_slice(&buf[..i]);
            r.consume(i + 1);
            return if out.len() > MESSAGE {
                Err(invalid())
            } else {
                Ok(true)
            };
        }
        let n = buf.len();
        out.extend_from_slice(buf);
        r.consume(n);
        if out.len() > MESSAGE {
            return Err(invalid());
        }
    }
}

/// A malformed message may be dropped without an answer, so anything that could be is a protocol violation.
pub fn parse(frame: &[u8]) -> Option<Command> {
    let Value::Object(mut map) = serde_json::from_slice::<Value>(frame).ok()? else {
        return None;
    };
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "id" | "method" | "params" | "sessionId"))
    {
        return None;
    }
    let id = map
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id <= CLIENT_IDS)?;
    let method = match map.remove("method") {
        Some(Value::String(m)) if !m.is_empty() => m,
        _ => return None,
    };
    let session = match map.remove("sessionId") {
        None => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return None,
    };
    let params = match map.remove("params") {
        None => Map::new(),
        Some(Value::Object(p)) => p,
        Some(_) => return None,
    };
    Some(Command {
        id,
        method,
        session,
        params,
    })
}

/// Whether a tab may load this URL: web pages, not the host's files or the browser's own pages,
/// whoever asks. Every `about:` URL is an empty document here. Checked on the URL as written:
/// Chromium trims and strips what it parses, and acts on debug URLs such as `chrome://kill`, and on
/// `javascript:`, before any navigation starts, so a browser handler never sees those.
pub fn navigable(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        ["http", "https", "data", "blob", "about"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
    })
}

/// Why a tab session may not send this command, or None when it may.
/// `children` maps attached child session ids to their target ids.
pub fn refusal(
    c: &Command,
    children: &HashMap<String, String>,
    controlled: bool,
) -> Option<&'static str> {
    let Some((domain, name)) = c.method.split_once('.') else {
        return Some("unknown method");
    };
    let child = |key: &str, values: bool| {
        c.params.get(key).and_then(Value::as_str).is_some_and(|v| {
            if values {
                children.values().any(|t| t == v)
            } else {
                children.contains_key(v)
            }
        })
    };
    const NO_CLEANUP: &str = "the engine cannot undo it when the session ends";
    // A path here would read any file the engine can, with nobody having authorized it.
    const SERVER_FILES: &str = "the supervisor authorizes server files through FileDialog";
    match domain {
        "Accessibility" | "Animation" | "Audits" | "CSS" | "DOMSnapshot" | "Debugger"
        | "Inspector" | "Log" | "Overlay" | "Performance" | "Runtime" | "Fetch" => None,
        "DOM" => match name {
            "setFileInputFiles" => Some(SERVER_FILES),
            "getFileInfo" => Some("it reveals server paths hidden from the page"),
            _ => None,
        },
        "Page" => match name {
            "startScreencast"
            | "stopScreencast"
            | "screencastFrameAck"
            | "startScreenRecording"
            | "stopScreenRecording" => Some("RFB is the display"),
            "setInterceptFileChooserDialog" => {
                Some("file pickers are answered with FileDialogReply")
            }
            "handleJavaScriptDialog" => Some("dialogs are answered with DialogReply"),
            "setDownloadBehavior" => Some("downloads are approved with DownloadReply"),
            "close" | "crash" => Some("tab lifetime belongs to the control channel"),
            "printToPDF" => Some("not supported"),
            "navigate"
                if !c
                    .params
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(navigable) =>
            {
                Some("tabs load only web pages")
            }
            "setDeviceMetricsOverride"
            | "clearDeviceMetricsOverride"
            | "setTouchEmulationEnabled"
            | "setGeolocationOverride"
            | "clearGeolocationOverride"
            | "setDeviceOrientationOverride"
            | "clearDeviceOrientationOverride" => Some("use the Emulation domain"),
            "addScriptToEvaluateOnNewDocument"
                if c.params
                    .get("worldName")
                    .is_some_and(|w| w.as_str() != Some("")) =>
            {
                Some("named worlds reach beyond the page's origin")
            }
            "addScriptToEvaluateOnLoad" | "removeScriptToEvaluateOnLoad" => {
                Some("use addScriptToEvaluateOnNewDocument")
            }
            "setFontFamilies"
            | "setFontSizes"
            | "setPrerenderingAllowed"
            | "setSPCTransactionMode"
            | "setRPHRegistrationMode" => Some(NO_CLEANUP),
            // Reads, navigation and page-local state the engine resets; nothing else.
            "enable"
            | "disable"
            | "getFrameTree"
            | "getResourceTree"
            | "getResourceContent"
            | "searchInResource"
            | "setLifecycleEventsEnabled"
            | "addScriptToEvaluateOnNewDocument"
            | "removeScriptToEvaluateOnNewDocument"
            | "createIsolatedWorld"
            | "navigate"
            | "reload"
            | "stopLoading"
            | "navigateToHistoryEntry"
            | "getNavigationHistory"
            | "captureScreenshot"
            | "captureSnapshot"
            | "getLayoutMetrics"
            | "bringToFront"
            | "setBypassCSP"
            | "setAdBlockingEnabled"
            | "setDocumentContent"
            | "getAppManifest"
            | "getInstallabilityErrors"
            | "getOriginTrials"
            | "getPermissionsPolicyState" => None,
            _ => Some(NO_CLEANUP),
        },
        "Input" if name == "setIgnoreInputEvents" => Some("viewer input cannot be blocked"),
        "Input"
            if name == "dispatchKeyEvent"
                && c.params
                    .get("commands")
                    .and_then(Value::as_array)
                    .is_some_and(|commands| {
                        commands.iter().any(|command| {
                            command.as_str().is_none_or(|command| {
                                let command = command.to_ascii_lowercase();
                                ["copy", "cut", "paste", "yank"]
                                    .iter()
                                    .any(|c| command.contains(c))
                            })
                        })
                    }) =>
        {
            Some("the browser clipboard is shared beyond this tab")
        }
        "Input" if name == "dispatchKeyEvent" && !valid_key(&c.params) => {
            Some("modifiers must be 0-15 and key codes 0-65535")
        }
        "Input" if name == "dispatchKeyEvent" && clipboard_key(&c.params) => {
            Some("the browser clipboard is shared beyond this tab")
        }
        "Input" if controlled => Some("a viewer controls input"),
        "Input"
            if name == "dispatchDragEvent"
                && c.params
                    .get("data")
                    .and_then(|d| d.get("files"))
                    .is_some_and(|f| f.as_array().is_none_or(|f| !f.is_empty())) =>
        {
            Some(SERVER_FILES)
        }
        "Input" => match name {
            "dispatchKeyEvent"
            | "dispatchMouseEvent"
            | "dispatchTouchEvent"
            | "dispatchDragEvent"
            | "insertText"
            | "imeSetComposition"
            | "emulateTouchFromMouseEvent"
            | "setInterceptDrags"
            | "cancelDragging" => None,
            _ => Some(NO_CLEANUP),
        },
        "Emulation" => match name {
            "setDeviceMetricsOverride" | "clearDeviceMetricsOverride" | "setVisibleSize" => {
                Some("viewport size is set with TabOperation RESIZE")
            }
            // Each applies to the whole renderer process, which other tabs may share.
            "setLocaleOverride" | "setTimezoneOverride" => {
                Some("timezone and locale are host-wide user preferences")
            }
            // Every allowed override has a reset in RESET.
            "canEmulate"
            | "setFocusEmulationEnabled"
            | "setEmulatedMedia"
            | "setTouchEmulationEnabled"
            | "setEmitTouchEventsForMouse"
            | "setUserAgentOverride"
            | "setGeolocationOverride"
            | "clearGeolocationOverride"
            | "setScriptExecutionDisabled"
            | "setCPUThrottlingRate"
            | "setDefaultBackgroundColorOverride"
            | "setAutoDarkModeOverride"
            | "setEmulatedVisionDeficiency"
            | "setScrollbarsHidden"
            | "setIdleOverride"
            | "clearIdleOverride"
            | "setDocumentCookieDisabled" => None,
            _ => Some(NO_CLEANUP),
        },
        "Network" => match name {
            "getCookies"
            | "getAllCookies"
            | "setCookie"
            | "setCookies"
            | "deleteCookies"
            | "clearBrowserCookies"
            | "setCookieControls" => Some("the cookie jar is not visible to the page"),
            "clearBrowserCache" => Some("the cache belongs to the profile"),
            "loadNetworkResource" => Some("it reads beyond the page's own origin"),
            "setRequestInterception"
            | "continueInterceptedRequest"
            | "getResponseBodyForInterception"
            | "takeResponseBodyForInterceptionAsStream" => Some("use the Fetch domain"),
            // Page traffic and the overrides RESET undoes; nothing that reaches profile state.
            "enable"
            | "disable"
            | "getResponseBody"
            | "getRequestPostData"
            | "getCertificate"
            | "getSecurityIsolationStatus"
            | "searchInResponseBody"
            | "streamResourceContent"
            | "replayXHR"
            | "setAttachDebugStack"
            | "setCacheDisabled"
            | "setExtraHTTPHeaders"
            | "setUserAgentOverride"
            | "setBlockedURLs"
            | "setBypassServiceWorker"
            | "emulateNetworkConditions" => None,
            _ => Some(NO_CLEANUP),
        },
        "Target" => match name {
            "setAutoAttach" => None,
            "detachFromTarget" if child("sessionId", false) || child("targetId", true) => None,
            "getTargetInfo" if !c.params.contains_key("targetId") || child("targetId", true) => {
                None
            }
            _ => Some("it reaches outside this tab"),
        },
        "Browser" if name == "getVersion" => None,
        "Browser" => Some("it controls the whole browser"),
        _ => Some("the domain is not available"),
    }
}

/// Chromium truncates these to C integers, so out-of-range values could alias a shortcut.
fn valid_key(params: &Map<String, Value>) -> bool {
    let within = |key: &str, max: u64| {
        params
            .get(key)
            .is_none_or(|v| v.as_u64().is_some_and(|n| n <= max))
    };
    within("modifiers", 15)
        && within("windowsVirtualKeyCode", 0xFFFF)
        && within("nativeVirtualKeyCode", 0xFFFF)
}

/// Whether a synthesized key is a clipboard shortcut, which the editor would run against the
/// browser-wide clipboard.
fn clipboard_key(params: &Map<String, Value>) -> bool {
    let modifiers = params.get("modifiers").and_then(Value::as_u64).unwrap_or(0);
    let (shift, command) = (modifiers & 8 != 0, modifiers & (2 | 4) != 0);
    let text = |key: &str| params.get(key).and_then(Value::as_str).unwrap_or("");
    let code = ["windowsVirtualKeyCode", "nativeVirtualKeyCode"]
        .iter()
        .find_map(|k| params.get(*k).and_then(Value::as_u64));
    let key = text("key").to_ascii_lowercase();
    let is = |letter: char, virtual_key: u64| {
        code == Some(virtual_key)
            || key == letter.to_string()
            || text("code") == format!("Key{}", letter.to_ascii_uppercase())
    };
    let insert = code == Some(45) || key == "insert" || text("code") == "Insert";
    let delete = code == Some(46) || key == "delete" || text("code") == "Delete";
    matches!(key.as_str(), "copy" | "cut" | "paste")
        || (command && (is('c', 67) || is('v', 86) || is('x', 88) || insert))
        || (shift && (insert || delete))
}

/// Allocates engine command ids 1..=CLIENT_IDS once each; None when they are exhausted.
pub fn allocate(next: &std::sync::atomic::AtomicU64) -> Option<u64> {
    use std::sync::atomic::Ordering::Relaxed;
    next.try_update(Relaxed, Relaxed, |n| (n < CLIENT_IDS).then_some(n + 1))
        .ok()
        .map(|n| n + 1)
}

/// Encodes an allowed command for Chromium under the engine's id, keeping page parity:
/// an isolated world never gets universal access beyond the page's origin.
pub fn encode(c: &Command, id: u64) -> Vec<u8> {
    let mut params = c.params.clone();
    if c.method == "Page.createIsolatedWorld" && params.contains_key("grantUniveralAccess") {
        params.insert("grantUniveralAccess".into(), false.into());
    }
    let mut v = json!({"id": id, "method": c.method, "params": params});
    if let Some(s) = &c.session {
        v["sessionId"] = s.as_str().into();
    }
    serde_json::to_vec(&v).unwrap()
}

/// The id of a DevTools response, which Chromium always serializes first.
pub fn leading_id(message: &[u8]) -> Option<u64> {
    let rest = message.strip_prefix(br#"{"id":"#)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()
}

/// Whether a DevTools response is an error; Chromium serializes `error` right after the id.
pub fn is_error(message: &[u8]) -> bool {
    message
        .strip_prefix(br#"{"id":"#)
        .map(|rest| &rest[rest.iter().take_while(|b| b.is_ascii_digit()).count()..])
        .is_some_and(|rest| rest.starts_with(br#","error""#))
}
/// The method of a DevTools event, which Chromium always serializes first.
pub fn leading_method(message: &[u8]) -> Option<&str> {
    let rest = message.strip_prefix(br#"{"method":""#)?;
    let end = rest.iter().position(|b| *b == b'"')?;
    std::str::from_utf8(&rest[..end]).ok()
}

/// Replaces a response's leading id without decoding it: responses may nest deeper than a
/// JSON decoder accepts.
pub fn with_id(message: &[u8], id: u64) -> Option<Vec<u8>> {
    let rest = message.strip_prefix(br#"{"id":"#)?;
    let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let mut out = format!(r#"{{"id":{id}"#).into_bytes();
    out.extend_from_slice(&rest[digits..]);
    Some(out)
}

/// An engine-generated CDP error response.
pub fn error(id: u64, session: Option<&str>, method: &str, reason: &str) -> Vec<u8> {
    let mut v = json!({"id": id, "error": {"code": -32000, "message":
        format!("Lanescape browser: {method} is not available in a tab session: {reason}")}});
    if let Some(s) = session {
        v["sessionId"] = s.into();
    }
    serde_json::to_vec(&v).unwrap()
}

/// Messages of these domains can carry cookie values.
pub fn needs_scrub(method: &str) -> bool {
    ["Network.", "Fetch.", "Audits."]
        .iter()
        .any(|d| method.starts_with(d))
}

/// Headers the browser fills in that page script can never read.
fn is_secret(name: &str) -> bool {
    [
        "cookie",
        "set-cookie",
        "authorization",
        "proxy-authorization",
    ]
    .iter()
    .any(|h| name.eq_ignore_ascii_case(h))
}

/// Removes cookie and authorization headers, raw cookie lines and cookie lists.
/// Cookies that aren't HttpOnly stay readable through document.cookie.
pub fn scrub(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.retain(|k, _| {
                !matches!(
                    k.as_str(),
                    "headersText"
                        | "requestHeadersText"
                        | "associatedCookies"
                        | "blockedCookies"
                        | "exemptedCookies"
                        | "cookies"
                        | "rawCookieLine"
                )
            });
            for (k, child) in map.iter_mut() {
                if matches!(k.as_str(), "headers" | "requestHeaders")
                    && let Value::Object(headers) = child
                {
                    headers.retain(|name, _| !is_secret(name));
                }
                scrub(child);
            }
        }
        Value::Array(items) => {
            items.retain(|item| {
                !(item.get("value").is_some()
                    && item
                        .get("name")
                        .and_then(Value::as_str)
                        .is_some_and(is_secret))
            });
            items.iter_mut().for_each(scrub);
        }
        _ => {}
    }
}

/// Session state that can outlast a client, and how to undo it: every override the policy allows.
/// Child sessions are detached separately, which discards their state. A session's end doesn't send
/// these yet, and relies on Chromium's own teardown (specs/0002-control.md, Not implemented yet).
pub const RESET: &[(&str, &str)] = &[
    (
        "Target.setAutoAttach",
        r#"{"autoAttach":false,"waitForDebuggerOnStart":false,"flatten":true}"#,
    ),
    ("Fetch.disable", "{}"),
    ("Input.setInterceptDrags", r#"{"enabled":false}"#),
    ("Debugger.disable", "{}"),
    ("Emulation.setScriptExecutionDisabled", r#"{"value":false}"#),
    ("Emulation.setCPUThrottlingRate", r#"{"rate":1}"#),
    ("Emulation.setTouchEmulationEnabled", r#"{"enabled":false}"#),
    (
        "Emulation.setEmitTouchEventsForMouse",
        r#"{"enabled":false}"#,
    ),
    ("Emulation.setFocusEmulationEnabled", r#"{"enabled":false}"#),
    (
        "Emulation.setEmulatedMedia",
        r#"{"media":"","features":[]}"#,
    ),
    ("Emulation.clearGeolocationOverride", "{}"),
    ("Emulation.setUserAgentOverride", r#"{"userAgent":""}"#),
    ("Emulation.setDefaultBackgroundColorOverride", "{}"),
    ("Emulation.setAutoDarkModeOverride", "{}"),
    (
        "Emulation.setEmulatedVisionDeficiency",
        r#"{"type":"none"}"#,
    ),
    ("Emulation.setScrollbarsHidden", r#"{"hidden":false}"#),
    ("Emulation.clearIdleOverride", "{}"),
    (
        "Emulation.setDocumentCookieDisabled",
        r#"{"disabled":false}"#,
    ),
    (
        "Network.emulateNetworkConditions",
        r#"{"offline":false,"latency":0,"downloadThroughput":-1,"uploadThroughput":-1}"#,
    ),
    ("Network.setBlockedURLs", r#"{"urls":[]}"#),
    ("Network.setCacheDisabled", r#"{"cacheDisabled":false}"#),
    ("Network.setExtraHTTPHeaders", r#"{"headers":{}}"#),
    ("Network.setBypassServiceWorker", r#"{"bypass":false}"#),
    ("Page.setBypassCSP", r#"{"enabled":false}"#),
    ("Page.setAdBlockingEnabled", r#"{"enabled":false}"#),
    (
        "Overlay.setInspectMode",
        r#"{"mode":"none","highlightConfig":{}}"#,
    ),
    ("Overlay.hideHighlight", "{}"),
    ("Network.disable", "{}"),
    ("Page.disable", "{}"),
    ("Runtime.disable", "{}"),
    ("DOM.disable", "{}"),
    ("CSS.disable", "{}"),
    ("Log.disable", "{}"),
    ("Overlay.disable", "{}"),
    ("Accessibility.disable", "{}"),
    ("Performance.disable", "{}"),
    ("Animation.disable", "{}"),
    ("Audits.disable", "{}"),
];

#[cfg(test)]
mod tests {
    use super::*;
    fn command(method: &str, params: Value) -> Command {
        Command {
            id: 1,
            method: method.into(),
            session: None,
            params: params.as_object().unwrap().clone(),
        }
    }
    #[test]
    fn frames_are_nul_terminated_and_bounded() {
        let mut r = io::BufReader::new(&b"{\"a\":1}\0{}\0"[..]);
        let mut out = Vec::new();
        assert!(read_frame(&mut r, &mut out).unwrap());
        assert_eq!(out, b"{\"a\":1}");
        out.clear();
        assert!(read_frame(&mut r, &mut out).unwrap());
        out.clear();
        assert!(!read_frame(&mut r, &mut out).unwrap());
        let mut truncated = io::BufReader::new(&b"{}"[..]);
        assert!(read_frame(&mut truncated, &mut Vec::new()).is_err());
        let big = vec![b' '; MESSAGE + 1];
        assert!(read_frame(&mut io::BufReader::new(&big[..]), &mut Vec::new()).is_err());
    }
    #[test]
    fn commands_must_be_exactly_what_chromium_accepts() {
        let ok =
            parse(br#"{"id":7,"method":"Runtime.evaluate","params":{},"sessionId":"s"}"#).unwrap();
        assert_eq!((ok.id, ok.session.as_deref()), (7, Some("s")));
        for bad in [
            &br#"[]"#[..],
            br#"{"method":"Page.enable"}"#,
            br#"{"id":-1,"method":"Page.enable"}"#,
            br#"{"id":2147418112,"method":"Page.enable"}"#,
            br#"{"id":1,"method":""}"#,
            br#"{"id":1,"method":"Page.enable","params":[]}"#,
            br#"{"id":1,"method":"Page.enable","sessionId":1}"#,
            br#"{"id":1,"method":"Page.enable","extra":true}"#,
        ] {
            assert!(parse(bad).is_none(), "{}", String::from_utf8_lossy(bad));
        }
    }
    #[test]
    fn policy_keeps_sessions_inside_the_tab_and_page() {
        let none = HashMap::new();
        let allow = |m: &str, p: Value| refusal(&command(m, p), &none, false);
        for method in [
            "Runtime.evaluate",
            "DOM.getDocument",
            "Page.captureScreenshot",
            "Network.enable",
            "Fetch.enable",
            "Emulation.setEmulatedMedia",
            "Input.dispatchMouseEvent",
            "Target.setAutoAttach",
            "Target.getTargetInfo",
            "Browser.getVersion",
        ] {
            assert_eq!(allow(method, json!({})), None, "{method}");
        }
        for method in [
            "Network.getCookies",
            "Network.getAllCookies",
            "Network.setCookie",
            "Network.loadNetworkResource",
            "Page.startScreencast",
            "Page.handleJavaScriptDialog",
            "Page.setInterceptFileChooserDialog",
            "Page.setDownloadBehavior",
            "Page.close",
            "Emulation.setDeviceMetricsOverride",
            "Emulation.setTimezoneOverride",
            "Emulation.setLocaleOverride",
            "Input.setIgnoreInputEvents",
            "Target.createTarget",
            "Target.attachToTarget",
            "Target.getTargets",
            "Target.createBrowserContext",
            "Browser.close",
            "Storage.getCookies",
            "SystemInfo.getInfo",
            "Unlisted.method",
            "nodomain",
        ] {
            assert!(allow(method, json!({})).is_some(), "{method}");
        }
        assert!(allow("Target.getTargetInfo", json!({"targetId":"other"})).is_some());
        assert!(allow("Target.detachFromTarget", json!({"sessionId":"other"})).is_some());
        let children = HashMap::from([("child".to_string(), "frame".to_string())]);
        let child = |m: &str, p: Value| refusal(&command(m, p), &children, false);
        assert_eq!(
            child("Target.detachFromTarget", json!({"sessionId":"child"})),
            None
        );
        assert_eq!(
            child("Target.getTargetInfo", json!({"targetId":"frame"})),
            None
        );
        let controlled = refusal(&command("Input.insertText", json!({})), &none, true);
        assert!(controlled.is_some());
    }
    #[test]
    fn policy_closes_paths_around_engine_flows() {
        let none = HashMap::new();
        let check = |m: &str, p: Value| refusal(&command(m, p), &none, false);
        for (method, params) in [
            ("DOM.getFileInfo", json!({})),
            ("Page.setDeviceMetricsOverride", json!({})),
            ("Page.startScreenRecording", json!({})),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","commands":["Paste"]}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","commands":["selectAll","pasteAndMatchStyle"]}),
            ),
            ("Page.addScriptToEvaluateOnLoad", json!({})),
            ("Page.setFontFamilies", json!({})),
            ("Emulation.setSensorOverrideEnabled", json!({})),
            ("Network.overrideNetworkState", json!({})),
            ("Network.enableDeviceBoundSessions", json!({})),
            ("Network.deleteDeviceBoundSession", json!({})),
            ("DOMDebugger.setXHRBreakpoint", json!({"url":""})),
            ("Page.resetNavigationHistory", json!({})),
            ("Input.unlistedFutureMethod", json!({})),
            ("Input.synthesizeTapGesture", json!({})),
            ("Page.setWebLifecycleState", json!({"state":"frozen"})),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":2,"windowsVirtualKeyCode":86}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","modifiers":4,"key":"c"}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","modifiers":10,"code":"KeyX"}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":8,"windowsVirtualKeyCode":45}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","key":"Paste"}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":-2147483646i64,"windowsVirtualKeyCode":86}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":2,"windowsVirtualKeyCode":4294967382u64}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":2.5,"windowsVirtualKeyCode":86}),
            ),
            ("DOM.setFileInputFiles", json!({"files":["/srv/a"]})),
            ("DOM.setFileInputFiles", json!({"files":[]})),
            (
                "Input.dispatchDragEvent",
                json!({"data":{"files":["/srv/a"]}}),
            ),
            (
                "Input.dispatchDragEvent",
                json!({"data":{"files":"/srv/a"}}),
            ),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source":"1","worldName":"w"}),
            ),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source":"1","worldName":1}),
            ),
        ] {
            assert!(check(method, params.clone()).is_some(), "{method} {params}");
        }
        for (method, params) in [
            ("Emulation.setScrollbarsHidden", json!({})),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","commands":["selectAll"]}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"keyDown","key":"v","text":"v","windowsVirtualKeyCode":86}),
            ),
            (
                "Input.dispatchKeyEvent",
                json!({"type":"rawKeyDown","modifiers":2,"windowsVirtualKeyCode":65}),
            ),
            ("Input.dispatchDragEvent", json!({"data":{"files":[]}})),
            ("Input.dispatchDragEvent", json!({"data":{"items":[]}})),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source":"1"}),
            ),
            (
                "Page.addScriptToEvaluateOnNewDocument",
                json!({"source":"1","worldName":""}),
            ),
        ] {
            assert_eq!(check(method, params.clone()), None, "{method} {params}");
        }
        for (method, _) in RESET {
            let refused = check(method, json!({"sessionId":"x","targetId":"x"}));
            assert!(
                refused.is_none() || method.starts_with("Target."),
                "{method} resets state the policy refuses: {refused:?}"
            );
        }
    }
    #[test]
    fn tabs_load_only_web_pages() {
        for url in [
            "http://a/",
            "HTTPS://a/",
            "data:text/html,x",
            "blob:http://a/b",
            "about:blank",
        ] {
            assert!(navigable(url), "{url}");
        }
        let none = HashMap::new();
        for url in [
            "file:///etc/passwd",
            "FILE:///etc/passwd",
            " file:///etc/passwd",
            "fi\tle:///etc/passwd",
            "view-source:file:///etc/passwd",
            "chrome://version/",
            "chrome://kill",
            "CHROME://KILL",
            "chrome-untrusted://a/",
            "devtools://devtools/",
            "filesystem:http://a/b",
            "javascript:1",
            "mailto:a@b",
            "",
            "http",
        ] {
            assert!(!navigable(url), "{url}");
            let c = command("Page.navigate", json!({"url": url}));
            assert!(refusal(&c, &none, false).is_some(), "{url}");
        }
        for params in [json!({}), json!({"url": 1})] {
            assert!(refusal(&command("Page.navigate", params), &none, false).is_some());
        }
        let c = command("Page.navigate", json!({"url": "http://a/"}));
        assert_eq!(refusal(&c, &none, false), None);
    }
    #[test]
    fn commands_are_reencoded_under_engine_ids() {
        let mut c = parse(br#"{"id":3,"method":"Page.createIsolatedWorld","params":{"frameId":"f","grantUniveralAccess":true},"sessionId":"s"}"#).unwrap();
        let v: Value = serde_json::from_slice(&encode(&c, 99)).unwrap();
        assert_eq!(v["id"], 99);
        assert_eq!(v["sessionId"], "s");
        assert_eq!(v["params"]["grantUniveralAccess"], false);
        assert_eq!(v["params"]["frameId"], "f");
        c.method = "Runtime.evaluate".into();
        c.session = None;
        let v: Value = serde_json::from_slice(&encode(&c, 5)).unwrap();
        assert!(v.get("sessionId").is_none());
        assert_eq!(leading_id(br#"{"id":12345,"result":{}}"#), Some(12345));
        let next = std::sync::atomic::AtomicU64::new(0);
        assert_eq!(allocate(&next), Some(1));
        let last = std::sync::atomic::AtomicU64::new(CLIENT_IDS - 1);
        assert_eq!(allocate(&last), Some(CLIENT_IDS));
        assert_eq!(allocate(&last), None);
        assert_eq!(allocate(&last), None);
        assert_eq!(leading_id(br#"{"method":"Page.loadEventFired"}"#), None);
        assert!(is_error(br#"{"id":9,"error":{"code":-32000}}"#));
        assert!(!is_error(br#"{"id":9,"result":{"error":1}}"#));
        assert!(!is_error(br#"{"method":"x","error":1}"#));
        assert_eq!(
            leading_method(br#"{"method":"Page.loadEventFired","params":{}}"#),
            Some("Page.loadEventFired")
        );
        assert_eq!(leading_method(br#"{"id":1}"#), None);
        assert_eq!(
            with_id(br#"{"id":4000000,"result":{"a":[[[]]]}}"#, 7).unwrap(),
            br#"{"id":7,"result":{"a":[[[]]]}}"#
        );
    }
    #[test]
    fn network_messages_lose_cookie_headers_only() {
        let mut v = json!({"method":"Network.responseReceivedExtraInfo","params":{
            "headers":{"Set-Cookie":"secret=hidden","Content-Type":"text/html"},
            "headersText":"HTTP/1.1 200\r\nSet-Cookie: secret=hidden",
            "blockedCookies":[{"cookie":{"name":"secret"}}],
            "request":{"headers":{"cookie":"secret=hidden","Authorization":"Basic hidden","Accept":"*/*"}},
            "responseHeaders":[{"name":"set-cookie","value":"secret=hidden"},{"name":"Content-Type","value":"text/html"}],
            "issue":{"details":{"cookieIssueDetails":{"rawCookieLine":"secret=hidden; HttpOnly"}}}}});
        scrub(&mut v);
        let text = v.to_string();
        assert!(!text.contains("hidden"), "{text}");
        assert!(text.contains("text/html") && text.contains("*/*"), "{text}");
        assert!(needs_scrub("Audits.issueAdded") && !needs_scrub("Runtime.evaluate"));
    }
    #[test]
    fn engine_errors_are_cdp_errors() {
        let e: Value = serde_json::from_slice(&error(5, Some("s"), "Page.close", "why")).unwrap();
        assert_eq!(e["id"], 5);
        assert_eq!(e["sessionId"], "s");
        assert_eq!(e["error"]["code"], -32000);
        assert!(
            e["error"]["message"]
                .as_str()
                .unwrap()
                .starts_with("Lanescape browser: Page.close")
        );
    }
    #[test]
    fn reset_commands_are_valid_json() {
        for (method, params) in RESET {
            assert!(method.contains('.'));
            assert!(serde_json::from_str::<Value>(params).unwrap().is_object());
        }
    }
}
