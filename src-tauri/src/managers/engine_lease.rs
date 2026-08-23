//! Exclusive access to the loaded transcription engine.
//!
//! [`TranscriptionManager`](super::transcription::TranscriptionManager) hands the
//! engine *out* of its `Mutex<Option<LoadedEngine>>` for the duration of a call
//! rather than holding the mutex across inference. That keeps the lock
//! uncontended, but it means two concurrent users of the engine do not queue —
//! the second one finds `None` and fails with a misleading "model failed to
//! load" error. The streaming worker worked around this with a bare
//! `AtomicU64` lease that only it respected; batch transcription never
//! participated, so a hotkey dictation racing a history retry (or, now, a CLI
//! job) could still lose.
//!
//! This gate makes the exclusion explicit and total: every path that needs the
//! engine acquires a lease first and holds it for the whole operation. Waiters
//! block on a condvar instead of failing.
//!
//! Interactive work wins. [`LeaseOwner::Batch`] waiters — real dictations —
//! register as interactive, and a [`LeaseOwner::Cli`] waiter yields to them
//! rather than taking a freed engine out from under a user who is waiting on
//! their own transcript. A running job is never preempted — Handy installs no
//! cancel token, so an in-flight inference runs to completion — meaning this
//! only reorders the queue; [`CLI_STARVATION_CAP`] bounds how long a CLI job
//! can be held back by a stream of dictations.

pub use handy_core::protocol::LeaseOwner;
use log::warn;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long a CLI waiter defers to interactive waiters before insisting.
const CLI_STARVATION_CAP: Duration = Duration::from_secs(60);

/// Longest single condvar wait. Bounding it keeps cancellation and the
/// starvation cap responsive without needing a notify for either.
const POLL_SLICE: Duration = Duration::from_millis(200);

/// Why a lease could not be acquired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseError {
    /// The engine is held and the caller asked not to wait.
    Busy(LeaseOwner),
    /// The caller's timeout elapsed. Carries the current holder, which is
    /// `None` when a CLI waiter timed out while yielding to interactive work.
    Timeout(Option<LeaseOwner>),
    /// The caller's cancel token was set while waiting.
    Cancelled,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LeaseError::Busy(owner) => {
                write!(f, "the transcription engine is busy with a {}", owner)
            }
            LeaseError::Timeout(Some(owner)) => write!(
                f,
                "timed out waiting for the transcription engine (held by a {})",
                owner
            ),
            LeaseError::Timeout(None) => {
                f.write_str("timed out waiting for the transcription engine")
            }
            LeaseError::Cancelled => f.write_str("cancelled while waiting for the engine"),
        }
    }
}

impl std::error::Error for LeaseError {}

#[derive(Default)]
struct LeaseState {
    /// Current holder, if any.
    owner: Option<LeaseOwner>,
    /// Identifies the current lease so a stale [`EngineLease`] drop cannot
    /// release a newer holder's claim.
    token: u64,
    next_token: u64,
    /// Number of interactive waiters currently blocked in [`EngineLeaseGate::acquire`].
    interactive_waiters: usize,
}

/// The gate itself. Cheap to clone via `Arc`; one instance per manager.
#[derive(Default)]
pub struct EngineLeaseGate {
    state: Mutex<LeaseState>,
    condvar: Condvar,
}

impl EngineLeaseGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|poisoned| {
            warn!("Engine lease mutex was poisoned by a previous panic, recovering");
            poisoned.into_inner()
        })
    }

    /// Who holds the engine right now, if anyone.
    pub fn busy_owner(&self) -> Option<LeaseOwner> {
        self.lock().owner
    }

    /// Whether any interactive waiter is currently queued behind the holder.
    /// A long-running CLI job polls this to tell the client that a dictation is
    /// waiting on it.
    pub fn has_interactive_waiters(&self) -> bool {
        self.lock().interactive_waiters > 0
    }

    /// Take the engine if it is free, without waiting.
    pub fn try_acquire(self: &Arc<Self>, owner: LeaseOwner) -> Result<EngineLease, LeaseError> {
        let mut state = self.lock();
        match state.owner {
            Some(current) => Err(LeaseError::Busy(current)),
            None => Ok(self.grant(&mut state, owner)),
        }
    }

    /// Take the engine, waiting for it to free up.
    ///
    /// `timeout` of `None` waits indefinitely. `cancel` is polled while waiting
    /// (at most [`POLL_SLICE`] late) so a disconnected CLI client stops
    /// occupying a slot in the queue.
    pub fn acquire(
        self: &Arc<Self>,
        owner: LeaseOwner,
        timeout: Option<Duration>,
        cancel: Option<&Arc<AtomicBool>>,
    ) -> Result<EngineLease, LeaseError> {
        let interactive = owner.is_interactive_waiter();
        let started = Instant::now();
        let deadline = timeout.map(|t| started + t);

        let mut state = self.lock();
        if interactive {
            state.interactive_waiters += 1;
        }

        let outcome = loop {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                break Err(LeaseError::Cancelled);
            }

            if state.owner.is_none() {
                // A CLI job steps aside for anyone with a human waiting, but
                // only for so long — otherwise steady dictation could starve it
                // forever.
                let yielding = !interactive
                    && state.interactive_waiters > 0
                    && started.elapsed() < CLI_STARVATION_CAP;
                if !yielding {
                    break Ok(self.grant(&mut state, owner));
                }
            }

            let now = Instant::now();
            if let Some(deadline) = deadline {
                if now >= deadline {
                    break Err(LeaseError::Timeout(state.owner));
                }
            }
            let slice = match deadline {
                Some(deadline) => deadline.saturating_duration_since(now).min(POLL_SLICE),
                None => POLL_SLICE,
            };
            let (next, _) = self
                .condvar
                .wait_timeout(state, slice)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = next;
        };

        if interactive {
            state.interactive_waiters -= 1;
            // A CLI waiter may have been yielding to us; let it re-evaluate.
            self.condvar.notify_all();
        }
        outcome
    }

    /// Record `owner` as the holder and mint the lease. Caller holds the lock.
    fn grant(self: &Arc<Self>, state: &mut LeaseState, owner: LeaseOwner) -> EngineLease {
        state.next_token += 1;
        state.token = state.next_token;
        state.owner = Some(owner);
        EngineLease {
            gate: Arc::clone(self),
            token: state.token,
            owner,
        }
    }

    fn release(&self, token: u64) {
        let mut state = self.lock();
        // A lease that was superseded (which should not happen, but is cheap to
        // guard) must not clear the current holder's claim.
        if state.token == token && state.owner.is_some() {
            state.owner = None;
            self.condvar.notify_all();
        }
    }
}

/// RAII claim on the transcription engine. Releasing it wakes every waiter.
pub struct EngineLease {
    gate: Arc<EngineLeaseGate>,
    token: u64,
    owner: LeaseOwner,
}

impl EngineLease {
    pub fn owner(&self) -> LeaseOwner {
        self.owner
    }

    /// Whether an interactive waiter is queued behind this lease.
    pub fn has_interactive_waiters(&self) -> bool {
        self.gate.has_interactive_waiters()
    }
}

impl Drop for EngineLease {
    fn drop(&mut self) {
        self.gate.release(self.token);
    }
}

impl fmt::Debug for EngineLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EngineLease")
            .field("owner", &self.owner)
            .field("token", &self.token)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn try_acquire_is_exclusive_and_names_the_holder() {
        let gate = Arc::new(EngineLeaseGate::new());
        let lease = gate.try_acquire(LeaseOwner::Cli).expect("first acquire");
        assert_eq!(gate.busy_owner(), Some(LeaseOwner::Cli));
        assert!(matches!(
            gate.try_acquire(LeaseOwner::Batch),
            Err(LeaseError::Busy(LeaseOwner::Cli))
        ));
        drop(lease);
        assert_eq!(gate.busy_owner(), None);
        assert!(gate.try_acquire(LeaseOwner::Batch).is_ok());
    }

    #[test]
    fn acquire_waits_for_the_holder_to_release() {
        let gate = Arc::new(EngineLeaseGate::new());
        let lease = gate.try_acquire(LeaseOwner::Cli).unwrap();

        let (tx, rx) = mpsc::channel();
        let waiter = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let lease = gate.acquire(LeaseOwner::Batch, None, None).unwrap();
                tx.send(()).unwrap();
                drop(lease);
            })
        };

        // Still blocked while the CLI job holds it.
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
        drop(lease);
        rx.recv_timeout(Duration::from_secs(5))
            .expect("batch waiter should be woken by the release");
        waiter.join().unwrap();
    }

    #[test]
    fn timeout_reports_the_current_holder() {
        let gate = Arc::new(EngineLeaseGate::new());
        let _held = gate.try_acquire(LeaseOwner::Stream).unwrap();
        let err = gate
            .acquire(LeaseOwner::Batch, Some(Duration::from_millis(50)), None)
            .unwrap_err();
        assert_eq!(err, LeaseError::Timeout(Some(LeaseOwner::Stream)));
    }

    #[test]
    fn cancel_token_aborts_the_wait() {
        let gate = Arc::new(EngineLeaseGate::new());
        let _held = gate.try_acquire(LeaseOwner::Batch).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let waiter = {
            let gate = Arc::clone(&gate);
            let cancel = Arc::clone(&cancel);
            thread::spawn(move || gate.acquire(LeaseOwner::Cli, None, Some(&cancel)))
        };
        thread::sleep(Duration::from_millis(50));
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(waiter.join().unwrap(), Err(LeaseError::Cancelled)));
    }

    #[test]
    fn interactive_waiter_is_served_before_a_queued_cli_waiter() {
        let gate = Arc::new(EngineLeaseGate::new());
        let held = gate.try_acquire(LeaseOwner::Stream).unwrap();
        let (tx, rx) = mpsc::channel();

        // The CLI waiter queues first, so without the priority rule it would win.
        let cli = {
            let gate = Arc::clone(&gate);
            let tx = tx.clone();
            thread::spawn(move || {
                let lease = gate.acquire(LeaseOwner::Cli, None, None).unwrap();
                tx.send(LeaseOwner::Cli).unwrap();
                drop(lease);
            })
        };
        thread::sleep(Duration::from_millis(100));
        let batch = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let lease = gate.acquire(LeaseOwner::Batch, None, None).unwrap();
                tx.send(LeaseOwner::Batch).unwrap();
                drop(lease);
            })
        };
        thread::sleep(Duration::from_millis(100));
        drop(held);

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            LeaseOwner::Batch
        );
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            LeaseOwner::Cli
        );
        batch.join().unwrap();
        cli.join().unwrap();
    }
}
