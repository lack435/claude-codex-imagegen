//! Per-request cancellation, shared between the protocol loop and a tool call.
//!
//! An MCP client abandons a request with `notifications/cancelled`. The receiver is meant to stop
//! processing it and send no response, so every `tools/call` handler carries one of these: the
//! protocol loop flips it, and the handler's reply is suppressed.
//!
//! Suppressing the reply is the cheap half. The expensive half is the Codex turn the call started,
//! which left alone keeps generating on behalf of a caller that has gone away. So a call can
//! install a *hook* -- in practice, "send `turn/interrupt` for this turn" -- that cancellation runs.
//!
//! The rule this type exists to enforce is that a response and a cancellation of the same request
//! never both take effect (docs/design.md, "Progress and cancellation"): either the response goes
//! out, or the hook runs and no response does.

use std::sync::{Mutex, MutexGuard};

/// What cancellation runs. Boxed and `Send` because it is installed by the handler thread and run
/// by the protocol reader thread.
pub type CancelHook = Box<dyn FnOnce() + Send>;

/// Cancellation state for one in-flight request.
#[derive(Default)]
pub struct RequestCancel {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    cancelled: bool,
    /// Whether the handler has committed to sending a response. Once it has, a late cancellation
    /// must not run the hook: the response describes work that is then left alone.
    responded: bool,
    hook: Option<CancelHook>,
}

impl RequestCancel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Has the client abandoned this request?
    ///
    /// The lock is held for a flag read only. It is never held across a write to stdout: a client
    /// that stopped draining stdout must not stall the protocol reader, and with it every other
    /// request's cancellation.
    pub fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// Install what cancellation should do, replacing any earlier hook. Returns `false`, and drops
    /// the hook unrun, if the request is already cancelled: the caller must then do the cleanup
    /// itself.
    ///
    /// Install and check happen under one lock. Doing them separately would lose a cancellation
    /// that lands in the gap: it would find no hook to run, and the caller would see a stale "not
    /// cancelled" and carry on generating for nobody.
    // No caller until a turn exists to interrupt (milestone M2); the tests exercise it now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_hook(&self, hook: CancelHook) -> bool {
        let mut state = self.lock();
        if state.cancelled {
            return false;
        }
        state.hook = Some(hook);
        true
    }

    /// Remove the hook, for when the work it would stop has finished on its own.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear_hook(&self) {
        let hook = self.lock().hook.take();
        // Dropped outside the lock, in case dropping its captures does anything slow.
        drop(hook);
    }

    /// Mark the request cancelled and run the hook, unless a response was already claimed.
    /// Returns whether a hook ran. Idempotent: the hook is taken, so it runs at most once.
    ///
    /// The hook runs outside the lock. It typically writes to the Codex child, which can block,
    /// and it may itself ask [`is_cancelled`](Self::is_cancelled); neither may happen under the
    /// state lock.
    pub fn cancel(&self) -> bool {
        let hook = {
            let mut state = self.lock();
            state.cancelled = true;
            if state.responded {
                None
            } else {
                state.hook.take()
            }
        };
        match hook {
            Some(hook) => {
                hook();
                true
            }
            None => false,
        }
    }

    /// Claim the right to answer this request, or learn that the client has cancelled and no
    /// response may be sent.
    ///
    /// Claiming and cancelling contend for one lock, so exactly one of them wins. That is what
    /// stops the sequence "the response goes out, then the turn it describes is interrupted": the
    /// loser either suppresses its response or leaves the turn alone. The lock is held for a flag
    /// flip only, never across the write of the response.
    pub fn try_claim_response(&self) -> bool {
        let mut state = self.lock();
        if state.cancelled {
            return false;
        }
        state.responded = true;
        // The hook can no longer run, so release what it captured now.
        let hook = state.hook.take();
        drop(state);
        drop(hook);
        true
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn counting_hook(counter: &Arc<AtomicUsize>) -> CancelHook {
        let counter = Arc::clone(counter);
        Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    }

    #[test]
    fn a_fresh_request_may_answer_and_has_nothing_to_run() {
        assert!(RequestCancel::new().try_claim_response());
        assert!(!RequestCancel::new().cancel());
    }

    #[test]
    fn cancelling_runs_the_hook_once_and_bars_a_response() {
        let runs = Arc::new(AtomicUsize::new(0));
        let request = RequestCancel::new();
        assert!(request.set_hook(counting_hook(&runs)));
        assert!(request.cancel());
        assert!(
            !request.cancel(),
            "a second cancellation has nothing left to run"
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(!request.try_claim_response());
    }

    #[test]
    fn installing_a_hook_after_a_cancellation_says_so_and_does_not_run_it() {
        // The losing side of the race: the notification arrived before the turn started, so the
        // caller is told to clean up itself.
        let runs = Arc::new(AtomicUsize::new(0));
        let request = RequestCancel::new();
        assert!(!request.cancel());
        assert!(!request.set_hook(counting_hook(&runs)));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_claimed_response_wins_and_the_hook_never_runs() {
        let runs = Arc::new(AtomicUsize::new(0));
        let request = RequestCancel::new();
        request.set_hook(counting_hook(&runs));
        assert!(request.try_claim_response());
        // The response is going out, so a cancellation arriving now must leave the work alone.
        assert!(!request.cancel());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(request.is_cancelled());
    }

    #[test]
    fn a_cleared_hook_does_not_run() {
        let runs = Arc::new(AtomicUsize::new(0));
        let request = RequestCancel::new();
        request.set_hook(counting_hook(&runs));
        request.clear_hook();
        assert!(!request.cancel());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_new_hook_replaces_the_old_one() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let request = RequestCancel::new();
        request.set_hook(counting_hook(&first));
        request.set_hook(counting_hook(&second));
        request.cancel();
        assert_eq!(first.load(Ordering::SeqCst), 0);
        assert_eq!(second.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_hook_runs_outside_the_lock() {
        // A hook that asks about the request it belongs to would deadlock if it ran under the
        // state lock.
        let request = Arc::new(RequestCancel::new());
        let seen = Arc::new(AtomicUsize::new(0));
        {
            let request_in_hook = Arc::clone(&request);
            let seen = Arc::clone(&seen);
            request.set_hook(Box::new(move || {
                if request_in_hook.is_cancelled() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        assert!(request.cancel());
        assert_eq!(seen.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_response_and_a_cancellation_never_both_take_effect() {
        // Race the two from separate threads many times. Whatever the interleaving, exactly one
        // side wins: a claimed response means the hook never ran, and a hook that ran means no
        // response was claimed.
        for _ in 0..500 {
            let request = Arc::new(RequestCancel::new());
            let runs = Arc::new(AtomicUsize::new(0));
            assert!(request.set_hook(counting_hook(&runs)));
            let barrier = Arc::new(Barrier::new(2));

            let canceller = {
                let request = Arc::clone(&request);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    request.cancel()
                })
            };
            barrier.wait();
            let claimed = request.try_claim_response();
            let hook_ran = canceller.join().unwrap();

            assert_ne!(claimed, hook_ran, "exactly one side must win");
            assert_eq!(runs.load(Ordering::SeqCst), usize::from(hook_ran));
        }
    }
}
