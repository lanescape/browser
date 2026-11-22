//! Whether a renderer is really sandboxed, from the engine's report (engine/platform.h,
//! RendererSandboxReport) and what this process can see of the renderer itself. Each system proves
//! it its own way, and anything short of proof stops the host.
use serde_json::Value;
use std::path::Path;

/// `None` when the renderer has gone before it could be checked: it runs nothing more, and the next
/// page's load asks again. Otherwise whether it is sandboxed, or why not.
pub fn check(report: &Value, data: &Path) -> Option<Result<(), String>> {
    let status = report["status"].as_i64().unwrap_or(0);
    let pid = report["renderer_pid"].as_i64().unwrap_or(0);
    platform::check(status, pid, data)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;

    pub fn check(status: i64, pid: i64, _data: &Path) -> Option<Result<(), String>> {
        let renderer = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        let seccomp = renderer.lines().any(|line| line == "Seccomp:\t2");
        Some(if active(status, seccomp) {
            Ok(())
        } else {
            Err(format!("status {status}"))
        })
    }

    /// The renderer is isolated: a setuid or user-namespace sandbox with a PID namespace, Chromium's
    /// own seccomp filter, and a filter on the process itself. The status alone doesn't prove the
    /// last, and an inherited container filter alone doesn't prove Chromium's. Bits are
    /// sandbox::policy::SandboxLinux::Status.
    pub fn active(status: i64, seccomp_on_process: bool) -> bool {
        const SUID: i64 = 1 << 0;
        const PID_NAMESPACE: i64 = 1 << 1;
        const SECCOMP_BPF: i64 = 1 << 3;
        const USER_NAMESPACE: i64 = 1 << 6;
        status > 0
            && status & (SUID | USER_NAMESPACE) != 0
            && status & PID_NAMESPACE != 0
            && status & SECCOMP_BPF != 0
            && seccomp_on_process
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CString, c_char, c_int},
        os::unix::ffi::OsStrExt,
        path::Path,
    };

    unsafe extern "C" {
        /// In libSystem, though not in the SDK's headers: 1 means the process is sandboxed, or, given
        /// an operation, that its sandbox denies it; -1 an error, such as no such process.
        fn sandbox_check(pid: libc::pid_t, operation: *const c_char, kind: c_int, ...) -> c_int;
    }
    const SANDBOX_FILTER_PATH: c_int = 1;

    pub fn check(status: i64, pid: i64, data: &Path) -> Option<Result<(), String>> {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return Some(Err(format!("renderer {pid}")));
        };
        let Ok(path) = CString::new(data.as_os_str().as_bytes()) else {
            return Some(Err("the data directory has no path to check".into()));
        };
        // The system says a process that doesn't exist is sandboxed, so the answers count only if
        // the same live child of this process was there before and after them. A renderer that is
        // this process itself, as in single-process mode, or anyone else's, fails.
        let before = match child(pid)? {
            Ok(started) => started,
            Err(why) => return Some(Err(why)),
        };
        // SAFETY: a null operation asks only whether the process is sandboxed.
        let sandboxed = unsafe { sandbox_check(pid, std::ptr::null(), 0) };
        // SAFETY: a path filter takes one NUL-terminated path.
        let reads_data = unsafe {
            sandbox_check(
                pid,
                c"file-read-data".as_ptr(),
                SANDBOX_FILTER_PATH,
                path.as_ptr(),
            )
        };
        if child(pid)?.ok() != Some(before) {
            return None;
        }
        Some(if active(status, sandboxed, reads_data) {
            Ok(())
        } else {
            Err(format!(
                "status {status}, sandboxed {sandboxed}, data directory {}",
                if reads_data == 1 { "denied" } else { "allowed" }
            ))
        })
    }

    /// When a running child of this process started, which tells it apart from any process that
    /// later has its ID. `None` if it has gone, or exited and runs nothing more; an error if it is
    /// anything but a live child, or can't be looked at.
    fn child(pid: libc::pid_t) -> Option<Result<(u64, u64), String>> {
        // Chromium names no process for a renderer it hasn't launched yet, or has lost.
        if pid == 0 {
            return None;
        }
        if pid < 0 || pid as u32 == std::process::id() {
            return Some(Err(format!("renderer {pid} is not a process of its own")));
        }
        // SAFETY: proc_bsdinfo is plain data, filled in whole or not at all.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
        // SAFETY: the buffer is the size given.
        let filled = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if filled != size {
            let error = std::io::Error::last_os_error();
            return (error.raw_os_error() != Some(libc::ESRCH))
                .then(|| Err(format!("renderer {pid}: {error}")));
        }
        const ZOMBIE: u32 = 5;
        if info.pbi_status == ZOMBIE {
            return None;
        }
        Some(if info.pbi_ppid == std::process::id() {
            Ok((info.pbi_start_tvsec, info.pbi_start_tvusec))
        } else {
            Err(format!("renderer {pid} is not this process's child"))
        })
    }

    /// The renderer is under Seatbelt by Chromium's account and by the system's, and its policy is
    /// one that keeps it out of the profiles' data: a permissive sandbox wouldn't.
    pub fn active(status: i64, sandboxed: c_int, data_denied: c_int) -> bool {
        status == 1 && sandboxed == 1 && data_denied == 1
    }
}

#[cfg(test)]
mod tests {
    use super::platform::active;

    #[cfg(target_os = "linux")]
    #[test]
    fn sandbox_needs_every_layer() {
        assert!(active(0b0110_1110, true));
        // An inherited filter must not stand in for Chromium's.
        assert!(!active(0b0110_0110, true));
        assert!(!active(0b0110_1110, false));
        assert!(!active(0b0000_1000, true));
        assert!(!active(i64::from(i32::MIN), true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_needs_every_layer() {
        assert!(active(1, 1, 1));
        // Chromium's word alone, or the system's, isn't enough, nor is a sandbox that lets the
        // renderer read the profiles.
        assert!(!active(0, 1, 1));
        assert!(!active(1, 0, 1));
        assert!(!active(1, 1, 0));
        assert!(!active(1, -1, -1));
    }

    /// Only a live child counts: the system calls a process that doesn't exist sandboxed, and a
    /// renderer that is this process, or another's, is no sandboxed renderer.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_a_live_child_is_checked() {
        let data = std::path::Path::new("/");
        let gone = serde_json::json!({"status": 1, "renderer_pid": i32::MAX});
        assert_eq!(super::check(&gone, data), None);
        // A renderer in this very process, as in single-process mode, or someone else's process.
        for pid in [std::process::id(), 1] {
            let other = serde_json::json!({"status": 1, "renderer_pid": pid});
            assert!(matches!(super::check(&other, data), Some(Err(_))), "{pid}");
        }
        // A child that may read what this test can is no renderer, sandboxed or not.
        let mut sleeper = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let child = serde_json::json!({"status": 1, "renderer_pid": sleeper.id()});
        let checked = super::check(&child, &std::env::current_exe().unwrap());
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        assert!(matches!(checked, Some(Err(_))), "{checked:?}");
    }
}
