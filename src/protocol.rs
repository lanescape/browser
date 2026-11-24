use prost::Message;
use serde_json::{Value, json};
use std::io::{self, Read, Write};
pub mod control {
    include!(concat!(env!("OUT_DIR"), "/lanescape.browser.rs"));
}
/// The package major is the protocol major; Version reports it.
pub const MAJOR: u32 = {
    let digits = env!("CARGO_PKG_VERSION_MAJOR").as_bytes();
    let (mut major, mut i) = (0, 0);
    while i < digits.len() {
        major = major * 10 + (digits[i] - b'0') as u32;
        i += 1;
    }
    major
};
/// The protocol's own copy of stdout, once [`claim_stdout`] has taken it.
static OUTPUT: std::sync::OnceLock<std::sync::Mutex<std::fs::File>> = std::sync::OnceLock::new();
/// Keeps the protocol's stream from everything else in the process: a line a library or helper
/// process prints to stdout would otherwise corrupt its framing. They get stderr instead.
pub fn claim_stdout() -> io::Result<()> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 10) };
    if fd < 0 || unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = OUTPUT.set(std::sync::Mutex::new(unsafe {
        std::fs::File::from_raw_fd(fd)
    }));
    Ok(())
}
pub fn read(input: &mut impl Read) -> io::Result<Option<Value>> {
    let mut length = [0; 4];
    let n = input.read(&mut length[..1])?;
    if n == 0 {
        return Ok(None);
    }
    input.read_exact(&mut length[1..])?;
    let n = u32::from_le_bytes(length) as usize;
    if n == 0 || n > 65536 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; n];
    input.read_exact(&mut bytes)?;
    let r = control::Request::decode(&*bytes).map_err(|_| io::ErrorKind::InvalidData)?;
    use control::request::Command;
    let (method, params) = match r.command.ok_or(io::ErrorKind::InvalidData)? {
        Command::Initialize(p) => {
            if p.capabilities.len() > 32
                || p.capabilities.iter().any(|token| {
                    token.is_empty()
                        || token.len() > 64
                        || !token.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                        })
                })
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            (
                "initialize",
                json!({"capabilities":p.capabilities,"discard_saved_tabs":p.discard_saved_tabs}),
            )
        }
        Command::Version(_) => ("version", json!({})),
        Command::Status(_) => ("status", json!({})),
        Command::ListTabs(_) => ("tab.list", json!({})),
        Command::Shutdown(_) => ("shutdown", json!({})),
        Command::CreateTab(p) => (
            "tab.create",
            json!({"profile":p.profile,"private":p.private,"url":if p.url.is_empty(){"about:blank".into()}else{p.url},"width":if p.width==0{1280}else{p.width},"height":if p.height==0{800}else{p.height}}),
        ),
        Command::ReleaseProfile(p) => ("profile.release", json!({"profile":p.profile})),
        Command::OpenDevtools(p) => ("devtools.open", json!({"tab":p.tab})),
        Command::StartRecording(p) => ("recording.start", json!({"tab":p.tab})),
        Command::StopRecording(p) => ("recording.stop", json!({"tab":p.tab})),
        Command::CloseDevtools(p) => ("devtools.close", json!({"session":p.session})),
        Command::SetUserPreferences(p) => {
            let p = p.preferences.unwrap_or_default();
            (
                "preferences.set",
                json!({"color_scheme":p.color_scheme,"timezone":p.timezone,"languages":p.languages}),
            )
        }
        Command::DialogReply(p) => (
            "dialog.reply",
            json!({"tab":p.tab,"request":p.request,"accept":p.accept,"text":p.text}),
        ),
        Command::FileDialogReply(p) => (
            "file.reply",
            json!({"tab":p.tab,"request":p.request,"cancel":p.cancel,"paths":p.server_paths}),
        ),
        Command::DownloadReply(p) => (
            "download.reply",
            json!({"tab":p.tab,"request":p.request,"cancel":p.cancel,"path":p.server_path}),
        ),
        Command::CancelDownload(p) => (
            "download.cancel",
            json!({"tab":p.tab,"download":p.download}),
        ),
        Command::ContextMenuReply(p) => (
            "menu.reply",
            json!({"tab":p.tab,"request":p.request,"cancel":p.cancel,"command":p.command}),
        ),
        Command::ImeInput(p) => (
            "ime.input",
            json!({"tab":p.tab,"viewer":p.viewer,"editor":p.editor,"action":p.action,"text":p.text,"selection_start":p.selection_start,"selection_end":p.selection_end,"underlines":p.underlines.iter().map(|u| json!({"start":u.start,"end":u.end,"color":u.color,"background_color":u.background_color,"thick":u.thick,"style":u.style})).collect::<Vec<_>>(),"after_rfb_input":p.after_rfb_input}),
        ),
        Command::ClipboardTransfer(p) => (
            "clipboard.transfer",
            json!({"tab":p.tab,"viewer":p.viewer,"action":p.action,"transfer":p.transfer,"format":p.format,"size":p.size,"offset":p.offset,"data":p.data}),
        ),
        Command::ReadClipboardTransfer(p) => (
            "clipboard.read",
            json!({"tab":p.tab,"viewer":p.viewer,"transfer":p.transfer,"offset":p.offset,"length":p.length}),
        ),
        Command::ClipboardOperation(p) => (
            "clipboard.operation",
            json!({"tab":p.tab,"viewer":p.viewer,"kind":p.kind,"accept":p.accept,"transfers":p.transfers,"menu_request":p.menu_request,"after_rfb_input":p.after_rfb_input}),
        ),
        Command::DisplayFence(p) => ("display.fence", json!({"tab":p.tab})),
        Command::TabOperation(p) => {
            let method = match p.action {
                1 => "tab.navigate",
                2 => "tab.back",
                3 => "tab.forward",
                4 => "tab.reload",
                5 => "tab.stop",
                6 => "tab.resize",
                7 => "viewer.control",
                8 => "tab.close",
                _ => return Err(io::ErrorKind::InvalidData.into()),
            };
            (
                method,
                json!({"tab":p.tab,"url":p.url,"width":p.width,"height":p.height,"viewer":p.viewer}),
            )
        }
    };
    Ok(Some(json!({"id":r.id,"method":method,"params":params})))
}
pub fn write(v: Value) {
    if let Some(output) = output(v) {
        emit(output);
    }
}
/// The protobuf a host response or event becomes, if it is one the protocol carries.
fn output(v: Value) -> Option<control::Output> {
    use control::{event::Event as E, output::Body, response::Result as R};
    let number = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0);
    let string = |v: &Value, key: &str| {
        let text = v[key].as_str().unwrap_or("");
        let mut end = text.len().min(16384);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text[..end].to_owned()
    };
    let strings = |v: &Value| {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let body = if let Some(id) = v["id"].as_u64() {
        let result = if v.get("error").is_some() {
            R::Error(control::Error {
                message: string(&v["error"], "message"),
            })
        } else {
            let p = &v["result"];
            if let Some(version) = p.get("version") {
                R::Version(control::Version {
                    major: MAJOR,
                    build: version["build"].as_str().map(str::to_owned),
                })
            } else if p.get("recording").is_some() {
                R::Recording(control::Recording {
                    tab: number(p, "tab"),
                    recording: number(p, "recording"),
                    socket: string(p, "socket"),
                })
            } else if p.get("session").is_some() {
                R::Devtools(control::DevTools {
                    tab: number(p, "tab"),
                    session: number(p, "session"),
                    socket: string(p, "socket"),
                })
            } else if p.get("backend").is_some() {
                R::Status(control::Status {
                    gpu_accelerated: p["gpu_accelerated"] == true,
                    backend: string(p, "backend"),
                    fallback_reason: string(p, "fallback_reason"),
                    capabilities: strings(&p["capabilities"]),
                    clipboard_formats: strings(&p["clipboard_formats"]),
                    previous_exit: p["previous_exit"].as_i64().unwrap_or(0) as i32,
                })
            } else if p.get("sequence").is_some() {
                R::DisplayFenced(control::DisplayFenced {
                    tab: number(p, "tab"),
                    sequence: number(p, "sequence"),
                })
            } else if let Some(data) = p.get("data").and_then(Value::as_array) {
                R::ClipboardChunk(control::ClipboardChunk {
                    transfer: number(p, "transfer"),
                    offset: number(p, "offset"),
                    data: data.iter().map(|b| b.as_u64().unwrap_or(0) as u8).collect(),
                    end: p["end"] == true,
                })
            } else if p.get("transfer").is_some() {
                R::ClipboardTransfer(control::ClipboardRepresentation {
                    transfer: number(p, "transfer"),
                    format: string(p, "format"),
                    size: number(p, "size"),
                })
            } else if p.get("tab").is_some() {
                R::Tab(control::Tab {
                    tab: number(p, "tab"),
                    display: string(p, "display"),
                    profile: string(p, "profile"),
                })
            } else if let Some(t) = p.as_array() {
                R::Tabs(control::Tabs {
                    tabs: t.iter().map(|t| number(t, "tab")).collect(),
                    details: t
                        .iter()
                        .map(|t| control::Tab {
                            tab: number(t, "tab"),
                            display: string(t, "display"),
                            profile: string(t, "profile"),
                        })
                        .collect(),
                })
            } else {
                R::Ok(control::Empty {})
            }
        };
        Body::Response(control::Response {
            id,
            result: Some(result),
        })
    } else {
        let p = &v["params"];
        let event = match v["method"].as_str() {
            Some("tab.loading_state") => E::LoadingState(control::LoadingState {
                tab: number(p, "tab"),
                is_loading: p["is_loading"] == true,
                can_go_back: p["can_go_back"] == true,
                can_go_forward: p["can_go_forward"] == true,
            }),
            Some("tab.loaded") => E::Loaded(control::Loaded {
                tab: number(p, "tab"),
                url: string(p, "url"),
                http_status: p["http_status"].as_i64().unwrap_or(0) as i32,
            }),
            Some("tab.popup") => E::Popup(control::Popup {
                opener: number(p, "opener"),
                tab: Some(control::Tab {
                    tab: number(p, "tab"),
                    display: string(p, "display"),
                    profile: string(p, "profile"),
                }),
            }),
            Some("dialog") => E::Dialog(control::Dialog {
                tab: number(p, "tab"),
                request: number(p, "request"),
                kind: p["kind"].as_i64().unwrap_or(0) as i32,
                message: string(p, "message"),
                default_text: string(p, "default_text"),
            }),
            Some("recording.ended") => E::RecordingEnded(control::RecordingEnded {
                tab: number(p, "tab"),
                recording: number(p, "recording"),
                reason: p["reason"].as_i64().unwrap_or(0) as i32,
                frames: number(p, "frames"),
                dropped: number(p, "dropped"),
            }),
            Some("devtools.closed") => E::DevtoolsClosed(control::DevToolsClosed {
                tab: number(p, "tab"),
                session: number(p, "session"),
                reason: p["reason"].as_i64().unwrap_or(0) as i32,
            }),
            Some("tab.closed") => E::Closed(control::Closed {
                tab: number(p, "tab"),
            }),
            Some("tab.title") => E::Title(control::Title {
                tab: number(p, "tab"),
                title: string(p, "title"),
            }),
            Some("tab.navigation") => E::Navigation(control::Navigation {
                tab: number(p, "tab"),
                url: string(p, "url"),
            }),
            Some("viewer.connected" | "viewer.disconnected") => E::Viewer(control::Viewer {
                tab: number(p, "tab"),
                viewer: number(p, "viewer"),
                connected: v["method"] == "viewer.connected",
            }),
            _ => return None,
        };
        Body::Event(control::Event { event: Some(event) })
    };
    Some(control::Output { body: Some(body) })
}
pub fn emit(output: control::Output) -> bool {
    let data = output.encode_to_vec();
    if data.len() > 65536 {
        return false;
    }
    let mut frame = (data.len() as u32).to_le_bytes().to_vec();
    frame.extend(data);
    match OUTPUT.get() {
        Some(output) => output.lock().unwrap().write_all(&frame).is_ok(),
        None => {
            let mut out = io::stdout().lock();
            out.write_all(&frame).and_then(|()| out.flush()).is_ok()
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn golden_wire_numbers() {
        let request = [4, 0, 0, 0, 8, 42, 34, 0];
        assert_eq!(
            read(&mut &request[..]).unwrap().unwrap()["method"],
            "status"
        );
        let status = control::Status {
            backend: "cpu".into(),
            ..Default::default()
        };
        let response = control::Response {
            id: 42,
            result: Some(control::response::Result::Status(status)),
        };
        let output = control::Output {
            body: Some(control::output::Body::Response(response)),
        };
        assert_eq!(
            output.encode_to_vec(),
            [10, 9, 8, 42, 18, 5, 18, 3, 99, 112, 117]
        );
    }
    #[test]
    fn version_handshake_wire_is_frozen_across_majors() {
        // Clients of any major must decode this exchange; never renumber it.
        let request = [4, 0, 0, 0, 8, 7, 18, 0];
        let decoded = read(&mut &request[..]).unwrap().unwrap();
        assert_eq!(decoded["id"], 7);
        assert_eq!(decoded["method"], "version");
        let response = |result| {
            control::Output {
                body: Some(control::output::Body::Response(control::Response {
                    id: 7,
                    result: Some(result),
                })),
            }
            .encode_to_vec()
        };
        assert_eq!(
            response(control::response::Result::Version(control::Version {
                major: 3,
                build: Some("b".into()),
            })),
            [10, 9, 8, 7, 58, 5, 8, 3, 18, 1, 98]
        );
        assert_eq!(
            response(control::response::Result::Error(control::Error {
                message: "x".into(),
            })),
            [10, 7, 8, 7, 50, 3, 10, 1, 120]
        );
        assert_eq!(MAJOR.to_string(), env!("CARGO_PKG_VERSION_MAJOR"));
    }
    #[test]
    fn request_framing() {
        let r = control::Request {
            id: 42,
            command: Some(control::request::Command::Status(control::Empty {})),
        };
        let data = r.encode_to_vec();
        let mut b = (data.len() as u32).to_le_bytes().to_vec();
        b.extend(data);
        assert_eq!(read(&mut &b[..]).unwrap().unwrap()["id"], 42);
        assert!(read(&mut &[0, 0, 1, 0][..]).is_err());
        assert!(read(&mut &[1, 0][..]).is_err());
    }
    #[test]
    fn initialization_accepts_an_empty_message_and_bounds_capability_tokens() {
        let empty = [4, 0, 0, 0, 8, 1, 26, 0];
        assert_eq!(
            read(&mut &empty[..]).unwrap().unwrap()["params"]["capabilities"],
            json!([])
        );
        let initialize = |capabilities| {
            let data = control::Request {
                id: 1,
                command: Some(control::request::Command::Initialize(control::Initialize {
                    capabilities,
                    ..Default::default()
                })),
            }
            .encode_to_vec();
            let mut frame = (data.len() as u32).to_le_bytes().to_vec();
            frame.extend(data);
            read(&mut &frame[..])
        };
        let tokens = vec!["loading-state".into(), "future.v99".into()];
        assert_eq!(
            initialize(tokens.clone()).unwrap().unwrap()["params"]["capabilities"],
            json!(tokens)
        );
        assert!(initialize(vec!["a".repeat(64); 32]).is_ok());
        for tokens in [
            vec!["a".into(); 33],
            vec!["a".repeat(65)],
            vec![String::new()],
            vec!["BAD".into()],
            vec!["water/水".into()],
        ] {
            assert!(initialize(tokens).is_err());
        }
    }
    #[test]
    fn restart_wire_numbers() {
        let status = control::Status {
            previous_exit: control::status::PreviousExit::Crashed as i32,
            ..Default::default()
        };
        assert_eq!(status.encode_to_vec(), [48, 2]);
        let tabs = control::Tabs {
            tabs: vec![7],
            details: vec![control::Tab {
                tab: 7,
                ..Default::default()
            }],
        };
        assert_eq!(tabs.encode_to_vec(), [10, 1, 7, 18, 2, 8, 7]);
        let initialize = [6, 0, 0, 0, 8, 1, 26, 2, 16, 1];
        assert_eq!(
            read(&mut &initialize[..]).unwrap().unwrap()["params"]["discard_saved_tabs"],
            true
        );
        // Tabs and the previous exit, as the host describes them.
        for (result, expected) in [
            (
                json!([{"tab":7,"profile":"~ab","display":"/run/7.sock"}]),
                control::response::Result::Tabs(control::Tabs {
                    tabs: vec![7],
                    details: vec![control::Tab {
                        tab: 7,
                        display: "/run/7.sock".into(),
                        profile: "~ab".into(),
                    }],
                }),
            ),
            (
                json!({"backend":"cpu","previous_exit":1}),
                control::response::Result::Status(control::Status {
                    backend: "cpu".into(),
                    previous_exit: 1,
                    ..Default::default()
                }),
            ),
        ] {
            let output = output(json!({"id":1,"result":result})).unwrap();
            let Some(control::output::Body::Response(response)) = output.body else {
                panic!("{output:?}");
            };
            assert_eq!(response.result, Some(expected));
        }
    }
    #[test]
    fn loading_state_and_capability_wire_numbers() {
        let status = control::Status {
            capabilities: vec!["loading-state".into()],
            ..Default::default()
        };
        let mut expected = vec![34, 13];
        expected.extend(b"loading-state");
        assert_eq!(status.encode_to_vec(), expected);
        let state = control::Event {
            event: Some(control::event::Event::LoadingState(control::LoadingState {
                tab: 42,
                is_loading: true,
                can_go_back: true,
                can_go_forward: true,
            })),
        };
        assert_eq!(state.encode_to_vec(), [66, 8, 8, 42, 16, 1, 24, 1, 32, 1]);
    }
}

#[cfg(test)]
mod native_wire_tests {
    use super::*;
    #[test]
    fn native_requests_keep_additive_wire_tags() {
        let cases = [
            (
                control::request::Command::FileDialogReply(control::FileDialogReply {
                    tab: 7,
                    request: 9,
                    cancel: true,
                    ..Default::default()
                }),
                90,
                "file.reply",
            ),
            (
                control::request::Command::DownloadReply(control::DownloadReply {
                    tab: 7,
                    request: 9,
                    cancel: true,
                    ..Default::default()
                }),
                98,
                "download.reply",
            ),
            (
                control::request::Command::CancelDownload(control::CancelDownload {
                    tab: 7,
                    download: 9,
                }),
                106,
                "download.cancel",
            ),
            (
                control::request::Command::ContextMenuReply(control::ContextMenuReply {
                    tab: 7,
                    request: 9,
                    cancel: true,
                    ..Default::default()
                }),
                114,
                "menu.reply",
            ),
        ];
        for (command, tag, method) in cases {
            let bytes = control::Request {
                id: 42,
                command: Some(command),
            }
            .encode_to_vec();
            assert_eq!(bytes[2], tag);
            let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
            framed.extend(bytes);
            let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
            assert_eq!(decoded["method"], method);
            assert_eq!(decoded["params"]["tab"], 7);
        }
        let event = control::Event {
            event: Some(control::event::Event::ContextMenu(
                control::ContextMenu::default(),
            )),
        };
        assert_eq!(event.encode_to_vec(), [98, 0]);
    }
    #[test]
    fn profile_and_devtools_wire_numbers() {
        use control::request::Command;
        let cases = [
            (
                Command::ReleaseProfile(control::ReleaseProfile {
                    profile: "p".into(),
                }),
                82,
                "profile.release",
                ("profile", json!("p")),
            ),
            (
                Command::OpenDevtools(control::OpenDevTools { tab: 7 }),
                122,
                "devtools.open",
                ("tab", json!(7)),
            ),
            (
                Command::CloseDevtools(control::CloseDevTools { session: 7 }),
                130,
                "devtools.close",
                ("session", json!(7)),
            ),
        ];
        for (command, tag, method, (key, value)) in cases {
            let bytes = control::Request {
                id: 42,
                command: Some(command),
            }
            .encode_to_vec();
            assert_eq!(bytes[2], tag);
            let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
            framed.extend(bytes);
            let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
            assert_eq!(decoded["method"], method);
            assert_eq!(decoded["params"][key], value);
        }
        let tab = control::CreateTab {
            profile: "p".into(),
            ..Default::default()
        };
        assert_eq!(tab.encode_to_vec(), [10, 1, 112]);
        // Field 2 is the old `private` flag, which the host refuses rather than ignore: ignored,
        // an old client's private tab would get the named profile's storage.
        let legacy = control::Request {
            id: 1,
            command: Some(Command::CreateTab(control::CreateTab {
                profile: "work".into(),
                ..Default::default()
            })),
        };
        let mut bytes = legacy.encode_to_vec();
        bytes.extend([16, 1]);
        bytes[3] += 2;
        let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
        framed.extend(bytes);
        let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
        assert_eq!(decoded["params"]["profile"], "work");
        assert_eq!(decoded["params"]["private"], true);
        let tab = control::Tab {
            tab: 7,
            display: "d".into(),
            profile: "~p".into(),
        };
        assert_eq!(tab.encode_to_vec(), [8, 7, 26, 1, 100, 34, 2, 126, 112]);
        let response = control::Response {
            id: 1,
            result: Some(control::response::Result::Devtools(
                control::DevTools::default(),
            )),
        };
        assert_eq!(response.encode_to_vec(), [8, 1, 66, 0]);
        let closed = control::Event {
            event: Some(control::event::Event::DevtoolsClosed(
                control::DevToolsClosed::default(),
            )),
        };
        assert_eq!(closed.encode_to_vec(), [114, 0]);
    }
    #[test]
    fn display_fence_wire_numbers() {
        let request = control::Request {
            id: 42,
            command: Some(control::request::Command::DisplayFence(
                control::DisplayFence { tab: 7 },
            )),
        }
        .encode_to_vec();
        assert_eq!(request, [8, 42, 178, 1, 2, 8, 7]);
        let mut framed = (request.len() as u32).to_le_bytes().to_vec();
        framed.extend(request);
        let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
        assert_eq!(decoded["method"], "display.fence");
        assert_eq!(decoded["params"]["tab"], 7);
        let response = control::Response {
            id: 1,
            result: Some(control::response::Result::DisplayFenced(
                control::DisplayFenced {
                    tab: 7,
                    sequence: 9,
                },
            )),
        };
        assert_eq!(response.encode_to_vec(), [8, 1, 90, 4, 8, 7, 16, 9]);
    }
    #[test]
    fn user_preferences_wire_numbers() {
        let preferences = control::UserPreferences {
            color_scheme: control::user_preferences::ColorScheme::Dark as i32,
            timezone: "Asia/Tokyo".into(),
            languages: vec!["fr-CA".into(), "en".into()],
        };
        let mut encoded = vec![8, 2, 18, 10];
        encoded.extend(b"Asia/Tokyo");
        encoded.extend([26, 5]);
        encoded.extend(b"fr-CA");
        encoded.extend([26, 2]);
        encoded.extend(b"en");
        assert_eq!(preferences.encode_to_vec(), encoded);
        let bytes = control::Request {
            id: 42,
            command: Some(control::request::Command::SetUserPreferences(
                control::SetUserPreferences {
                    preferences: Some(preferences),
                },
            )),
        }
        .encode_to_vec();
        // Field 17, length-delimited, is the two-byte tag 0x8a 0x01.
        assert_eq!(&bytes[2..5], [0x8a, 0x01, encoded.len() as u8 + 2]);
        assert_eq!(bytes[5], 10);
        let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
        framed.extend(bytes);
        let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
        assert_eq!(decoded["method"], "preferences.set");
        assert_eq!(
            decoded["params"],
            json!({"color_scheme":2,"timezone":"Asia/Tokyo","languages":["fr-CA","en"]})
        );
        let empty = control::Request {
            id: 1,
            command: Some(control::request::Command::SetUserPreferences(
                control::SetUserPreferences::default(),
            )),
        }
        .encode_to_vec();
        let mut framed = (empty.len() as u32).to_le_bytes().to_vec();
        framed.extend(empty);
        let decoded = read(&mut framed.as_slice()).unwrap().unwrap();
        assert_eq!(
            decoded["params"],
            json!({"color_scheme":0,"timezone":"","languages":[]})
        );
    }
}

#[cfg(test)]
mod ime_clipboard_wire_tests {
    use super::*;
    use control::request::Command;
    fn decode(command: Command) -> (Vec<u8>, Value) {
        let bytes = control::Request {
            id: 42,
            command: Some(command),
        }
        .encode_to_vec();
        let mut framed = (bytes.len() as u32).to_le_bytes().to_vec();
        framed.extend(&bytes);
        (bytes, read(&mut framed.as_slice()).unwrap().unwrap())
    }
    #[test]
    fn ime_and_clipboard_requests_keep_additive_wire_tags() {
        let ime = control::ImeInput {
            tab: 7,
            viewer: 2,
            editor: 3,
            action: control::ime_input::Action::Preedit as i32,
            text: "日本".into(),
            selection_start: 1,
            selection_end: 2,
            underlines: vec![control::ImeUnderline {
                end: 2,
                thick: true,
                style: control::ime_underline::Style::Dash as i32,
                ..Default::default()
            }],
            after_rfb_input: 9,
        };
        let (bytes, decoded) = decode(Command::ImeInput(ime));
        // Field 18, length-delimited, is the two-byte tag 0x92 0x01.
        assert_eq!(bytes[2..4], [0x92, 0x01]);
        assert_eq!(decoded["method"], "ime.input");
        assert_eq!(
            decoded["params"],
            json!({"tab":7,"viewer":2,"editor":3,"action":1,"text":"日本","selection_start":1,"selection_end":2,"underlines":[{"start":0,"end":2,"color":0,"background_color":0,"thick":true,"style":2}],"after_rfb_input":9})
        );
        let (bytes, decoded) = decode(Command::ClipboardTransfer(control::ClipboardTransfer {
            tab: 7,
            viewer: 2,
            action: control::clipboard_transfer::Action::Write as i32,
            transfer: 5,
            offset: 3,
            data: vec![0, 255],
            ..Default::default()
        }));
        assert_eq!(bytes[2..4], [0x9a, 0x01]);
        assert_eq!(decoded["method"], "clipboard.transfer");
        assert_eq!(
            decoded["params"],
            json!({"tab":7,"viewer":2,"action":2,"transfer":5,"format":"","size":0,"offset":3,"data":[0,255]})
        );
        let (bytes, decoded) = decode(Command::ReadClipboardTransfer(
            control::ReadClipboardTransfer {
                tab: 7,
                viewer: 2,
                transfer: 5,
                offset: 3,
                length: 4,
            },
        ));
        assert_eq!(bytes[2..4], [0xa2, 0x01]);
        assert_eq!(decoded["method"], "clipboard.read");
        assert_eq!(
            decoded["params"],
            json!({"tab":7,"viewer":2,"transfer":5,"offset":3,"length":4})
        );
        let (bytes, decoded) = decode(Command::ClipboardOperation(control::ClipboardOperation {
            tab: 7,
            viewer: 2,
            kind: control::clipboard_operation::Kind::PastePlain as i32,
            accept: vec!["text/plain".into()],
            transfers: vec![5, 6],
            menu_request: 8,
            after_rfb_input: 9,
        }));
        assert_eq!(bytes[2..4], [0xaa, 0x01]);
        assert_eq!(decoded["method"], "clipboard.operation");
        assert_eq!(
            decoded["params"],
            json!({"tab":7,"viewer":2,"kind":4,"accept":["text/plain"],"transfers":[5,6],"menu_request":8,"after_rfb_input":9})
        );
    }
    #[test]
    fn ime_and_clipboard_output_wire_numbers() {
        let status = control::Status {
            clipboard_formats: vec!["text/plain".into()],
            ..Default::default()
        };
        let mut expected = vec![42, 10];
        expected.extend(b"text/plain");
        assert_eq!(status.encode_to_vec(), expected);
        let response = |result| {
            control::Response {
                id: 1,
                result: Some(result),
            }
            .encode_to_vec()
        };
        use control::response::Result as R;
        assert_eq!(
            response(R::ClipboardTransfer(control::ClipboardRepresentation {
                transfer: 5,
                format: "a".into(),
                size: 1,
            })),
            [8, 1, 74, 7, 8, 5, 18, 1, 97, 24, 1]
        );
        assert_eq!(
            response(R::ClipboardChunk(control::ClipboardChunk {
                transfer: 5,
                offset: 1,
                data: vec![7],
                end: true,
            })),
            [8, 1, 82, 9, 8, 5, 16, 1, 26, 1, 7, 32, 1]
        );
        use control::event::Event as E;
        let event = |e| control::Event { event: Some(e) }.encode_to_vec();
        assert_eq!(
            event(E::EditorState(control::EditorState {
                tab: 7,
                viewer: 2,
                editor: 3,
                composing: true,
                caret: Some(control::Rect {
                    x: 1,
                    y: 2,
                    width: 3,
                    height: 4,
                }),
                character_bounds: vec![control::Rect::default()],
                inline_composition: true,
            })),
            [
                122, 22, 8, 7, 16, 2, 24, 3, 32, 1, 42, 8, 8, 1, 16, 2, 24, 3, 32, 4, 50, 0, 56, 1
            ]
        );
        assert_eq!(
            event(E::ClipboardCopied(control::ClipboardCopied {
                tab: 7,
                operation: 42,
                representations: vec![control::ClipboardRepresentation::default()],
                ..Default::default()
            })),
            [0x82, 0x01, 6, 8, 7, 24, 42, 42, 0]
        );
        assert_eq!(
            event(E::ClipboardFinished(control::ClipboardFinished {
                tab: 7,
                operation: 42,
                status: control::clipboard_finished::Status::Cancelled as i32,
                ..Default::default()
            })),
            [0x8a, 0x01, 6, 8, 7, 24, 42, 40, 6]
        );
    }
}
#[cfg(test)]
mod ime_clipboard_nested_wire_tests {
    use super::*;
    /// Every field set to a distinct value, so renumbering any nested field fails.
    #[test]
    fn nested_fields_keep_their_numbers() {
        let cases: [(Vec<u8>, &[u8]); 7] = [
            (
                control::ImeUnderline {
                    start: 1,
                    end: 2,
                    color: 3,
                    background_color: 4,
                    thick: true,
                    style: 2,
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 32, 4, 40, 1, 48, 2],
            ),
            (
                control::ImeInput {
                    tab: 1,
                    viewer: 2,
                    editor: 3,
                    action: 4,
                    text: "a".into(),
                    selection_start: 6,
                    selection_end: 7,
                    underlines: vec![control::ImeUnderline::default()],
                    after_rfb_input: 9,
                }
                .encode_to_vec(),
                &[
                    8, 1, 16, 2, 24, 3, 32, 4, 42, 1, 97, 48, 6, 56, 7, 66, 0, 72, 9,
                ],
            ),
            (
                control::ClipboardTransfer {
                    tab: 1,
                    viewer: 2,
                    action: 3,
                    transfer: 4,
                    format: "a".into(),
                    size: 6,
                    offset: 7,
                    data: vec![8],
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 32, 4, 42, 1, 97, 48, 6, 56, 7, 66, 1, 8],
            ),
            (
                control::ReadClipboardTransfer {
                    tab: 1,
                    viewer: 2,
                    transfer: 3,
                    offset: 4,
                    length: 5,
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 32, 4, 40, 5],
            ),
            (
                control::ClipboardOperation {
                    tab: 1,
                    viewer: 2,
                    kind: 3,
                    accept: vec!["a".into()],
                    transfers: vec![5],
                    menu_request: 6,
                    after_rfb_input: 7,
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 34, 1, 97, 42, 1, 5, 48, 6, 56, 7],
            ),
            (
                control::ClipboardCopied {
                    tab: 1,
                    viewer: 2,
                    operation: 3,
                    menu_request: 4,
                    representations: vec![Default::default()],
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 32, 4, 42, 0],
            ),
            (
                control::ClipboardFinished {
                    tab: 1,
                    viewer: 2,
                    operation: 3,
                    menu_request: 4,
                    status: 5,
                }
                .encode_to_vec(),
                &[8, 1, 16, 2, 24, 3, 32, 4, 40, 5],
            ),
        ];
        for (encoded, golden) in cases {
            assert_eq!(encoded, golden);
        }
    }
}
