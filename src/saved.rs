//! Saved tabs, so a restarted host resumes them (specs/0002-control.md, Restarts). Each tab is one
//! file the engine described, replaced whole, so a crash leaves its old version or its new one,
//! never half of either.
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;

/// How the previous host on this data directory ended, as Status reports it.
pub const FIRST_RUN: i32 = 0;
pub const CLEAN: i32 = 1;
pub const CRASHED: i32 = 2;

/// A saved tab file larger than this isn't one this host wrote.
const LARGEST: u64 = 64 << 20;
/// Tab IDs cross into the engine as 32-bit integers, and the next one must fit too.
const LAST_ID: u64 = i32::MAX as u64 - 1;

/// What a starting host finds.
pub struct Saved {
    pub previous_exit: i32,
    /// The lowest tab ID no host on this data directory has handed out.
    pub next_tab: u64,
    /// Every readable saved tab's ID and context, by ID; `load` reads the rest when it is needed, so
    /// however many there are, startup holds little.
    pub tabs: Vec<Value>,
}

/// Writes not yet on disk: the latest of each tab, its state to save or `None` to forget it. A
/// tab that changes faster than the disk keeps up costs one write, of its latest state.
struct Pending {
    writes: BTreeMap<u64, Option<Vec<u8>>>,
    /// What waits for the forgets queued before it to be on disk.
    after: Vec<Box<dyn FnOnce() + Send>>,
    stopping: bool,
    failed: bool,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    writes: BTreeMap::new(),
    after: Vec::new(),
    stopping: false,
    failed: false,
});
static WAKE: Condvar = Condvar::new();
static WRITER: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

fn tabs_dir(root: &Path) -> PathBuf {
    root.join("tabs")
}

fn not_found(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn sync(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Replaces a file whole: written aside, synced, renamed over it, and the rename synced.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let aside = path.with_extension("tmp");
    not_found(fs::remove_file(&aside))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&aside)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&aside, path)?;
    sync(path.parent().unwrap_or(Path::new("/")))
}

/// Holds the data directory for this process: two hosts on one would hand out the same tab IDs and
/// write over each other's tabs. A replacement starts once its predecessor has exited.
pub fn lock(root: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join("lock"))?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("lock is not a file"));
    }
    // SAFETY: the descriptor is open for the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(if e.kind() == io::ErrorKind::WouldBlock {
            io::Error::other("another host is using this data directory")
        } else {
            e
        });
    }
    // Held until the process ends, however it ends.
    std::mem::forget(file);
    Ok(())
}

fn is_hex32(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether a saved context key names a profile this host could have made.
fn valid_context(context: &str) -> bool {
    if let Some(id) = context.strip_prefix("unnamed-") {
        return is_hex32(id);
    }
    context
        .strip_prefix("profile-")
        .is_some_and(|name| crate::identifier(&Value::from(name)).is_ok())
}

/// A saved tab worth restoring: the engine's own description, for the ID its file is named after.
/// The engine checks each entry's details itself, since Chromium trusts them.
fn valid(tab: &Value, id: u64) -> bool {
    tab["tab"].as_u64() == Some(id)
        && (1..=LAST_ID).contains(&id)
        && tab["context"].as_str().is_some_and(valid_context)
        && tab["width"].is_u64()
        && tab["height"].is_u64()
        && crate::dimensions(tab).is_ok()
        && tab["entries"].as_array().is_some_and(|entries| {
            entries.len() <= 100
                && entries
                    .iter()
                    .all(|e| e["url"].as_str().is_some_and(crate::cdp::navigable))
        })
}

/// The ID a directory entry is the saved tab of: one named `ID.json` exactly.
fn id_of(entry: &fs::DirEntry) -> Option<u64> {
    let name = entry.file_name().into_string().ok()?;
    let id = name.strip_suffix(".json")?.parse::<u64>().ok()?;
    (name == format!("{id}.json")).then_some(id)
}

/// One of this host's small records, or `None` if it doesn't exist. Anything but a short regular
/// file is an error, read without following a link or waiting on a pipe.
fn read_small(path: &Path) -> io::Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!("{path:?} is not a file")));
    }
    let mut text = String::new();
    file.take(64).read_to_string(&mut text)?;
    if text.len() == 64 {
        return Err(io::Error::other(format!("{path:?} is too long")));
    }
    Ok(Some(text))
}

/// A saved tab: a regular file this host could have written, or `None` for anything else. A file
/// it can't read is an error, not a tab to discard: it may be fine once whatever is wrong with the
/// disk is put right.
fn read(path: &Path, id: u64) -> io::Result<Option<Value>> {
    // Not following a symbolic link, nor waiting on a pipe.
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !metadata.is_file() || metadata.len() > LARGEST {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?
        .take(LARGEST)
        .read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)
        .ok()
        .filter(|tab| valid(tab, id)))
}

/// A saved tab's whole state, read when it is restored rather than held from startup.
pub fn load(root: &Path, id: u64) -> io::Result<Option<Value>> {
    read(&tabs_dir(root).join(format!("{id}.json")), id)
}

/// Removes what startup can't use, saying so without its contents, which hold what a page held. A
/// host that can't remove it still starts.
fn discard(entry: &fs::DirEntry) {
    let name = entry.file_name();
    if !name.to_string_lossy().ends_with(".tmp") {
        eprintln!("lanescape-browser: discarding saved tab {name:?}, which can't be restored");
    }
    let removed = match entry.file_type() {
        Ok(kind) if kind.is_dir() => fs::remove_dir_all(entry.path()),
        _ => fs::remove_file(entry.path()),
    };
    if let Err(e) = removed {
        eprintln!("lanescape-browser: can't remove {name:?}: {e}");
    }
}

/// Reads what the previous host left, before Chromium starts, and marks this one running. Removes
/// saved tabs it can't restore and unnamed profiles no saved tab uses.
pub fn open(root: &Path) -> io::Result<Saved> {
    // What is there, looked at before anything is written: only a missing name is absent, and
    // `tabs` must be a real directory, or startup stops with nothing changed.
    let dir = tabs_dir(root);
    let present = |path: &Path| match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    };
    let have_tabs = match present(&dir)? {
        Some(metadata) if metadata.is_dir() => true,
        Some(_) => return Err(io::Error::other("tabs is not a directory")),
        None => false,
    };
    let have_ledger = present(&root.join("next-tab"))?.is_some();
    // Restart state without the marker of how its host ended is no first run.
    let previous_exit = match read_small(&root.join("session"))? {
        Some(state) if state == "stopped" => CLEAN,
        Some(_) => CRASHED,
        None if have_tabs || have_ledger => CRASHED,
        None => FIRST_RUN,
    };
    // Only this file knows the IDs of tabs that have closed: without it, they could be reused. It
    // exists from the first run on, before anything says a host has run.
    let first_run = previous_exit == FIRST_RUN && !have_tabs;
    let recorded = match read_small(&root.join("next-tab"))? {
        // Exactly as written: digits alone.
        Some(n) => n
            .parse::<u64>()
            .ok()
            .filter(|id| *n == id.to_string() && (1..=LAST_ID + 1).contains(id))
            .ok_or_else(|| io::Error::other("next-tab is unreadable"))?,
        None if first_run => {
            write_atomic(&root.join("next-tab"), b"1")?;
            1
        }
        None => return Err(io::Error::other("next-tab is missing")),
    };
    write_atomic(&root.join("session"), b"running")?;
    if !have_tabs {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
    }
    let mut tabs = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let tab = match id_of(&entry) {
            Some(id) => read(&entry.path(), id)?,
            None => None,
        };
        match tab {
            Some(tab) => tabs.push(serde_json::json!({"tab":tab["tab"],"context":tab["context"]})),
            None => discard(&entry),
        }
    }
    tabs.sort_by_key(|t| t["tab"].as_u64());
    // A tab saved before its ID was recorded, by a host that then crashed, still has it.
    let next_tab = tabs.last().map_or(recorded, |t| {
        recorded.max(t["tab"].as_u64().unwrap_or(0) + 1)
    });
    if next_tab > recorded {
        write_atomic(&root.join("next-tab"), next_tab.to_string().as_bytes())?;
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.strip_prefix("unnamed-").is_some_and(is_hex32)
            && entry.file_type()?.is_dir()
            && !tabs.iter().any(|t| t["context"] == name)
            && let Err(e) = fs::remove_dir_all(entry.path())
        {
            eprintln!("lanescape-browser: can't remove {name:?}: {e}");
        }
    }
    Ok(Saved {
        previous_exit,
        next_tab,
        tabs,
    })
}

/// Makes sure no later host hands out `tab` again. Runs before anyone hears of the tab, and a tab
/// whose ID can't be recorded must not be heard of. `reserved` is the `next-tab` last written.
pub fn reserve(root: &Path, tab: u64, reserved: &mut u64) -> io::Result<()> {
    if tab < *reserved {
        return Ok(());
    }
    if tab > LAST_ID {
        return Err(io::Error::other("tab IDs are used up"));
    }
    write_atomic(&root.join("next-tab"), (tab + 1).to_string().as_bytes())
        .inspect_err(|_| failed())?;
    *reserved = tab + 1;
    Ok(())
}

/// A write that didn't happen: this run can't end CLEAN.
fn failed() {
    PENDING.lock().unwrap().failed = true;
}

/// Removes saved tabs now, durably, before anything that depends on their being gone.
pub fn forget_now(root: &Path, tabs: impl IntoIterator<Item = u64>) -> io::Result<()> {
    let dir = tabs_dir(root);
    let removed = tabs
        .into_iter()
        .try_for_each(|tab| not_found(fs::remove_file(dir.join(format!("{tab}.json")))))
        .and_then(|()| sync(&dir));
    removed.inspect_err(|_| failed())
}

/// Queues a tab's latest state. One too large to read back is saved without its page state, as the
/// URLs of its history, and failing that as its current entry alone, so it still comes back.
pub fn save(tab: u64, mut state: Value) {
    let mut bytes = state.to_string().into_bytes();
    if bytes.len() as u64 > LARGEST {
        for entry in state["entries"].as_array_mut().into_iter().flatten() {
            entry.as_object_mut().map(|e| e.remove("page_state"));
        }
        bytes = state.to_string().into_bytes();
    }
    if bytes.len() as u64 > LARGEST {
        let index = state["index"].as_u64().unwrap_or(0) as usize;
        let current = state["entries"].get(index).cloned();
        state["entries"] = Value::Array(current.into_iter().collect());
        state["index"] = 0.into();
        bytes = state.to_string().into_bytes();
    }
    PENDING.lock().unwrap().writes.insert(tab, Some(bytes));
    WAKE.notify_one();
}

/// Runs `then` once every forget queued so far is on disk, as removing what a forgotten tab used
/// must: a crash before then would bring the tab back without it. After a failed write it never
/// runs; the next host decides with what survived.
pub fn after_forgets(then: impl FnOnce() + Send + 'static) {
    PENDING.lock().unwrap().after.push(Box::new(then));
    WAKE.notify_one();
}

pub fn forget(tab: u64) {
    PENDING.lock().unwrap().writes.insert(tab, None);
    WAKE.notify_one();
}

/// Starts the thread that writes saved tabs, so neither Chromium's UI thread nor the control loop
/// waits on the disk.
pub fn start(root: PathBuf) {
    let handle = std::thread::spawn(move || {
        let dir = tabs_dir(&root);
        loop {
            let (writes, after) = {
                let mut pending = PENDING.lock().unwrap();
                loop {
                    if !pending.writes.is_empty() || !pending.after.is_empty() {
                        break (
                            std::mem::take(&mut pending.writes),
                            std::mem::take(&mut pending.after),
                        );
                    }
                    if pending.stopping {
                        return;
                    }
                    pending = WAKE.wait(pending).unwrap();
                }
            };
            let mut failed = false;
            let mut forgot = false;
            for (tab, write) in writes {
                let path = dir.join(format!("{tab}.json"));
                let result = match write {
                    Some(state) => write_atomic(&path, &state),
                    None => {
                        forgot = true;
                        not_found(fs::remove_file(path))
                    }
                };
                if let Err(e) = result {
                    eprintln!("lanescape-browser: can't save tab {tab}: {e}");
                    failed = true;
                }
            }
            if forgot && let Err(e) = sync(&dir) {
                eprintln!("lanescape-browser: can't save tabs: {e}");
                failed = true;
            }
            let failed = {
                let mut pending = PENDING.lock().unwrap();
                pending.failed |= failed;
                pending.failed
            };
            if !failed {
                after.into_iter().for_each(|then| then());
            }
        }
    });
    *WRITER.lock().unwrap() = Some(handle);
}

/// Waits for every queued write, then records a clean exit if this was one and every tab is as it
/// should be on disk.
pub fn finish(root: &Path, clean: bool) {
    PENDING.lock().unwrap().stopping = true;
    WAKE.notify_one();
    let writer = WRITER.lock().unwrap().take();
    let joined = writer.is_none_or(|handle| handle.join().is_ok());
    let saved = joined && !PENDING.lock().unwrap().failed;
    if clean
        && saved
        && let Err(e) = write_atomic(&root.join("session"), b"stopped")
    {
        eprintln!("lanescape-browser: can't record a clean exit: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("lanescape-saved-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn tab(id: u64, context: &str) -> Value {
        json!({"tab":id,"context":context,"width":400,"height":300,"index":0,"entries":[{"url":"https://example.com/"}]})
    }

    #[test]
    fn the_previous_exit_comes_from_the_session_file() {
        let root = root("session");
        assert_eq!(open(&root).unwrap().previous_exit, FIRST_RUN);
        assert_eq!(open(&root).unwrap().previous_exit, CRASHED);
        write_atomic(&root.join("session"), b"stopped").unwrap();
        assert_eq!(open(&root).unwrap().previous_exit, CLEAN);
        assert_eq!(fs::read_to_string(root.join("session")).unwrap(), "running");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_tabs_and_unused_unnamed_profiles_go() {
        let root = root("sweep");
        let used = format!("unnamed-{}", "a".repeat(32));
        let unused = format!("unnamed-{}", "b".repeat(32));
        let tabs = root.join("tabs");
        fs::create_dir_all(&tabs).unwrap();
        for dir in [&used, &unused, &"profile-6f6e65".into()] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let write = |name: &str, value: &Value| {
            fs::write(tabs.join(name), serde_json::to_vec(value).unwrap()).unwrap()
        };
        write("3.json", &tab(3, &used));
        write("4.json", &tab(4, "profile-one"));
        write("5.json", &tab(6, "profile-one"));
        write("007.json", &tab(7, "profile-one"));
        write("8.json", &tab(8, "profile-../x"));
        write("9.json", &tab(9, &format!("unnamed-{}", "A".repeat(32))));
        let mut file = tab(10, "profile-one");
        file["entries"][0]["url"] = json!("file:///etc/passwd");
        write("10.json", &file);
        write("2147483647.json", &tab(2147483647, "profile-one"));
        fs::write(tabs.join("11.json"), b"{not json").unwrap();
        fs::write(tabs.join("12.tmp"), b"half").unwrap();
        fs::create_dir(tabs.join("13.json")).unwrap();
        // Saved tabs without the record of IDs handed out aren't a first run.
        assert!(open(&root).is_err());
        fs::write(root.join("next-tab"), b"1").unwrap();
        let saved = open(&root).unwrap();
        let ids: Vec<_> = saved
            .tabs
            .iter()
            .map(|t| t["tab"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, [3, 4]);
        assert_eq!(saved.next_tab, 5);
        let mut left: Vec<_> = fs::read_dir(&tabs)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["3.json", "4.json"]);
        assert!(root.join(&used).is_dir());
        assert!(!root.join(&unused).exists());
        assert!(root.join("profile-6f6e65").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tab_ids_are_never_handed_out_twice() {
        let root = root("ids");
        let mut reserved = open(&root).unwrap().next_tab;
        assert_eq!(reserved, 1);
        reserve(&root, 1, &mut reserved).unwrap();
        reserve(&root, 2, &mut reserved).unwrap();
        assert_eq!(reserved, 3);
        assert_eq!(fs::read_to_string(root.join("next-tab")).unwrap(), "3");
        // No saved tab is left, yet the IDs handed out stay taken.
        assert_eq!(open(&root).unwrap().next_tab, 3);
        // A tab saved before a crash could record its ID is taken too, from then on.
        fs::write(
            root.join("tabs/9.json"),
            serde_json::to_vec(&tab(9, "profile-one")).unwrap(),
        )
        .unwrap();
        assert_eq!(open(&root).unwrap().next_tab, 10);
        assert_eq!(fs::read_to_string(root.join("next-tab")).unwrap(), "10");
        assert!(reserve(&root, LAST_ID + 1, &mut reserved).is_err());
        // Without the record, closed tabs' IDs could come round again.
        fs::write(root.join("next-tab"), b"lost").unwrap();
        assert!(open(&root).is_err());
        fs::write(root.join("next-tab"), b"1   then anything").unwrap();
        assert!(open(&root).is_err());
        fs::remove_file(root.join("next-tab")).unwrap();
        assert!(open(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn one_host_has_a_data_directory() {
        let root = root("lock");
        lock(&root).unwrap();
        assert!(
            lock(&root)
                .unwrap_err()
                .to_string()
                .contains("another host")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
