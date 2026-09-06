//! Scope-released timers for cordis (cordis v3 `@cordisjs/plugin-timer`).
//!
//! Install [`TimerPlugin`] once; every context that resolves the `timer`
//! service can then create timers that are **effects of the calling
//! fiber**: unloading the fiber cancels them automatically.
//!
//! ```no_run
//! use cordis::{Context, Disposer, Plugin, PluginError, PluginResult};
//! use cordis_timer::{TimerContextExt, TimerPlugin};
//! use serde_json::json;
//!
//! let ctx = Context::new();
//! ctx.plugin(TimerPlugin, json!(null)).unwrap();
//!
//! struct Worker;
//! impl Plugin for Worker {
//!     fn name(&self) -> Option<&str> { Some("worker") }
//!     fn apply(&self, ctx: &Context, _: &serde_json::Value) -> cordis::PluginResult {
//!         // Cancelled automatically when this fiber unloads.
//!         ctx.set_timeout(
//!             std::time::Duration::from_millis(100),
//!             || println!("late"),
//!         ).map_err(cordis::PluginError::new)?;
//!         Ok(())
//!     }
//! }
//! # let _ = ctx.plugin(Worker, json!(null)).unwrap();
//! ```
//!
//! The wheel runs on one background thread created at install time and
//! shut down when the `TimerPlugin` fiber is disposed.

#![deny(missing_docs)]

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cordis::{Context, Disposer, Plugin, PluginError, PluginResult};
use serde_json::Value;

/// The name under which [`TimerPlugin`] provides [`TimerWheel`].
pub const TIMER_SERVICE: &str = "timer";

struct Task {
    cancelled: AtomicBool,
    callback: Mutex<Option<Box<dyn FnMut() + Send>>>,
    interval: Option<Duration>,
    done: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

impl Task {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(sender) = self.done.lock().unwrap().take() {
            // Dropping the sender wakes a `sleep` waiting on this task.
            drop(sender);
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
            .name("cordis-timer".into())
            .spawn(move || thread_wheel.drive())
            .expect("cordis-timer: spawn wheel thread");
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
            // User code runs outside the lock.
            for task in due.drain(..) {
                if task.cancelled.load(Ordering::SeqCst) {
                    continue;
                }
                {
                    let mut guard = task.callback.lock().unwrap();
                    if let Some(callback) = guard.as_mut() {
                        callback();
                    }
                }
                if task.interval.is_none() {
                    // One-shot: release the callback and wake any sleeper.
                    task.callback.lock().unwrap().take();
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
    /// The timer wheel, when [`TimerPlugin`] is installed and active.
    fn timer_wheel(&self) -> Option<Arc<TimerWheel>>;

    /// Run `callback` once after `delay`; cancelled when the current
    /// fiber unloads. Returns a manual cancel handle.
    fn set_timeout<F: FnOnce() + Send + 'static>(
        &self,
        delay: Duration,
        callback: F,
    ) -> Result<CancelHandle, String>;

    /// Run `callback` every `period` until cancelled or the current
    /// fiber unloads.
    fn set_interval<F: Fn() + Send + Sync + 'static>(
        &self,
        period: Duration,
        callback: F,
    ) -> Result<CancelHandle, String>;

    /// Block the current thread for `delay`; returns early with
    /// [`SleepOutcome::Cancelled`] when the current fiber unloads.
    ///
    /// This blocks the *calling* thread — call it from worker threads,
    /// not from a plugin body on the install path. The sleep is an
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
            return Err("cordis-timer: timer service not available".into());
        };
        let mut callback = Some(callback);
        let task = Arc::new(Task {
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(Some(Box::new(move || {
                if let Some(callback) = callback.take() {
                    callback();
                }
            }))),
            interval: None,
            done: Mutex::new(None),
        });
        wheel.schedule(delay, task.clone());
        // The timer is an effect of this fiber: unload cancels it.
        let effect_task = task.clone();
        let effect_wheel = wheel.clone();
        self.effect("timer/timeout", move |_| {
            let disposer: Disposer = Box::new(move || {
                effect_task.cancel();
                effect_wheel.signal.notify_all();
            });
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        Ok(CancelHandle { task })
    }

    fn set_interval<F: Fn() + Send + Sync + 'static>(
        &self,
        period: Duration,
        callback: F,
    ) -> Result<CancelHandle, String> {
        let Some(wheel) = self.timer_wheel() else {
            return Err("cordis-timer: timer service not available".into());
        };
        let task = Arc::new(Task {
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(Some(Box::new(callback))),
            interval: Some(period),
            done: Mutex::new(None),
        });
        wheel.schedule(period, task.clone());
        let effect_task = task.clone();
        let effect_wheel = wheel.clone();
        self.effect("timer/interval", move |_| {
            let disposer: Disposer = Box::new(move || {
                effect_task.cancel();
                effect_wheel.signal.notify_all();
            });
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        Ok(CancelHandle { task })
    }

    fn sleep(&self, delay: Duration) -> Result<SleepOutcome, String> {
        let Some(wheel) = self.timer_wheel() else {
            return Err("cordis-timer: timer service not available".into());
        };
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        let task = Arc::new(Task {
            cancelled: AtomicBool::new(false),
            callback: Mutex::new(None),
            interval: None,
            done: Mutex::new(Some(sender)),
        });
        wheel.schedule(delay, task.clone());
        // Tie the sleep to this fiber as an effect: unloading the fiber
        // cancels the task, dropping the sender, which wakes the blocked
        // thread with [`SleepOutcome::Cancelled`].
        self.effect("timer/sleep", move |_| {
            let task = task.clone();
            let disposer: Disposer = Box::new(move || task.cancel());
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        let outcome = receiver
            .recv()
            .map(|_| SleepOutcome::Elapsed)
            .unwrap_or(SleepOutcome::Cancelled);
        Ok(outcome)
    }
}

/// Installs the timer wheel and provides it as the `timer` service.
pub struct TimerPlugin;

impl Plugin for TimerPlugin {
    fn name(&self) -> Option<&str> {
        Some("timer")
    }

    fn apply(&self, ctx: &Context, _config: &Value) -> PluginResult {
        let wheel = TimerWheel::new();
        ctx.provide(TIMER_SERVICE, wheel.clone())
            .map_err(PluginError::new)?;
        // Shut the wheel thread down when this fiber unloads.
        ctx.effect("timer/wheel", move |_| {
            let disposer: Disposer = Box::new(move || wheel.shutdown());
            Ok(Some(disposer))
        })
        .map_err(PluginError::new)?;
        Ok(())
    }
}
