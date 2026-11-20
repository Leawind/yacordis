//! Scope-released timers for cordis (cordis v3 `@cordisjs/component-timer`).
//!
//! Install [`TimerComponent`] once; every context that resolves the `timer`
//! service can then create timers that are **effects of the calling
//! fiber**: unloading the fiber cancels them automatically.
//!
//! ```no_run
//! use yacordis::{Context, Disposer, Component, ComponentError, ComponentResult};
//! use yacordis_timer::{TimerContextExt, TimerComponent};
//! use serde_json::json;
//!
//! let ctx = Context::new();
//! ctx.component(TimerComponent, json!(null)).unwrap();
//!
//! struct Worker;
//! impl Component for Worker {
//!     fn name(&self) -> Option<&str> { Some("worker") }
//!     fn apply(&self, ctx: &Context, _: &serde_json::Value) -> yacordis::ComponentResult {
//!         // Cancelled automatically when this fiber unloads.
//!         ctx.set_timeout(
//!             std::time::Duration::from_millis(100),
//!             || println!("late"),
//!         ).map_err(yacordis::ComponentError::new)?;
//!         Ok(())
//!     }
//! }
//! # let _ = ctx.component(Worker, json!(null)).unwrap();
//! ```
//!
//! The wheel runs on one background thread created at install time and
//! shut down when the `TimerComponent` fiber is disposed.

#![deny(missing_docs)]

use std::cell::Cell;
use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use yacordis::{Component, ComponentError, ComponentResult, Context, Disposer};

/// The name under which [`TimerComponent`] provides [`TimerWheel`].
pub const TIMER_SERVICE: &str = "timer";

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Extract the message from a caught panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

thread_local! {
    /// Id of the timer callback executing on this thread, if any. A drain
    /// targeting the task running on the calling thread must skip waiting
    /// (an unload triggered from inside a callback would wait for itself).
    static TIMER_CURRENT: Cell<Option<u64>> = const { Cell::new(None) };
}

struct Task {
    id: u64,
    cancelled: AtomicBool,
    callback: Mutex<Option<Box<dyn FnMut() + Send>>>,
    interval: Option<Duration>,
    done: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Whether the callback is executing on the wheel thread right now.
    /// Unload waits for it to return before rolling back
    /// (drain-before-rollback), so component code never runs
    /// concurrently with disposers.
    inflight: Mutex<bool>,
    inflight_signal: Condvar,
}

impl Task {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(sender) = self.done.lock().unwrap().take() {
            // Dropping the sender wakes a `sleep` waiting on this task.
            drop(sender);
        }
    }

    /// Wait until this task's callback is no longer executing. Skipped
    /// when the callback runs on the calling thread itself.
    fn drain(&self) {
        if TIMER_CURRENT.with(|cell| cell.get()) == Some(self.id) {
            return;
        }
        let mut inflight = self.inflight.lock().unwrap();
        while *inflight {
            inflight = self.inflight_signal.wait(inflight).unwrap();
        }
    }
}

struct Entry {
    deadline: Instant,
    seq: u64,
    task: Arc<Task>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // BinaryHeap is a max-heap: reverse so the earliest deadline pops first.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

struct WheelState {
    heap: BinaryHeap<Entry>,
    seq: u64,
    shutdown: bool,
}

/// The timer wheel: one background thread drives every registered timer.
pub struct TimerWheel {
    state: Mutex<WheelState>,
    signal: Condvar,
}

impl TimerWheel {
    fn new() -> Arc<Self> {
        let wheel = Arc::new(Self {
            state: Mutex::new(WheelState {
                heap: BinaryHeap::new(),
                seq: 0,
                shutdown: false,
            }),
            signal: Condvar::new(),
        });
        let thread_wheel = wheel.clone();
        std::thread::Builder::new()
            .name("yacordis-timer".into())
            .spawn(move || thread_wheel.drive())
            .expect("yacordis-timer: spawn wheel thread");
        wheel
    }

    fn drive(&self) {
        let mut due: Vec<Arc<Task>> = Vec::new();
        loop {
            {
                let mut state = self.state.lock().unwrap();
                loop {
                    if state.shutdown {
                        return;
                    }
                    let earliest = state.heap.peek().map(|entry| entry.deadline);
                    match earliest {
                        None => {
                            state = self.signal.wait(state).unwrap();
                        }
                        Some(deadline) => {
                            let now = Instant::now();
                            if deadline <= now {
                                break;
                            }
                            let (guard, _wait_result) =
                                self.signal.wait_timeout(state, deadline - now).unwrap();
                            state = guard;
                        }
                    }
                }
                while let Some(entry) = state.heap.peek() {
                    if entry.deadline > Instant::now() {
                        break;
                    }
                    let entry = state.heap.pop().expect("peeked");
                    if entry.task.cancelled.load(Ordering::SeqCst) {
                        continue;
                    }
                    if let Some(period) = entry.task.interval {
                        // Re-arm before running so the callback can
                        // observe periodicity even when it is slow.
                        let next = Entry {
                            deadline: Instant::now() + period,
                            seq: state.seq,
                            task: entry.task.clone(),
                        };
                        state.seq += 1;
                        state.heap.push(next);
                    }
                    due.push(entry.task.clone());
                }
            }
            // User code runs outside the lock, accounted as in flight so
            // unload can drain it before rolling back.
            for task in due.drain(..) {
                if task.cancelled.load(Ordering::SeqCst) {
                    continue;
                }
                TIMER_CURRENT.with(|cell| cell.set(Some(task.id)));
                *task.inflight.lock().unwrap() = true;
                // User code runs with no framework lock held: a callback
                // that panics must not poison the callback slot, and a
                // callback that unloads its own fiber must not deadlock
                // against `drain`. Panics are contained here — an escaped
                // one would kill the wheel thread and silence every timer.
                let mut callback = task.callback.lock().unwrap().take();
                let mut panicked = false;
                if let Some(callback) = callback.as_mut() {
                    let caught =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut *callback));
                    if let Err(payload) = caught {
                        panicked = true;
                        eprintln!(
                            "[yacordis-timer] timer callback panicked: {}",
                            panic_message(&payload)
                        );
                    }
                }
                if panicked {
                    // A broken callback would re-panic on every tick: stop
                    // the timer instead of repeating the same failure.
                    task.cancelled.store(true, Ordering::SeqCst);
                }
                // Interval timers keep their callback for the next tick;
                // one-shot timers hand it to the `done` waiter below.
                if task.interval.is_some() && !panicked {
                    *task.callback.lock().unwrap() = callback;
                }
                // Unconditional: a stuck in-flight flag would block the
                // unload drain forever.
                *task.inflight.lock().unwrap() = false;
                task.inflight_signal.notify_all();
                TIMER_CURRENT.with(|cell| cell.set(None));
                if task.interval.is_none() {
                    // One-shot: wake any sleeper. The callback itself was
                    // already consumed above, so its resources are released
                    // with this iteration's `callback`.
                    if let Some(sender) = task.done.lock().unwrap().take() {
                        let _ = sender.send(());
                    }
                }
            }
        }
    }

    fn schedule(&self, delay: Duration, task: Arc<Task>) {
        let mut state = self.state.lock().unwrap();
        if state.shutdown {
            // Wheel gone: cancel immediately so callers never see a
            // live-looking timer.
            task.cancel();
            return;
        }
        let seq = state.seq;
        state.seq += 1;
        state.heap.push(Entry {
            deadline: Instant::now() + delay,
            seq,
            task,
        });
        self.signal.notify_all();
    }

    fn shutdown(&self) {
        let mut state = self.state.lock().unwrap();
        state.shutdown = true;
        // Cancel every pending task (wakes sleepers), then wake the thread.
        while let Some(entry) = state.heap.pop() {
            entry.task.cancel();
        }
        self.signal.notify_all();
    }
}

impl Drop for TimerWheel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Handle that cancels a timer before it fires (idempotent).
#[derive(Clone)]
pub struct CancelHandle {
    task: Arc<Task>,
}

impl CancelHandle {
    /// Cancel the timer. Already-fired or cancelled timers ignore this.
    pub fn cancel(&self) {
        self.task.cancel();
    }

    /// Whether the timer has not fired and was not cancelled.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !self.task.cancelled.load(Ordering::SeqCst)
    }
}

/// Why a [`TimerContextExt::sleep`] returned early.
#[derive(Debug, PartialEq, Eq)]
pub enum SleepOutcome {
    /// The delay elapsed fully.
    Elapsed,
    /// The sleep was cancelled (fiber unload or explicit cancel).
    Cancelled,
}

/// Timer helpers for any context that can resolve the `timer` service.
pub trait TimerContextExt {
    /// The timer wheel, when [`TimerComponent`] is installed and active.
    fn timer_wheel(&self) -> Option<Arc<TimerWheel>>;

    /// Run `callback` once after `delay`; cancelled when the current
    /// fiber unloads. Unloading waits for an in-flight callback to
    /// return before rolling back, so the callback must not block
    /// indefinitely. Returns a manual cancel handle.
    fn set_timeout<F: FnOnce() + Send + 'static>(
        &self,
        delay: Duration,
        callback: F,
    ) -> Result<CancelHandle, String>;

    /// Run `callback` every `period` until cancelled or the current
    /// fiber unloads. Unloading waits for an in-flight callback to
    /// return before rolling back, so the callback must not block
    /// indefinitely.
    fn set_interval<F: Fn() + Send + Sync + 'static>(
        &self,
        period: Duration,
        callback: F,
    ) -> Result<CancelHandle, String>;

    /// Block the current thread for `delay`; returns early with
    /// [`SleepOutcome::Cancelled`] when the current fiber unloads.
    ///
    /// This blocks the *calling* thread — call it from worker threads,
    /// not from a component body on the install path. The sleep is an
    /// effect of the context's fiber, so it also wakes (as
    /// [`SleepOutcome::Cancelled`]) when that fiber is disposed.
    fn sleep(&self, delay: Duration) -> Result<SleepOutcome, String>;
}

impl TimerContextExt for Context {
    fn timer_wheel(&self) -> Option<Arc<TimerWheel>> {
        self.get::<TimerWheel>(TIMER_SERVICE)
    }

    fn set_timeout<F: FnOnce() + Send + 'static>(
        &self,
        delay: Duration,
        callback: F,
    ) -> Result<CancelHandle, String> {
        let Some(wheel) = self.timer_wheel() else {
            return Err("yacordis-timer: timer service not available".into());
        };
        let mut callback = Some(callback);
        let task = Arc::new(Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(Some(Box::new(move || {
                if let Some(callback) = callback.take() {
                    callback();
                }
            }))),
            interval: None,
            done: Mutex::new(None),
            inflight: Mutex::new(false),
            inflight_signal: Condvar::new(),
        });
        // The effect comes first: a failing registration (the fiber going
        // away) leaves nothing scheduled. The disposer cancels pending
        // firings and waits for an in-flight callback to return before the
        // rollback continues — no component code runs concurrently with
        // disposers.
        let effect_task = task.clone();
        self.effect("timer/timeout", move |_| {
            let disposer: Disposer = Box::new(move || {
                effect_task.cancel();
                effect_task.drain();
            });
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        wheel.schedule(delay, task.clone());
        Ok(CancelHandle { task })
    }

    fn set_interval<F: Fn() + Send + Sync + 'static>(
        &self,
        period: Duration,
        callback: F,
    ) -> Result<CancelHandle, String> {
        let Some(wheel) = self.timer_wheel() else {
            return Err("yacordis-timer: timer service not available".into());
        };
        let task = Arc::new(Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(Some(Box::new(callback))),
            interval: Some(period),
            done: Mutex::new(None),
            inflight: Mutex::new(false),
            inflight_signal: Condvar::new(),
        });
        // Same discipline as `set_timeout`: effect first, schedule after.
        let effect_task = task.clone();
        self.effect("timer/interval", move |_| {
            let disposer: Disposer = Box::new(move || {
                effect_task.cancel();
                effect_task.drain();
            });
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        wheel.schedule(period, task.clone());
        Ok(CancelHandle { task })
    }

    fn sleep(&self, delay: Duration) -> Result<SleepOutcome, String> {
        let Some(wheel) = self.timer_wheel() else {
            return Err("yacordis-timer: timer service not available".into());
        };
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        let task = Arc::new(Task {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(None),
            interval: None,
            done: Mutex::new(Some(sender)),
            inflight: Mutex::new(false),
            inflight_signal: Condvar::new(),
        });
        // Tie the sleep to this fiber as an effect: unloading the fiber
        // cancels the task, dropping the sender, which wakes the blocked
        // thread with [`SleepOutcome::Cancelled`]. Same discipline as the
        // other two forms: effect first, schedule after, so a failing
        // registration leaves nothing pending on the wheel.
        let effect_task = task.clone();
        self.effect("timer/sleep", move |_| {
            let effect_task = effect_task.clone();
            let disposer: Disposer = Box::new(move || effect_task.cancel());
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        wheel.schedule(delay, task.clone());
        let outcome = receiver
            .recv()
            .map(|_| SleepOutcome::Elapsed)
            .unwrap_or(SleepOutcome::Cancelled);
        Ok(outcome)
    }
}

/// Installs the timer wheel and provides it as the `timer` service.
pub struct TimerComponent;

impl Component for TimerComponent {
    fn name(&self) -> Option<&str> {
        Some("timer")
    }

    fn apply(&self, ctx: &Context, _config: &Value) -> ComponentResult {
        let wheel = TimerWheel::new();
        ctx.provide(TIMER_SERVICE, wheel.clone())
            .map_err(ComponentError::new)?;
        // Shut the wheel thread down when this fiber unloads.
        ctx.effect("timer/wheel", move |_| {
            let disposer: Disposer = Box::new(move || wheel.shutdown());
            Ok(Some(disposer))
        })
        .map_err(ComponentError::new)?;
        Ok(())
    }
}
