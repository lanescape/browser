//! What the suites look up about the host's processes, which each system tells its own way: Linux
//! through /proc, macOS through libproc and the sandbox library.
use std::path::Path;

/// The file a process has open as `fd`, as something two answers can be compared by, or `None` if
/// the process or descriptor is gone.
pub fn open_file(pid: i32, fd: i32) -> Option<String> {
    platform::open_file(pid, fd)
}

/// A process's command line, its arguments joined by spaces.
pub fn command_line(pid: i32) -> Option<String> {
    platform::command_line(pid)
}

/// The CPU time a process has used, in hundredths of a second.
pub fn cpu_ticks(pid: i32) -> Option<u64> {
    platform::cpu_ticks(pid)
}

/// Whether a renderer is under Chromium's own sandbox, judged by what the system says of the process
/// rather than by what Lanescape browser reports. `data` is the host's data directory.
pub fn renderer_sandbox(pid: i32, data: &Path) -> Result<(), String> {
    platform::renderer_sandbox(pid, data)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::path::Path;

    pub fn open_file(pid: i32, fd: i32) -> Option<String> {
        std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    }

    pub fn command_line(pid: i32) -> Option<String> {
        let line = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        Some(String::from_utf8_lossy(&line).replace('\0', " "))
    }

    /// User and system time, in the kernel's clock ticks, a hundred a second.
    pub fn cpu_ticks(pid: i32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Fields after the parenthesised name, which can itself hold spaces.
        let after = stat.rsplit(')').next().unwrap_or("");
        let fields: Vec<&str> = after.split_whitespace().collect();
        Some(
            fields
                .get(11)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                + fields
                    .get(12)
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0),
        )
    }

    /// Chromium's seccomp filter is on the process itself.
    pub fn renderer_sandbox(pid: i32, _data: &Path) -> Result<(), String> {
        let status =
            std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|e| e.to_string())?;
        if status.lines().any(|l| l == "Seccomp:\t2") {
            Ok(())
        } else {
            Err(status)
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::{
        ffi::{CString, c_char, c_int, c_void},
        os::unix::ffi::OsStrExt,
        path::Path,
    };

    // libproc's descriptor information (sys/proc_info.h), which the libc crate doesn't carry.
    const PROC_PIDFDVNODEPATHINFO: c_int = 2;
    const PROC_PIDFDPIPEINFO: c_int = 6;
    const PROX_FDTYPE_VNODE: u32 = 1;
    const PROX_FDTYPE_PIPE: u32 = 6;

    #[repr(C)]
    struct ProcFileInfo {
        open_flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guard_flags: u32,
    }

    #[repr(C)]
    struct PipeFdInfo {
        file: ProcFileInfo,
        stat: libc::vinfo_stat,
        handle: u64,
        peer: u64,
        status: c_int,
        reserved: c_int,
    }

    #[repr(C)]
    struct VnodeFdInfoWithPath {
        file: ProcFileInfo,
        path: libc::vnode_info_path,
    }

    fn fd_type(pid: i32, fd: i32) -> Option<u32> {
        let mut fds = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0
            };
            4096
        ];
        let size = std::mem::size_of_val(fds.as_slice()) as c_int;
        // SAFETY: the buffer is the size given.
        let filled = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size)
        };
        let count = usize::try_from(filled).ok()? / std::mem::size_of::<libc::proc_fdinfo>();
        fds[..count]
            .iter()
            .find(|f| f.proc_fd == fd)
            .map(|f| f.proc_fdtype)
    }

    fn info<T>(pid: i32, fd: i32, flavor: c_int) -> Option<T> {
        // SAFETY: T is plain data, filled in whole or not at all.
        let mut info: T = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<T>() as c_int;
        // SAFETY: the buffer is the size given.
        let filled = unsafe {
            libc::proc_pidfdinfo(pid, fd, flavor, (&raw mut info).cast::<c_void>(), size)
        };
        (filled == size).then_some(info)
    }

    pub fn open_file(pid: i32, fd: i32) -> Option<String> {
        match fd_type(pid, fd)? {
            PROX_FDTYPE_PIPE => {
                let pipe: PipeFdInfo = info(pid, fd, PROC_PIDFDPIPEINFO)?;
                Some(format!("pipe {:x}", pipe.handle))
            }
            PROX_FDTYPE_VNODE => {
                let vnode: VnodeFdInfoWithPath = info(pid, fd, PROC_PIDFDVNODEPATHINFO)?;
                let stat = &vnode.path.vip_vi.vi_stat;
                Some(format!("file {} {}", stat.vst_dev, stat.vst_ino))
            }
            other => Some(format!("descriptor of type {other}")),
        }
    }

    /// User and system time, which libproc gives in Mach time units.
    pub fn cpu_ticks(pid: i32) -> Option<u64> {
        // SAFETY: proc_taskinfo is plain data, filled in whole or not at all.
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_taskinfo>() as c_int;
        // SAFETY: the buffer is the size given.
        let filled = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&raw mut info).cast(), size)
        };
        if filled != size {
            return None;
        }
        let mut base = Timebase { numer: 0, denom: 0 };
        // SAFETY: it fills the struct.
        unsafe { mach_timebase_info(&mut base) };
        let units = u128::from(info.pti_total_user + info.pti_total_system);
        let nanoseconds = units * u128::from(base.numer) / u128::from(base.denom.max(1));
        Some((nanoseconds / 10_000_000) as u64)
    }

    pub fn command_line(pid: i32) -> Option<String> {
        let out = std::process::Command::new("ps")
            .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// How Mach time units convert to nanoseconds.
    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> c_int;
        fn sandbox_check(pid: libc::pid_t, operation: *const c_char, kind: c_int, ...) -> c_int;
    }

    /// The renderer is under Seatbelt, and its policy keeps it out of the profiles' data.
    pub fn renderer_sandbox(pid: i32, data: &Path) -> Result<(), String> {
        let path = CString::new(data.as_os_str().as_bytes()).unwrap();
        // SAFETY: a null operation asks only whether the process is sandboxed; a path filter takes
        // one NUL-terminated path.
        let (sandboxed, denied) = unsafe {
            (
                sandbox_check(pid, std::ptr::null(), 0),
                sandbox_check(pid, c"file-read-data".as_ptr(), 1, path.as_ptr()),
            )
        };
        // The system calls a process that doesn't exist sandboxed, so it has to be there still.
        if command_line(pid).is_none() {
            return Err("it has gone".into());
        }
        if sandboxed == 1 && denied == 1 {
            Ok(())
        } else {
            Err(format!(
                "sandboxed {sandboxed}, reading the data denied {denied}"
            ))
        }
    }
}
