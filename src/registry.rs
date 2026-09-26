//! The image turns running in this process, one per session (docs/design.md, "Sessions" and
//! "Progress and cancellation").
//!
//! A call claims its session here before anything is spent, and the claim is refused when:
//!
//! - the server is shutting down (SERVER_SHUTTING_DOWN);
//! - another call holds the same name, compared case-insensitively as NTFS compares file names
//!   (SESSION_BUSY), so a `turn/start` never merges into a running turn;
//! - `--max-concurrent` turns are already running (TOO_MANY_RUNNING).
//!
//! Once the call has its Codex thread it attaches it, and from then on the app-server reader
//! routes that thread's notifications here, by `threadId`, into the call's own channel. The reader
//! only routes: sending on the channel never blocks, and whatever the event means is worked out on
//! the call's thread.
//!
//! A call that gives up on its turn before `turn/completed` (a cancel, or the budget running out)
//! *lingers*: it returns, but the session stays busy until that `turn/completed` arrives or the
//! child dies (docs/design.md, "After `turn/interrupt`"). The registry also makes sure the turn is
//! interrupted exactly once, whoever asks first and whenever its id becomes known: a cancel that
//! arrives before `turn/start` has answered is remembered, and the interrupt comes due the moment
//! the turn id turns up.
//!
//! The event type is the caller's; the registry only needs to know which turn an event names,
//! whether it ends the turn, and whether it must survive arriving before its thread is attached
//! ([`TurnEvent`]).

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::errors::{self, Failure};

/// What the registry needs to know about a routed notification.
pub trait TurnEvent: Send + 'static {
    /// The turn the notification names, if it names one (`item/*` carry `turnId`,
    /// `turn/started` and `turn/completed` carry `turn.id`).
    fn turn_id(&self) -> Option<&str>;
    /// Whether it ends the turn: `turn/completed`.
    fn ends_turn(&self) -> bool;
    /// Whether to hold on to it when no call has attached its thread yet, and hand it over when
    /// one does. Codex reports MCP server startups right after answering `thread/start`, so the
    /// reader can route one before the call has had the reply and attached its thread
    /// [verified: smoke log]; the canary must not be lost in that gap. Only for small events.
    fn keep_unrouted(&self) -> bool {
        false
    }
}

/// How many unrouted events are held (see [`TurnEvent::keep_unrouted`]). They only bridge the
/// moment between a `thread/start` reply and its `attach`, so a handful is plenty; the oldest goes
/// first.
const MAX_UNROUTED: usize = 32;

/// Whether the Codex child a thread lives on is still alive. A lingering turn whose child has died
/// can never complete, so it frees its session instead. Asked with no registry lock held, but on
/// the path of every new call and of `status`, so it must answer at once.
pub type Liveness = Arc<dyn Fn() -> bool + Send + Sync>;

/// A `turn/interrupt` to send now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interrupt {
    pub thread_id: String,
    pub turn_id: String,
}

/// What the notification handler must do after routing an event, straight away and without
/// waiting for a reply: it runs on the app-server reader thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FollowUp {
    /// An interrupt came due: the turn id just became known for a turn whose interrupt was asked
    /// for earlier (a cancel before `turn/start` answered, or a call that gave up waiting for it).
    Interrupt(Interrupt),
    /// A turn whose call gave up has now completed, and its session is free again. Unsubscribe
    /// from the thread now: the call could not, because unsubscribing earlier would also have
    /// stopped the very `turn/completed` that frees the session.
    Unsubscribe { thread_id: String },
}

/// A running turn, for `status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningTurn {
    pub session: String,
    pub phase: String,
    pub elapsed: Duration,
    /// Its call gave up after interrupting it, and Codex has not yet confirmed that it stopped.
    pub interrupted: bool,
}

/// The registry. Shared as an `Arc`, because slots and cancel hooks hold on to it.
pub struct Registry<E> {
    max_concurrent: usize,
    state: Mutex<State<E>>,
}

struct State<E> {
    shutting_down: bool,
    next_id: u64,
    entries: Vec<Entry<E>>,
    /// Events kept for a thread no call has attached yet, oldest first.
    unrouted: VecDeque<(String, E)>,
}

struct Entry<E> {
    id: u64,
    /// As the call named it; compared case-insensitively.
    session: String,
    started: Instant,
    phase: String,
    thread_id: Option<String>,
    turn_id: Option<String>,
    /// Whether the child the thread lives on is alive.
    child: Option<Liveness>,
    /// The call's channel. `None` until attached, and again once the call has given up.
    events: Option<Sender<E>>,
    /// The call has returned without seeing `turn/completed`.
    lingering: bool,
    /// Someone asked for the turn to be interrupted.
    interrupt_wanted: bool,
    /// The interrupt has been handed out to be sent, so it never goes twice.
    interrupt_sent: bool,
}

impl<E> Entry<E> {
    /// The interrupt now due: asked for, not yet handed out, and the thread and turn both known.
    /// Handing it out marks it sent.
    fn due_interrupt(&mut self) -> Option<Interrupt> {
        if !self.interrupt_wanted || self.interrupt_sent {
            return None;
        }
        let interrupt = Interrupt {
            thread_id: self.thread_id.clone()?,
            turn_id: self.turn_id.clone()?,
        };
        self.interrupt_sent = true;
        Some(interrupt)
    }

    fn learn_turn(&mut self, turn_id: Option<&str>) {
        if self.turn_id.is_none() {
            self.turn_id = turn_id.map(str::to_string);
        }
    }
}

impl<E: TurnEvent> Registry<E> {
    pub fn new(max_concurrent: u32) -> Self {
        Self {
            max_concurrent: (max_concurrent as usize).max(1),
            state: Mutex::new(State {
                shutting_down: false,
                next_id: 1,
                entries: Vec::new(),
                unrouted: VecDeque::new(),
            }),
        }
    }

    /// Claim `session` for one call. The slot frees it again when dropped, unless the call
    /// lingers.
    ///
    /// A lingering turn counts as running, both for its session and for `--max-concurrent`:
    /// Codex may still be generating it.
    pub fn try_start(self: &Arc<Self>, session: &str) -> Result<TurnSlot<E>, Failure> {
        self.prune_dead();
        let mut state = self.lock();
        if state.shutting_down {
            return Err(errors::server_shutting_down());
        }
        if let Some(busy) = state
            .entries
            .iter()
            .find(|e| e.session.eq_ignore_ascii_case(session))
        {
            return Err(errors::session_busy(session, busy.lingering));
        }
        if state.entries.len() >= self.max_concurrent {
            return Err(errors::too_many_running(self.max_concurrent));
        }
        let id = state.next_id;
        state.next_id += 1;
        state.entries.push(Entry {
            id,
            session: session.to_string(),
            started: Instant::now(),
            phase: "starting".to_string(),
            thread_id: None,
            turn_id: None,
            child: None,
            events: None,
            lingering: false,
            interrupt_wanted: false,
            interrupt_sent: false,
        });
        Ok(TurnSlot {
            registry: Arc::clone(self),
            id,
            session: session.to_string(),
            released: false,
        })
    }

    /// Route one notification for `thread_id` to the turn running on it. `None` when there is
    /// nothing more to do, including when no turn of ours runs on that thread (one we already
    /// left, or not ours at all), in which case the event is dropped.
    ///
    /// Runs on the app-server reader thread, so it never blocks: the channel is unbounded, and a
    /// call that has stopped reading costs nothing.
    pub fn route(&self, thread_id: &str, event: E) -> Option<FollowUp> {
        let mut state = self.lock();
        let Some(index) = state
            .entries
            .iter()
            .position(|e| e.thread_id.as_deref() == Some(thread_id))
        else {
            if event.keep_unrouted() {
                if state.unrouted.len() >= MAX_UNROUTED {
                    state.unrouted.pop_front();
                }
                state.unrouted.push_back((thread_id.to_string(), event));
            }
            return None;
        };
        let entry = &mut state.entries[index];
        if entry.lingering && event.ends_turn() {
            state.entries.remove(index);
            return Some(FollowUp::Unsubscribe {
                thread_id: thread_id.to_string(),
            });
        }
        entry.learn_turn(event.turn_id());
        let follow_up = entry.due_interrupt().map(FollowUp::Interrupt);
        if let Some(events) = &entry.events {
            // Fails only once the call has dropped its receiver, when the event is moot.
            let _ = events.send(event);
        }
        follow_up
    }

    /// Refuse new calls from now on, and interrupt every turn: the ones whose id is known now,
    /// by the returned interrupts, and the others as soon as their id turns up, through `route`
    /// or `set_turn` (docs/design.md, "Lifecycle").
    pub fn begin_shutdown(&self) -> Vec<Interrupt> {
        let mut state = self.lock();
        state.shutting_down = true;
        state
            .entries
            .iter_mut()
            .filter_map(|entry| {
                entry.interrupt_wanted = true;
                entry.due_interrupt()
            })
            .collect()
    }

    /// The turns running now, oldest first, for `status`.
    pub fn running(&self) -> Vec<RunningTurn> {
        self.prune_dead();
        self.lock()
            .entries
            .iter()
            .map(|e| RunningTurn {
                session: e.session.clone(),
                phase: e.phase.clone(),
                elapsed: e.started.elapsed(),
                interrupted: e.lingering,
            })
            .collect()
    }

    /// Free the sessions of lingering turns whose child has died: those turns can never complete.
    /// A call's own entry is left alone, since the call notices the death itself. The liveness
    /// checks run with the lock released, so a slow one never holds up the reader.
    fn prune_dead(&self) {
        let lingering: Vec<(u64, Liveness)> = self
            .lock()
            .entries
            .iter()
            .filter(|e| e.lingering)
            .filter_map(|e| e.child.as_ref().map(|alive| (e.id, Arc::clone(alive))))
            .collect();
        let dead: Vec<u64> = lingering
            .into_iter()
            .filter(|(_, alive)| !alive())
            .map(|(id, _)| id)
            .collect();
        if !dead.is_empty() {
            self.lock().entries.retain(|e| !dead.contains(&e.id));
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<E>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` on this slot's entry, if it is still there.
    fn with_entry<T>(&self, id: u64, f: impl FnOnce(&mut Entry<E>) -> T) -> Option<T> {
        let mut state = self.lock();
        state.entries.iter_mut().find(|e| e.id == id).map(f)
    }
}

/// One call's claim on its session. Dropping it (or [`finish`](Self::finish)) frees the session;
/// [`linger`](Self::linger) keeps it busy until the turn completes.
pub struct TurnSlot<E: TurnEvent> {
    registry: Arc<Registry<E>>,
    id: u64,
    session: String,
    released: bool,
}

impl<E: TurnEvent> TurnSlot<E> {
    /// The session name, as the call gave it.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The phase `status` shows for this turn.
    pub fn set_phase(&self, phase: &str) {
        self.registry
            .with_entry(self.id, |e| e.phase = phase.to_string());
    }

    /// Route `thread_id`'s notifications to this call from now on, and say how to tell whether the
    /// child the thread lives on is alive. Call it as soon as `thread/start` answers, before `turn/start`, so that no
    /// notification of the turn can arrive unrouted.
    ///
    /// Events kept for this thread before it was attached (see [`TurnEvent::keep_unrouted`]) are
    /// delivered first, in the order they arrived. That happens under the same lock `route` takes,
    /// so every event reaches the call exactly once, whichever side of the attach it arrived on.
    pub fn attach(&self, thread_id: &str, alive: Liveness) -> Receiver<E> {
        let (events, receiver) = mpsc::channel();
        let mut guard = self.registry.lock();
        let state = &mut *guard;
        let Some(entry) = state.entries.iter_mut().find(|e| e.id == self.id) else {
            return receiver;
        };
        let mut kept = VecDeque::new();
        for (thread, event) in state.unrouted.drain(..) {
            if thread == thread_id {
                entry.learn_turn(event.turn_id());
                let _ = events.send(event);
            } else {
                kept.push_back((thread, event));
            }
        }
        state.unrouted = kept;
        entry.thread_id = Some(thread_id.to_string());
        entry.child = Some(alive);
        entry.events = Some(events);
        receiver
    }

    /// Record the turn id from `turn/start`'s reply. Returns the interrupt to send now if one was
    /// asked for before the id was known.
    pub fn set_turn(&self, turn_id: &str) -> Option<Interrupt> {
        self.registry
            .with_entry(self.id, |e| {
                e.learn_turn(Some(turn_id));
                e.due_interrupt()
            })
            .flatten()
    }

    /// A handle for the cancel hook, which runs on the MCP reader thread. It may outlive the slot,
    /// and then does nothing.
    pub fn handle(&self) -> TurnHandle<E> {
        TurnHandle {
            registry: Arc::clone(&self.registry),
            id: self.id,
        }
    }

    /// The turn is over (`turn/completed` was seen, or no turn was ever started): free the
    /// session. The same as dropping the slot, spelled out.
    pub fn finish(self) {}

    /// Give up on the turn before its `turn/completed`: return now, but keep the session busy
    /// until that `turn/completed` arrives (routed here, it comes back as
    /// [`FollowUp::Unsubscribe`]) or the child dies. Call it only once `turn/start` has been sent.
    ///
    /// The turn is interrupted: the returned interrupt, if its id is known and none was sent yet,
    /// is for the caller to send; otherwise it comes due through `route` as soon as the id turns
    /// up. A call that never attached a thread has no turn to wait for, so its session is freed at
    /// once.
    pub fn linger(mut self) -> Option<Interrupt> {
        self.released = true;
        let mut state = self.registry.lock();
        let index = state.entries.iter().position(|e| e.id == self.id)?;
        if state.entries[index].thread_id.is_none() {
            state.entries.remove(index);
            return None;
        }
        let entry = &mut state.entries[index];
        entry.lingering = true;
        entry.events = None;
        entry.interrupt_wanted = true;
        entry.due_interrupt()
    }
}

impl<E: TurnEvent> Drop for TurnSlot<E> {
    fn drop(&mut self) {
        if !self.released {
            self.registry.lock().entries.retain(|e| e.id != self.id);
        }
    }
}

/// What a cancel hook holds: enough to ask for this turn's interrupt from another thread.
pub struct TurnHandle<E> {
    registry: Arc<Registry<E>>,
    id: u64,
}

impl<E: TurnEvent> TurnHandle<E> {
    /// Ask for the turn to be interrupted. Returns the interrupt to send now when the turn id is
    /// known and no interrupt was handed out yet. Otherwise the request is remembered, and the
    /// interrupt comes due through `set_turn` or `route` once the id is known.
    ///
    /// Takes one short lock and never waits on Codex, so it is safe in a cancel hook on the MCP
    /// reader thread.
    pub fn request_interrupt(&self) -> Option<Interrupt> {
        self.registry
            .with_entry(self.id, |e| {
                e.interrupt_wanted = true;
                e.due_interrupt()
            })
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, PartialEq, Eq)]
    struct Event {
        turn: Option<&'static str>,
        ends: bool,
        label: &'static str,
    }

    impl TurnEvent for Event {
        fn turn_id(&self) -> Option<&str> {
            self.turn
        }
        fn ends_turn(&self) -> bool {
            self.ends
        }
        fn keep_unrouted(&self) -> bool {
            self.label.starts_with("keep")
        }
    }

    fn item(turn: &'static str, label: &'static str) -> Event {
        Event {
            turn: Some(turn),
            ends: false,
            label,
        }
    }

    fn completed(turn: &'static str) -> Event {
        Event {
            turn: Some(turn),
            ends: true,
            label: "turn/completed",
        }
    }

    fn untagged(label: &'static str) -> Event {
        Event {
            turn: None,
            ends: false,
            label,
        }
    }

    fn registry(max: u32) -> Arc<Registry<Event>> {
        Arc::new(Registry::new(max))
    }

    fn alive() -> Liveness {
        Arc::new(|| true)
    }

    /// A liveness the test can flip.
    fn switch() -> (Arc<AtomicBool>, Liveness) {
        let flag = Arc::new(AtomicBool::new(true));
        let probe = Arc::clone(&flag);
        (flag, Arc::new(move || probe.load(Ordering::SeqCst)))
    }

    fn interrupt(thread: &str, turn: &str) -> Interrupt {
        Interrupt {
            thread_id: thread.to_string(),
            turn_id: turn.to_string(),
        }
    }

    fn code(result: Result<TurnSlot<Event>, Failure>) -> &'static str {
        match result {
            Ok(_) => "OK",
            Err(f) => f.code,
        }
    }

    #[test]
    fn a_busy_session_is_refused_whatever_its_case() {
        let r = registry(4);
        let slot = r.try_start("Fox").unwrap();
        assert_eq!(slot.session(), "Fox");
        let err = r.try_start("fox").err().unwrap();
        assert_eq!(err.code, "SESSION_BUSY");
        assert!(err.summary.contains("another call on it is still running"));
        assert!(err.render_for_agent().starts_with("REQUEST REJECTED"));
        assert_eq!(code(r.try_start("FOX")), "SESSION_BUSY");
        // A different name is not busy.
        let other = r.try_start("fox2").unwrap();
        drop(slot);
        // Freed on drop.
        let again = r.try_start("fOx").unwrap();
        drop((other, again));
        assert!(r.running().is_empty());
    }

    #[test]
    fn the_concurrency_cap_counts_every_running_turn() {
        let r = registry(2);
        let a = r.try_start("a").unwrap();
        let _b = r.try_start("b").unwrap();
        let err = r.try_start("c").err().unwrap();
        assert_eq!(err.code, "TOO_MANY_RUNNING");
        assert!(
            err.summary.contains("--max-concurrent 2"),
            "{}",
            err.summary
        );
        // A busy name is reported as busy, the more specific reason.
        assert_eq!(code(r.try_start("A")), "SESSION_BUSY");
        a.finish();
        assert_eq!(code(r.try_start("c")), "OK");
    }

    #[test]
    fn concurrent_claims_on_one_name_let_exactly_one_through() {
        let r = registry(64);
        let barrier = std::sync::Barrier::new(16);
        // Every result is held until all are in, so a winner cannot free the name for a latecomer.
        let results: Vec<Result<TurnSlot<Event>, Failure>> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    s.spawn(|| {
                        barrier.wait();
                        r.try_start("same")
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert!(results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|f| f.code == "SESSION_BUSY"));
    }

    #[test]
    fn shutdown_refuses_new_calls_and_interrupts_every_turn() {
        let r = registry(4);
        let known = r.try_start("known").unwrap();
        let _rx = known.attach("t1", alive());
        known.set_turn("u1");
        let pending = r.try_start("pending").unwrap();
        let rx = pending.attach("t2", alive());
        assert_eq!(r.begin_shutdown(), vec![interrupt("t1", "u1")]);
        assert_eq!(code(r.try_start("new")), "SERVER_SHUTTING_DOWN");
        // The turn whose id was not known yet is interrupted as soon as it turns up, once.
        assert_eq!(
            r.route("t2", item("u2", "turn/started")),
            Some(FollowUp::Interrupt(interrupt("t2", "u2")))
        );
        assert_eq!(r.route("t2", item("u2", "item/started")), None);
        assert_eq!(rx.try_iter().count(), 2, "both still reach the call");
        // Nothing is handed out twice.
        assert!(r.begin_shutdown().is_empty());
    }

    #[test]
    fn events_reach_the_call_that_attached_the_thread_and_nothing_else() {
        let r = registry(4);
        let a = r.try_start("a").unwrap();
        let b = r.try_start("b").unwrap();
        let rx_a = a.attach("thread-a", alive());
        let rx_b = b.attach("thread-b", alive());
        assert_eq!(r.route("thread-a", untagged("thread/started")), None);
        assert_eq!(r.route("thread-b", item("tb", "item/started")), None);
        assert_eq!(r.route("thread-a", item("ta", "item/completed")), None);
        assert_eq!(r.route("elsewhere", untagged("stray")), None);
        let labels = |rx: &Receiver<Event>| rx.try_iter().map(|e| e.label).collect::<Vec<_>>();
        assert_eq!(labels(&rx_a), vec!["thread/started", "item/completed"]);
        assert_eq!(labels(&rx_b), vec!["item/started"]);
        // Once the call is gone, its thread is no longer ours.
        drop(a);
        drop(rx_a);
        assert_eq!(r.route("thread-a", item("ta", "late")), None);
    }

    #[test]
    fn an_event_that_beats_the_attach_is_delivered_on_attach_if_it_asks_to_be_kept() {
        let r = registry(4);
        let slot = r.try_start("s").unwrap();
        // Routed before the call has attached its thread: kept, or dropped, as each asks.
        assert_eq!(r.route("t", untagged("keep: mcp startup")), None);
        assert_eq!(r.route("t", untagged("thread/started")), None);
        assert_eq!(r.route("other", untagged("keep: someone else's")), None);
        let rx = slot.attach("t", alive());
        assert_eq!(r.route("t", untagged("after")), None);
        let labels: Vec<&str> = rx.try_iter().map(|e| e.label).collect();
        assert_eq!(labels, vec!["keep: mcp startup", "after"]);
        // The other thread's event is still held for whoever attaches it.
        let other = r.try_start("o").unwrap();
        let rx = other.attach("other", alive());
        assert_eq!(rx.try_iter().count(), 1);

        // The buffer is bounded: the oldest go first.
        for _ in 0..(MAX_UNROUTED + 5) {
            r.route("nobody", untagged("keep"));
        }
        assert_eq!(r.lock().unrouted.len(), MAX_UNROUTED);
    }

    #[test]
    fn a_call_that_stopped_reading_never_blocks_the_router() {
        let r = registry(1);
        let slot = r.try_start("s").unwrap();
        let rx = slot.attach("t", alive());
        let started = Instant::now();
        for _ in 0..10_000 {
            r.route("t", item("u", "item/updated"));
        }
        drop(rx);
        r.route("t", item("u", "after the receiver is gone"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn an_interrupt_is_handed_out_once_whoever_asks_first() {
        // Cancelled after the turn started: the hook gets the interrupt at once.
        let r = registry(4);
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", alive());
        assert_eq!(slot.set_turn("u"), None, "nobody asked yet");
        let hook = slot.handle();
        assert_eq!(hook.request_interrupt(), Some(interrupt("t", "u")));
        assert_eq!(hook.request_interrupt(), None);
        assert_eq!(slot.linger(), None, "already sent by the hook");
    }

    #[test]
    fn a_cancel_before_the_turn_id_is_known_comes_due_when_it_arrives() {
        // From turn/start's reply...
        let r = registry(4);
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", alive());
        assert_eq!(slot.handle().request_interrupt(), None);
        assert_eq!(slot.set_turn("u"), Some(interrupt("t", "u")));
        assert_eq!(slot.set_turn("u"), None);
        drop(slot);

        // ...or from a notification that names it, which is still delivered.
        let slot = r.try_start("s").unwrap();
        let rx = slot.attach("t2", alive());
        assert_eq!(slot.handle().request_interrupt(), None);
        assert_eq!(r.route("t2", untagged("thread/status/changed")), None);
        assert_eq!(
            r.route("t2", item("u2", "turn/started")),
            Some(FollowUp::Interrupt(interrupt("t2", "u2")))
        );
        assert_eq!(rx.try_iter().count(), 2);
        assert_eq!(slot.set_turn("u2"), None);
    }

    #[test]
    fn a_lingering_turn_keeps_its_session_busy_until_turn_completed() {
        let r = registry(2);
        let slot = r.try_start("fox").unwrap();
        let rx = slot.attach("t", alive());
        slot.set_turn("u");
        slot.set_phase("generating image");
        assert_eq!(slot.linger(), Some(interrupt("t", "u")));
        // The call's channel is closed; the session is still taken, and still counts.
        assert!(rx.recv_timeout(Duration::from_millis(10)).is_err());
        let err = r.try_start("FOX").err().unwrap();
        assert_eq!(err.code, "SESSION_BUSY");
        assert!(err.summary.contains("interrupted"), "{}", err.summary);
        let _other = r.try_start("other").unwrap();
        assert_eq!(code(r.try_start("third")), "TOO_MANY_RUNNING");
        let running = r.running();
        assert_eq!(running.len(), 2);
        assert_eq!(running[0].session, "fox");
        assert_eq!(running[0].phase, "generating image");
        assert!(running[0].interrupted);
        assert!(running[0].elapsed < Duration::from_secs(60));
        assert!(!running[1].interrupted);

        // Other notifications change nothing; turn/completed frees it and asks for the unsubscribe.
        assert_eq!(r.route("t", item("u", "item/completed")), None);
        assert_eq!(
            r.route("t", completed("u")),
            Some(FollowUp::Unsubscribe {
                thread_id: "t".to_string()
            })
        );
        assert_eq!(code(r.try_start("fox")), "OK");
    }

    #[test]
    fn a_turn_start_reply_that_came_too_late_is_interrupted_when_the_turn_shows_up() {
        let r = registry(4);
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", alive());
        // The call gave up while turn/start was still unanswered.
        assert_eq!(slot.linger(), None);
        assert_eq!(r.route("t", untagged("thread/status/changed")), None);
        assert_eq!(
            r.route("t", item("u", "turn/started")),
            Some(FollowUp::Interrupt(interrupt("t", "u")))
        );
        assert_eq!(r.route("t", item("u", "item/started")), None);
        assert_eq!(code(r.try_start("s")), "SESSION_BUSY");
        assert_eq!(
            r.route("t", completed("u")),
            Some(FollowUp::Unsubscribe {
                thread_id: "t".to_string()
            })
        );
        assert_eq!(code(r.try_start("s")), "OK");
    }

    #[test]
    fn a_lingering_turn_on_a_dead_child_frees_its_session() {
        let r = registry(1);
        let (child_alive, liveness) = switch();
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", liveness);
        slot.set_turn("u");
        slot.linger();
        assert_eq!(code(r.try_start("s")), "SESSION_BUSY");
        assert_eq!(r.running().len(), 1);
        child_alive.store(false, Ordering::SeqCst);
        assert!(r.running().is_empty());
        assert_eq!(code(r.try_start("s")), "OK");
    }

    #[test]
    fn a_running_call_is_never_pruned_even_if_its_child_died() {
        // The call notices the death itself and returns; its entry goes with its slot.
        let r = registry(1);
        let (child_alive, liveness) = switch();
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", liveness);
        child_alive.store(false, Ordering::SeqCst);
        assert_eq!(code(r.try_start("s")), "SESSION_BUSY");
        assert_eq!(r.running().len(), 1);
        drop(slot);
        assert_eq!(code(r.try_start("s")), "OK");
    }

    #[test]
    fn giving_up_before_any_thread_frees_the_session_at_once() {
        let r = registry(1);
        let slot = r.try_start("s").unwrap();
        assert_eq!(slot.linger(), None);
        assert_eq!(code(r.try_start("s")), "OK");
    }

    #[test]
    fn the_cancel_hook_handle_outlives_the_slot_harmlessly() {
        let r = registry(1);
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", alive());
        let hook = slot.handle();
        slot.finish();
        assert_eq!(hook.request_interrupt(), None);
        // And it works from another thread, as the MCP reader runs it.
        let slot = r.try_start("s").unwrap();
        let _rx = slot.attach("t", alive());
        slot.set_turn("u");
        let hook = slot.handle();
        let sent = std::thread::spawn(move || hook.request_interrupt())
            .join()
            .unwrap();
        assert_eq!(sent, Some(interrupt("t", "u")));
    }
}
