//! Request-scoped cancellation. Commit is the only point where evidence may be admitted.
use crate::document::{error, Error};
use serde::Serialize;
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelResult {
    Requested,
    AlreadyRequested,
    TooLate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Cancelled,
    Deadline,
    Committed,
    Finished,
}
type Interrupt = Arc<dyn Fn() + Send + Sync>;
struct State {
    phase: Phase,
    deadline: Instant,
    interrupt: Option<(u64, Interrupt)>,
    #[cfg(any(windows, feature = "cpu-onnx", test))]
    next_interrupt: u64,
}
struct Inner {
    state: Mutex<State>,
    changed: Condvar,
}
struct Owner {
    inner: Arc<Inner>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.phase == Phase::Running {
            state.phase = Phase::Finished;
        }
        state.interrupt = None;
        self.inner.changed.notify_all();
    }
}

/// A clone targets the same request, never a PID or another request.
/// One bounded-lifetime timer per control; finishing/dropping the last owner wakes it.
#[derive(Clone)]
pub struct RequestControl {
    owner: Arc<Owner>,
}
impl std::fmt::Debug for RequestControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RequestControl(..)")
    }
}

fn update_deadline(state: &mut State) {
    if state.phase == Phase::Running && Instant::now() >= state.deadline {
        state.phase = Phase::Deadline;
        if let Some((_, interrupt)) = &state.interrupt {
            interrupt();
        }
    }
}
fn check(state: &mut State) -> Result<(), Error> {
    update_deadline(state);
    match state.phase {
        Phase::Running => Ok(()),
        Phase::Cancelled => Err(error("request cancelled; no partial result")),
        Phase::Deadline => Err(error("request deadline exceeded; no partial result")),
        Phase::Committed | Phase::Finished => Err(error("request already finished")),
    }
}
impl RequestControl {
    pub fn new(timeout: Duration) -> Result<Self, Error> {
        if timeout.is_zero() || timeout > Duration::from_secs(300) {
            return Err(error("request timeout must be positive and <=300s"));
        }
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                phase: Phase::Running,
                deadline: Instant::now() + timeout,
                interrupt: None,
                #[cfg(any(windows, feature = "cpu-onnx", test))]
                next_interrupt: 0,
            }),
            changed: Condvar::new(),
        });
        let owner = Arc::new(Owner {
            inner: inner.clone(),
        });
        std::thread::Builder::new()
            .name("retrieval-deadline".into())
            .spawn(move || {
                let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    update_deadline(&mut state);
                    if state.phase != Phase::Running {
                        break;
                    }
                    let wait = state.deadline.saturating_duration_since(Instant::now());
                    state = inner
                        .changed
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            })
            .map_err(|_| error("request deadline timer unavailable"))?;
        Ok(Self { owner })
    }
    pub fn checkpoint(&self) -> Result<(), Error> {
        check(
            &mut self
                .owner
                .inner
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        )
    }
    /// Shorten a surrounding request to the current operation's smaller budget.
    pub fn limit_deadline(&self, deadline: Instant) -> Result<(), Error> {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        state.deadline = state.deadline.min(deadline);
        self.owner.inner.changed.notify_all();
        check(&mut state)
    }
    pub fn cancel(&self) -> CancelResult {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        update_deadline(&mut state);
        match state.phase {
            Phase::Running => {
                state.phase = Phase::Cancelled;
                if let Some((_, interrupt)) = &state.interrupt {
                    interrupt();
                }
                self.owner.inner.changed.notify_all();
                CancelResult::Requested
            }
            Phase::Cancelled | Phase::Deadline => CancelResult::AlreadyRequested,
            Phase::Committed | Phase::Finished => CancelResult::TooLate,
        }
    }
    /// The closure must be short, atomic on error, and contain no blocking I/O.
    /// Cancel and commit serialize under the same lock: cancellation cannot win
    /// and then allow this closure to create references. An admitted commit may finish.
    pub fn commit<T>(&self, admit: impl FnOnce() -> Result<T, Error>) -> Result<T, Error> {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        check(&mut state)?;
        let result = admit();
        state.phase = if result.is_ok() {
            Phase::Committed
        } else {
            Phase::Finished
        };
        state.interrupt = None;
        self.owner.inner.changed.notify_all();
        result
    }
    /// End a failed/read-only job so its timer cannot outlive the operation.
    /// This does not undo committed references or turn a cancellation into success.
    pub fn finish(&self) {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state.phase == Phase::Running {
            state.phase = Phase::Finished;
        }
        state.interrupt = None;
        self.owner.inner.changed.notify_all();
    }
    #[cfg(any(windows, feature = "cpu-onnx", test))]
    pub(crate) fn interrupt(&self, callback: Interrupt) -> Result<InterruptGuard, Error> {
        let mut state = self
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        check(&mut state)?;
        if state.interrupt.is_some() {
            return Err(error("request already has an active native operation"));
        }
        state.next_interrupt += 1;
        let id = state.next_interrupt;
        state.interrupt = Some((id, callback));
        Ok(InterruptGuard {
            control: self.clone(),
            id,
        })
    }
    /// Progress observation only; a registered hook is not proof of native termination.
    pub fn interrupt_armed(&self) -> bool {
        self.owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .interrupt
            .is_some()
    }
}

#[cfg(any(windows, feature = "cpu-onnx", test))]
pub(crate) struct InterruptGuard {
    control: RequestControl,
    id: u64,
}
#[cfg(any(windows, feature = "cpu-onnx", test))]
impl Drop for InterruptGuard {
    fn drop(&mut self) {
        let mut state = self
            .control
            .owner
            .inner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if state
            .interrupt
            .as_ref()
            .is_some_and(|(id, _)| *id == self.id)
        {
            state.interrupt = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn timer_interrupts_without_polling_and_drop_disarms() {
        let control = RequestControl::new(Duration::from_secs(2)).unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let notified = count.clone();
        let guard = control
            .interrupt(Arc::new(move || {
                notified.fetch_add(1, Ordering::SeqCst);
            }))
            .unwrap();
        control
            .limit_deadline(Instant::now() + Duration::from_millis(10))
            .unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while count.load(Ordering::SeqCst) == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(guard);
        assert!(!control.interrupt_armed());
        assert!(control.checkpoint().is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn finish_and_last_owner_release_timer_state() {
        let control = RequestControl::new(Duration::from_secs(300)).unwrap();
        let weak = Arc::downgrade(&control.owner.inner);
        control.finish();
        assert_eq!(control.cancel(), CancelResult::TooLate);
        drop(control);
        let end = Instant::now() + Duration::from_secs(2);
        while weak.upgrade().is_some() && Instant::now() < end {
            std::thread::yield_now();
        }
        assert!(weak.upgrade().is_none());
    }
}
