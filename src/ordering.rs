//! Orders control requests after earlier input. A request carrying `after_rfb_input` first waits
//! until that many KeyEvent and PointerEvent messages of its viewer's connection were processed.
//! Once due, it waits for its editor check: the editor's renderer confirms that the element the
//! request names still has focus. A tab has at most one check running or request dispatching,
//! and holds its RFB input meanwhile: the next request's check starts only after the previous
//! request reached the page and was answered, so it also sees any focus change the previous one
//! caused. Requests are kept whole, whatever their method, and handled again at each stage.
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

/// Requests waiting for input or a check, across the host.
pub const LIMIT: usize = 64;
/// A check unanswered for this long is refused: its frame or renderer has gone away.
pub const TIMEOUT: Duration = Duration::from_secs(2);

/// A check sent to a frame's renderer.
struct Sent {
    token: u64,
    frame: String,
    at: Instant,
}
enum Stage {
    /// Waiting for this many RFB input messages.
    Input(u64),
    /// Waiting for its turn on the tab (nothing sent yet), then for the renderer's answer.
    Check(Option<Sent>),
}
struct Entry {
    tab: u64,
    viewer: u64,
    stage: Stage,
    request: Value,
}
#[derive(Default)]
pub struct Held {
    /// RFB input processed per (tab, viewer) connection.
    counts: HashMap<(u64, u64), u64>,
    /// In arrival order; checked entries in the order they became due.
    entries: Vec<Entry>,
    next_token: u64,
    /// Tabs whose checked request is on its way to the page and not yet answered.
    dispatching: HashSet<u64>,
}
fn target(request: &Value) -> (u64, u64, u64) {
    let p = &request["params"];
    let number = |key: &str| p[key].as_u64().unwrap_or(0);
    (number("tab"), number("viewer"), number("after_rfb_input"))
}
/// The renderer's answer for a request that passed its check stage: whether it confirmed the
/// editor. None for a request not yet checked.
pub fn checked(request: &Value) -> Option<bool> {
    request.get("checked").and_then(Value::as_bool)
}
impl Held {
    /// Counts one RFB input message and returns the requests it makes due, in arrival order.
    pub fn input(&mut self, tab: u64, viewer: u64) -> Vec<Value> {
        let count = self.counts.entry((tab, viewer)).or_default();
        *count += 1;
        let count = *count;
        self.take(|e| {
            e.tab == tab
                && e.viewer == viewer
                && matches!(e.stage, Stage::Input(after) if after <= count)
        })
    }
    /// Whether the tab has a request checking or dispatching; its RFB input then waits.
    pub fn busy(&self, tab: u64) -> bool {
        self.dispatching.contains(&tab)
            || self
                .entries
                .iter()
                .any(|e| e.tab == tab && matches!(e.stage, Stage::Check(_)))
    }
    fn full(&self) -> Result<(), String> {
        if self.entries.len() >= LIMIT {
            return Err("too many requests waiting for earlier input".into());
        }
        Ok(())
    }
    /// Holds a validated request until its input count is reached. False when it is due now.
    pub fn hold(&mut self, request: &Value) -> Result<bool, String> {
        let (tab, viewer, after) = target(request);
        if after <= self.counts.get(&(tab, viewer)).copied().unwrap_or(0) {
            return Ok(false);
        }
        self.full()?;
        self.entries.push(Entry {
            tab,
            viewer,
            stage: Stage::Input(after),
            request: request.clone(),
        });
        Ok(true)
    }
    /// Keeps a due request until its turn on the tab and then its check.
    pub fn check(&mut self, request: &Value) -> Result<(), String> {
        self.full()?;
        let (tab, viewer, _) = target(request);
        self.entries.push(Entry {
            tab,
            viewer,
            stage: Stage::Check(None),
            request: request.clone(),
        });
        Ok(())
    }
    /// Runs every transition that does not wait for a new request, in order: checks unanswered
    /// for [`TIMEOUT`] at `now` expire, the renderers' answers (token, frame, confirmed) complete
    /// theirs, and each idle tab's next check starts. Expiry comes first, so an answer later than
    /// the timeout never counts. Returns the requests now ready, marked [`checked`], and those
    /// refused: expired or impossible to check.
    pub fn tick(
        &mut self,
        now: Instant,
        answers: impl IntoIterator<Item = (u64, String, bool)>,
        send: impl FnMut(&Value, u64) -> Option<String>,
    ) -> (Vec<Value>, Vec<Value>) {
        let mut refused = self.expired(now);
        let ready = self.completed(answers, now);
        refused.extend(self.start(now, send));
        (ready, refused)
    }
    /// Starts the check of each idle tab's next request with `send(request, token)`, which
    /// returns the frame it asked, or None when the editor or its frame is gone. Returns the
    /// requests that could not be checked, for refusal.
    pub fn start(
        &mut self,
        now: Instant,
        mut send: impl FnMut(&Value, u64) -> Option<String>,
    ) -> Vec<Value> {
        let mut refused = Vec::new();
        loop {
            let mut seen = HashSet::new();
            let mut gone = None;
            for (i, e) in self.entries.iter_mut().enumerate() {
                let Stage::Check(sent) = &mut e.stage else {
                    continue;
                };
                if self.dispatching.contains(&e.tab) || !seen.insert(e.tab) || sent.is_some() {
                    continue;
                }
                self.next_token += 1;
                let token = self.next_token;
                match send(&e.request, token) {
                    Some(frame) => {
                        *sent = Some(Sent {
                            token,
                            frame,
                            at: now,
                        })
                    }
                    None => {
                        gone = Some(i);
                        break;
                    }
                }
            }
            // The tab's next request gets its turn on the next pass.
            match gone {
                Some(i) => refused.push(self.entries.remove(i).request),
                None => return refused,
            }
        }
    }
    /// Records the renderers' answers and returns the requests now ready, marked [`checked`];
    /// each tab then dispatches until [`Held::finished`]. An answer from any frame but the one
    /// asked, or after the timeout, is ignored.
    fn completed(
        &mut self,
        answers: impl IntoIterator<Item = (u64, String, bool)>,
        now: Instant,
    ) -> Vec<Value> {
        let mut ready = Vec::new();
        for (token, frame, confirmed) in answers {
            let Some(i) = self.entries.iter().position(|e| {
                matches!(&e.stage, Stage::Check(Some(s))
                    if s.token == token && s.frame == frame && now.duration_since(s.at) < TIMEOUT)
            }) else {
                continue;
            };
            let e = self.entries.remove(i);
            self.dispatching.insert(e.tab);
            let mut request = e.request;
            request["checked"] = confirmed.into();
            ready.push(request);
        }
        ready
    }
    /// Checks unanswered for [`TIMEOUT`] at `now`, for refusal.
    fn expired(&mut self, now: Instant) -> Vec<Value> {
        self.take(
            |e| matches!(&e.stage, Stage::Check(Some(s)) if now.duration_since(s.at) >= TIMEOUT),
        )
    }
    /// The tab's dispatching request was applied or refused and answered; the next may start.
    pub fn finished(&mut self, tab: u64) {
        self.dispatching.remove(&tab);
    }
    /// A connection closed: its count restarts and its requests are returned for refusal.
    pub fn disconnected(&mut self, tab: u64, viewer: u64) -> Vec<Value> {
        self.counts.remove(&(tab, viewer));
        self.take(|e| e.tab == tab && e.viewer == viewer)
    }
    /// CONTROL changed on the tab: its requests are returned for refusal.
    pub fn controller_changed(&mut self, tab: u64) -> Vec<Value> {
        self.take(|e| e.tab == tab)
    }
    /// The tab closed: its counts go and its requests are returned for refusal.
    pub fn closed(&mut self, tab: u64) -> Vec<Value> {
        self.counts.retain(|(t, _), _| *t != tab);
        self.controller_changed(tab)
    }
    /// Every request, for refusal at shutdown.
    pub fn all(&mut self) -> Vec<Value> {
        self.take(|_| true)
    }
    fn take(&mut self, matches: impl Fn(&Entry) -> bool) -> Vec<Value> {
        let (taken, kept) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|e| matches(e));
        self.entries = kept;
        taken.into_iter().map(|e: Entry| e.request).collect()
    }
}

/// RFB input one connection may have waiting behind checks. A check lasts at most [`TIMEOUT`]:
/// 2 s of pointer motion at 120 Hz is 240 messages, and a person's keys far fewer, so a viewer
/// sending more is flooding the tab.
pub const BLOCKED_PER_CONNECTION: usize = 256;
/// Waiting RFB input across the host: a few busy viewers' worth, a few hundred KiB at most.
pub const BLOCKED: usize = 1024;
/// RFB input of tabs with a request checking or dispatching, in arrival order per tab.
#[derive(Default)]
pub struct Blocked {
    tabs: HashMap<u64, VecDeque<Value>>,
    counts: HashMap<(u64, u64), usize>,
    total: usize,
}
fn connection(request: &Value) -> (u64, u64) {
    let (tab, viewer, _) = target(request);
    (tab, viewer)
}
impl Blocked {
    /// Whether the tab has input waiting; its later input must queue behind it.
    pub fn waiting(&self, tab: u64) -> bool {
        self.tabs.contains_key(&tab)
    }
    /// Queues an input message. False when its connection or the host is at its limit: the
    /// message is dropped and the caller closes the connection.
    pub fn push(&mut self, request: Value) -> bool {
        let (tab, viewer) = connection(&request);
        let count = self.counts.entry((tab, viewer)).or_default();
        if *count >= BLOCKED_PER_CONNECTION || self.total >= BLOCKED {
            return false;
        }
        *count += 1;
        self.total += 1;
        self.tabs.entry(tab).or_default().push_back(request);
        true
    }
    /// The oldest waiting message of a tab no longer `busy`.
    pub fn next(&mut self, busy: impl Fn(u64) -> bool) -> Option<Value> {
        let tab = *self.tabs.keys().find(|tab| !busy(**tab))?;
        let queue = self.tabs.get_mut(&tab)?;
        let request = queue.pop_front()?;
        if queue.is_empty() {
            self.tabs.remove(&tab);
        }
        self.forget(&request);
        Some(request)
    }
    fn forget(&mut self, request: &Value) {
        let key = connection(request);
        if let Some(count) = self.counts.get_mut(&key) {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&key);
            }
        }
        self.total -= 1;
    }
    /// A connection closed: its waiting input goes.
    pub fn disconnected(&mut self, tab: u64, viewer: u64) {
        self.drop_where(tab, |r| connection(r).1 == viewer);
    }
    /// The tab closed: its waiting input goes.
    pub fn closed(&mut self, tab: u64) {
        self.drop_where(tab, |_| true);
    }
    fn drop_where(&mut self, tab: u64, matches: impl Fn(&Value) -> bool) {
        let Some(queue) = self.tabs.remove(&tab) else {
            return;
        };
        let (dropped, kept): (VecDeque<_>, VecDeque<_>) =
            queue.into_iter().partition(|r| matches(r));
        for request in &dropped {
            self.forget(request);
        }
        if !kept.is_empty() {
            self.tabs.insert(tab, kept);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn request(id: u64, tab: u64, viewer: u64, after: u64) -> Value {
        json!({"id":id,"method":"ime.input","params":{"tab":tab,"viewer":viewer,"after_rfb_input":after}})
    }
    fn ids(requests: Vec<Value>) -> Vec<u64> {
        requests.iter().map(|r| r["id"].as_u64().unwrap()).collect()
    }
    #[test]
    fn requests_wait_for_their_connections_input_in_order() {
        let mut held = Held::default();
        assert_eq!(held.hold(&request(1, 1, 1, 0)), Ok(false));
        assert_eq!(held.hold(&request(2, 1, 1, 2)), Ok(true));
        assert_eq!(held.hold(&request(3, 1, 1, 2)), Ok(true));
        assert_eq!(held.hold(&request(4, 1, 2, 1)), Ok(true));
        assert_eq!(held.hold(&request(5, 2, 1, 1)), Ok(true));
        assert!(held.input(1, 1).is_empty());
        // Another viewer's and another tab's input count separately.
        assert_eq!(ids(held.input(1, 2)), [4]);
        assert_eq!(ids(held.input(1, 1)), [2, 3]);
        assert!(held.input(1, 1).is_empty());
        assert_eq!(held.hold(&request(6, 1, 1, 3)), Ok(false));
        assert_eq!(ids(held.input(2, 1)), [5]);
        // Waiting for input does not hold the tab's input.
        assert!(!held.busy(1) && !held.busy(2));
    }
    #[test]
    fn held_requests_are_bounded_host_wide() {
        let mut held = Held::default();
        for i in 0..LIMIT as u64 {
            assert_eq!(held.hold(&request(i, i % 3, 1, 1)), Ok(true));
        }
        assert!(held.hold(&request(99, 9, 1, 1)).is_err());
        // A due request is never held, so the limit does not refuse it.
        held.input(9, 1);
        assert_eq!(held.hold(&request(99, 9, 1, 1)), Ok(false));
        assert_eq!(held.all().len(), LIMIT);
        assert_eq!(held.hold(&request(100, 9, 1, 2)), Ok(true));
        // Requests waiting for a check count too.
        for i in 1..LIMIT as u64 {
            assert_eq!(held.check(&request(i, 1, 1, 0)), Ok(()));
        }
        assert!(held.check(&request(0, 1, 1, 0)).is_err());
        assert!(held.hold(&request(0, 1, 1, 9)).is_err());
    }
    /// Answers no check and sends every check to `frame<id>`, except request 4's.
    fn send(r: &Value, _token: u64) -> Option<String> {
        let id = r["id"].as_u64().unwrap();
        (id != 4).then(|| format!("frame{id}"))
    }
    #[test]
    fn one_check_or_dispatch_per_tab_at_a_time() {
        let mut held = Held::default();
        let now = Instant::now();
        let sent = std::cell::RefCell::new(Vec::new());
        let record = |r: &Value, token| {
            sent.borrow_mut().push((r["id"].as_u64().unwrap(), token));
            send(r, token)
        };
        for (id, tab) in [(1, 1), (2, 2), (3, 1), (4, 3), (5, 3)] {
            held.check(&request(id, tab, 1, 0)).unwrap();
        }
        assert!(held.busy(1) && held.busy(3) && !held.busy(4));
        // A request that cannot be checked is refused and its tab's next request starts.
        let (ready, refused) = held.tick(now, [], record);
        assert!(ready.is_empty());
        assert_eq!(ids(refused), [4]);
        let token = |id| sent.borrow().iter().find(|s| s.0 == id).unwrap().1;
        assert_eq!(
            sent.borrow().iter().map(|s| s.0).collect::<Vec<_>>(),
            [1, 2, 4, 5]
        );
        // An answer from another frame, or for an unknown token, is ignored.
        let (ready, _) = held.tick(now, [(token(1), "frame2".into(), true)], send);
        assert!(ready.is_empty());
        let (ready, _) = held.tick(now, [(99, "frame1".into(), true)], send);
        assert!(ready.is_empty());
        let answers = [
            (token(1), "frame1".into(), true),
            (token(2), "frame2".into(), false),
            (token(5), "frame5".into(), true),
        ];
        let (ready, _) = held.tick(now, answers, record);
        assert_eq!(ids(ready.clone()), [1, 2, 5]);
        assert_eq!(
            ready.iter().map(checked).collect::<Vec<_>>(),
            [Some(true), Some(false), Some(true)]
        );
        assert_eq!(checked(&request(1, 1, 1, 0)), None);
        // Request 3 waits for request 1 to be answered, not merely checked.
        assert_eq!(sent.borrow().len(), 4);
        assert!(held.busy(1));
        held.finished(1);
        held.tick(now, [], record);
        assert_eq!(sent.borrow().last().unwrap().0, 3);
        let (ready, _) = held.tick(now, [(token(3), "frame3".into(), true)], send);
        assert_eq!(ids(ready), [3]);
        for tab in 1..=3 {
            held.finished(tab);
            assert!(!held.busy(tab));
        }
        // Refusal rules apply while checking; a late answer finds nothing.
        held.check(&request(6, 1, 1, 0)).unwrap();
        held.check(&request(7, 1, 1, 0)).unwrap();
        held.tick(now, [], record);
        assert_eq!(ids(held.closed(1)), [6, 7]);
        assert!(!held.busy(1));
        let (ready, _) = held.tick(now, [(token(6), "frame6".into(), true)], send);
        assert!(ready.is_empty());
    }
    #[test]
    fn an_expired_check_starts_the_next_without_a_new_request() {
        let mut held = Held::default();
        let now = Instant::now();
        // Two requests queued on a tab whose renderer never answers.
        held.check(&request(1, 1, 1, 0)).unwrap();
        held.check(&request(2, 1, 1, 0)).unwrap();
        let (_, refused) = held.tick(now, [], send);
        assert!(refused.is_empty());
        let (_, refused) = held.tick(now + TIMEOUT / 2, [], send);
        assert!(refused.is_empty());
        // The first expires and, in the same tick, the second's check is sent.
        let (_, refused) = held.tick(now + TIMEOUT, [], send);
        assert_eq!(ids(refused), [1]);
        assert!(held.busy(1));
        let (_, refused) = held.tick(now + TIMEOUT * 2, [], send);
        assert_eq!(ids(refused), [2]);
        assert!(!held.busy(1));
    }
    #[test]
    fn an_answer_after_the_timeout_never_counts() {
        let mut held = Held::default();
        let now = Instant::now();
        let token = std::cell::Cell::new(0);
        held.check(&request(1, 1, 1, 0)).unwrap();
        held.tick(now, [], |r, t| {
            token.set(t);
            send(r, t)
        });
        // The answer arrives with the tick that finds the check expired.
        let (ready, refused) =
            held.tick(now + TIMEOUT, [(token.get(), "frame1".into(), true)], send);
        assert!(ready.is_empty());
        assert_eq!(ids(refused), [1]);
        assert!(!held.busy(1));
    }
    #[test]
    fn blocked_input_is_bounded_per_connection_and_host() {
        let input = |tab: u64, viewer: u64, n: u64| json!({"method":"internal.key","params":{"tab":tab,"viewer":viewer,"key":n}});
        let mut blocked = Blocked::default();
        for n in 0..BLOCKED_PER_CONNECTION as u64 {
            assert!(blocked.push(input(1, 1, n)));
        }
        // That connection is full; another on the same tab is not.
        assert!(!blocked.push(input(1, 1, 999)));
        assert!(blocked.push(input(1, 2, 0)));
        // Busy tabs keep their input; an idle tab's comes out oldest first.
        assert_eq!(blocked.next(|_| true), None);
        let first = blocked.next(|_| false).unwrap();
        assert_eq!(first["params"]["key"], 0);
        assert!(blocked.push(input(1, 1, 1000)));
        assert!(!blocked.push(input(1, 1, 1001)));
        // A closed connection's input goes and frees its share.
        blocked.disconnected(1, 1);
        assert!(blocked.waiting(1));
        assert_eq!(blocked.next(|_| false).unwrap()["params"]["viewer"], 2);
        assert!(!blocked.waiting(1));
        // The host-wide limit holds across connections and tabs.
        for n in 0..BLOCKED as u64 {
            let (tab, viewer) = (n % 8, n / 8 % 8);
            assert!(blocked.push(input(tab, viewer, n)), "{n}");
        }
        assert!(!blocked.push(input(9, 9, 0)));
        blocked.closed(0);
        assert!(!blocked.waiting(0));
        assert!(blocked.push(input(9, 9, 0)));
        assert_eq!(blocked.total, BLOCKED - BLOCKED / 8 + 1);
    }
    #[test]
    fn disconnect_control_change_and_close_refuse() {
        let mut held = Held::default();
        held.input(1, 1);
        for (id, tab, viewer) in [(1, 1, 1), (2, 1, 2), (3, 2, 1), (4, 3, 1)] {
            assert_eq!(held.hold(&request(id, tab, viewer, 5)), Ok(true));
        }
        assert_eq!(ids(held.disconnected(1, 1)), [1]);
        // The count restarts with the connection.
        assert_eq!(held.hold(&request(5, 1, 1, 1)), Ok(true));
        assert_eq!(ids(held.controller_changed(1)), [2, 5]);
        held.input(2, 1);
        assert_eq!(ids(held.closed(2)), [3]);
        assert_eq!(held.hold(&request(6, 2, 1, 1)), Ok(true));
        assert_eq!(ids(held.all()), [4, 6]);
        assert!(held.all().is_empty());
    }
}
