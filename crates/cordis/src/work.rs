//! Framework-mediated concurrency (§4.1 I1: async as plumbing, never a
//! second scheduling domain).
//!
//! Two work-unit forms, both fully owned by the framework:
//!
//! - [`Context::run_blocking`]: `work` runs on a dedicated framework
//!   thread. It receives no `Context` — mid-flight framework mutation
//!   would race rollback — only a cooperative [`CancellationToken`].
//! - [`Context::spawn_future`]: the future is polled on the framework's
//!   per-`Core` executor thread; abort takes effect at the next await
//!   point (a poll that never yields cannot be interrupted — the same
//!   honesty as `run_blocking` ignoring its token).
//!
//! Completion is delivered by invoking `on_done(&Context, outcome)` on
//! the thread that finished the work (worker or executor). That call is
//! framework-initiated, so it may use the full context API (emit,
//! provide, install...) — the same reentrancy contract as FFI
//! callbacks. If the fiber is already gone, completion is dropped.
//!
//! Lifecycle integration is the drain-before-rollback protocol: every
//! work unit is registered both as an effect of its fiber (scope
//! lifetime) and in the fiber's work list. `unload` (and the
//! failed-body rollback) first cancels and joins every in-flight unit,
//! then disposes effects — no plugin code runs concurrently with
//! rollback. `join` skips the unit executing on the current thread, so
//! an `on_done` that disposes its own fiber cannot self-deadlock.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::task::{Context as TaskContext, Poll, Wake, Waker};
use std::thread;

use serde_json::Value;

use crate::context::{Context, Core, IsolateMap};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::{CordisError, PluginError};
use crate::fiber::Fiber;
use crate::logger::LogLevel;

/// Cooperative cancellation token handed to a blocking work unit. The
/// unit must poll it for timely unloads; a unit that ignores it holds
/// the unloading fiber until it returns.
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Whether unload or an explicit cancel has requested this unit to
    /// stop.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Outcome of a work unit, delivered to the completion callback.
pub enum WorkOutcome {
    /// The work returned a value.
    Done(Value),
    /// Cancelled by unload/dispose (or explicit cancel) before
    /// finishing.
    Cancelled,
    /// The work returned an error.
    Failed(PluginError),
    /// The work panicked; the payload message is carried as an error.
    Panicked(String),
}

/// Handle for a registered work unit. Cancel is idempotent; the unit
/// also cancels when its fiber unloads or its enclosing effect is
/// disposed.
pub struct WorkHandle {
    unit: Arc<WorkUnit>,
}

impl WorkHandle {
    /// Request cancellation. Blocking work observes it through its
    /// token; futures are dropped at their next await point.
    pub fn cancel(&self) {
        self.unit.cancel();
    }

    /// Whether the unit has finished (including its completion
    /// callback).
    pub fn is_finished(&self) -> bool {
        *self.unit.done.lock().unwrap()
    }
}

pub(crate) struct WorkUnit {
    pub id: u64,
    label: String,
    fiber: Weak<Fiber>,
    core: Arc<Core>,
    isolate: Arc<IsolateMap>,
    cancelled: Arc<AtomicBool>,
    /// Set (and announced) after the completion callback returned. Held
    /// across the callback, so `join` waits for plugin code to finish.
    done: Mutex<bool>,
    signal: Condvar,
    on_done: Mutex<Option<Box<dyn FnOnce(&Context, WorkOutcome) + Send>>>,
    /// Executor plumbing for futures: the shared queue state and the
    /// task carrying the future (weak: the executor owns it).
    exec: Option<Arc<ExecInner>>,
    task: Mutex<Weak<ExecTask>>,
}

impl WorkUnit {
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(exec) = &self.exec {
            if let Some(task) = self.task.lock().unwrap().upgrade() {
                task.aborted.store(true, Ordering::Release);
            }
            exec.abort();
        }
    }

    /// Wait until the unit finished (completion callback included).
    /// Skipped for the unit currently executing on this thread (an
    /// `on_done` disposing its own fiber would otherwise wait for
    /// itself).
    pub(crate) fn join(&self) {
        if WORK_CURRENT.with(|c| c.get()) == Some(self.id) {
            return;
        }
        let mut done = self.done.lock().unwrap();
        while !*done {
            done = self.signal.wait(done).unwrap();
        }
    }
}

thread_local! {
    /// Id of the work unit executing on this thread, if any.
    static WORK_CURRENT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Run one completion step: build the context (dropping completion when
/// the fiber is gone), invoke the callback outside panics, then mark
/// done. The done flag is held across the callback, so concurrent
/// `join`s wait for plugin code to finish and double delivery is
/// impossible. Runs on the worker/executor thread.
fn deliver(unit: &Arc<WorkUnit>, outcome: WorkOutcome) {
    let mut done = unit.done.lock().unwrap();
    if *done {
        return;
    }
    if let Some(fiber) = unit.fiber.upgrade() {
        let ctx = Context {
            core: unit.core.clone(),
            fiber: unit.fiber.clone(),
            isolate: unit.isolate.clone(),
        };
        let _fiber_guard = fiber; // keep the fiber alive across the callback
        // Mark this thread as executing the unit's completion: a drain
        // targeting this fiber must skip joining it (thread workers and
        // the shared executor thread alike).
        WORK_CURRENT.with(|c| c.set(Some(unit.id)));
        let callback = unit.on_done.lock().unwrap().take();
        if let Some(callback) = callback {
            let result = catch_unwind(AssertUnwindSafe(|| callback(&ctx, outcome)));
            if let Err(payload) = result {
                let message = crate::effect::panic_message(&payload);
                unit.core.logger.log(
                    LogLevel::Error,
                    "cordis/work",
                    &format!("work completion for `{}` panicked: {message}", unit.label),
                );
            }
        }
        WORK_CURRENT.with(|c| c.set(None));
    }
    *done = true;
    drop(done);
    unit.signal.notify_all();
}

fn run_worker(unit: Arc<WorkUnit>, work: Box<dyn FnOnce(&CancellationToken) -> Result<Value, PluginError> + Send>) {
    let token = CancellationToken(unit.cancelled.clone());
    let outcome = match catch_unwind(AssertUnwindSafe(|| work(&token))) {
        Ok(Ok(value)) => {
            if unit.cancelled.load(Ordering::Acquire) {
                WorkOutcome::Cancelled
            } else {
                WorkOutcome::Done(value)
            }
        }
        Ok(Err(err)) => WorkOutcome::Failed(err),
        Err(payload) => WorkOutcome::Panicked(crate::effect::panic_message(&payload)),
    };
    deliver(&unit, outcome);
}

impl Context {
    /// Run `work` on a dedicated framework thread. The unit is an effect
    /// of the current fiber: unload cancels and joins it before rolling
    /// back. `on_done` runs on the worker thread as a framework-initiated
    /// call; it is dropped when the fiber is already disposed.
    pub fn run_blocking<W, D>(
        &self,
        label: impl Into<String>,
        work: W,
        on_done: D,
    ) -> Result<WorkHandle, CordisError>
    where
        W: FnOnce(&CancellationToken) -> Result<Value, PluginError> + Send + 'static,
        D: FnOnce(&Context, WorkOutcome) + Send + 'static,
    {
        let fiber = self.active_fiber("start blocking work")?;
        let unit = self.register_work(label.into(), fiber, on_done, None)?;
        let worker_unit = unit.unit.clone();
        thread::spawn(move || run_worker(worker_unit, Box::new(work)));
        Ok(unit)
    }

    /// Spawn a future on the framework's executor thread (one per
    /// `Core`, started lazily, alive until process exit). The unit is an
    /// effect of the current fiber with the same drain semantics as
    /// [`Context::run_blocking`]; abort drops the future at its next
    /// await point.
    pub fn spawn_future<Fut, D>(
        &self,
        label: impl Into<String>,
        future: Fut,
        on_done: D,
    ) -> Result<WorkHandle, CordisError>
    where
        Fut: Future<Output = Result<Value, PluginError>> + Send + 'static,
        D: FnOnce(&Context, WorkOutcome) + Send + 'static,
    {
        let fiber = self.active_fiber("spawn a future")?;
        let exec = self
            .core
            .work
            .exec
            .get_or_init(|| Executor::start(self.core.clone()))
            .inner
            .clone();
        let unit = self.register_work(label.into(), fiber, on_done, Some(exec.clone()))?;
        let task = ExecTask::new(future, unit.unit.clone());
        *unit.unit.task.lock().unwrap() = Arc::downgrade(&task);
        exec.submit(task);
        Ok(unit)
    }

    fn register_work<D>(
        &self,
        label: String,
        fiber: Arc<Fiber>,
        on_done: D,
        exec: Option<Arc<ExecInner>>,
    ) -> Result<WorkHandle, CordisError>
    where
        D: FnOnce(&Context, WorkOutcome) + Send + 'static,
    {
        let id = self.core.work.next_work_id();
        let unit = Arc::new(WorkUnit {
            id,
            label,
            fiber: Arc::downgrade(&fiber),
            core: self.core.clone(),
            isolate: self.isolate.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            done: Mutex::new(false),
            signal: Condvar::new(),
            on_done: Mutex::new(Some(Box::new(on_done))),
            exec,
            task: Mutex::new(Weak::new()),
        });

        // Scope lifetime: an effect so enclosing batches (and the plain
        // fiber unload) dispose it like any other registration.
        let handle = EffectHandle::new(format!("work/{}", unit.label));
        let dispose_unit = unit.clone();
        handle.push(Box::new(move || {
            dispose_unit.cancel();
            dispose_unit.join();
            fiber_remove_work(&dispose_unit);
        }));
        attach_effect(&fiber, handle);

        // Drain list: unload joins these before rolling back.
        fiber.inner.lock().unwrap().works.push(unit.clone());

        Ok(WorkHandle { unit })
    }
}

fn fiber_remove_work(unit: &Arc<WorkUnit>) {
    if let Some(fiber) = unit.fiber.upgrade() {
        let mut inner = fiber.inner.lock().unwrap();
        inner.works.retain(|entry| entry.id != unit.id);
    }
}

// ---------------------------------------------------------------------
// Fiber integration points (called from fiber.rs)
// ---------------------------------------------------------------------

/// Drain-before-rollback: cancel and join every in-flight work unit of
/// the fiber. Skips the unit running on the current thread.
pub(crate) fn drain_fiber_works(fiber: &Fiber) {
    let units: Vec<Arc<WorkUnit>> = {
        let mut inner = fiber.inner.lock().unwrap();
        std::mem::take(&mut inner.works)
    };
    for unit in units {
        unit.cancel();
        unit.join();
    }
}

// ---------------------------------------------------------------------
// Single-threaded executor for spawned futures
// ---------------------------------------------------------------------

type FutureBox = Pin<Box<dyn Future<Output = Result<Value, PluginError>> + Send>>;

struct ExecTask {
    future: Mutex<Option<FutureBox>>,
    unit: Arc<WorkUnit>,
    aborted: AtomicBool,
}

impl ExecTask {
    fn new(future: impl Future<Output = Result<Value, PluginError>> + Send + 'static, unit: Arc<WorkUnit>) -> Arc<ExecTask> {
        Arc::new(ExecTask {
            future: Mutex::new(Some(Box::pin(future))),
            unit,
            aborted: AtomicBool::new(false),
        })
    }
}

impl Wake for ExecTask {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let exec = self.unit.exec.as_ref().expect("executor task carries its executor");
        let mut g = exec.ready.lock().unwrap();
        g.queue.push_back(self.clone());
        exec.signal.notify_all();
    }
}

pub(crate) struct ExecInner {
    ready: Mutex<ExecState>,
    signal: Condvar,
}

struct ExecState {
    queue: VecDeque<Arc<ExecTask>>,
    /// Tasks that yielded `Pending`: parked until their waker re-queues
    /// them, and reachable by the abort path (a queued task can be
    /// checked when popped, but a parked one would otherwise vanish
    /// from the executor's view).
    inflight: HashMap<u64, Arc<ExecTask>>,
}

impl ExecInner {
    fn submit(&self, task: Arc<ExecTask>) {
        let mut g = self.ready.lock().unwrap();
        g.queue.push_back(task);
        self.signal.notify_all();
    }

    /// Signal an abort; the executor drops the future at the task's next
    /// scheduling point (a poll in progress cannot be interrupted).
    fn abort(&self) {
        self.signal.notify_all();
    }
}

pub(crate) struct Executor {
    inner: Arc<ExecInner>,
}

impl Executor {
    /// Start the executor thread. It holds an `Arc<Core>`, so the core
    /// (and the executor) live until process exit — the same
    /// keep-mapped honesty as loaded plugin code.
    fn start(core: Arc<Core>) -> Executor {
        let inner = Arc::new(ExecInner {
            ready: Mutex::new(ExecState {
                queue: VecDeque::new(),
                inflight: HashMap::new(),
            }),
            signal: Condvar::new(),
        });
        let thread_inner = inner.clone();
        thread::Builder::new()
            .name("cordis-executor".into())
            .spawn(move || executor_loop(core, thread_inner))
            .expect("start executor thread");
        Executor { inner }
    }
}

fn executor_loop(_core: Arc<Core>, exec: Arc<ExecInner>) {
    loop {
        // 1. Finish aborted parked tasks (callbacks run outside locks).
        let aborted: Vec<Arc<ExecTask>> = {
            let mut g = exec.ready.lock().unwrap();
            let keys: Vec<u64> = g
                .inflight
                .iter()
                .filter(|(_, t)| t.aborted.load(Ordering::Acquire))
                .map(|(id, _)| *id)
                .collect();
            keys.into_iter().filter_map(|id| g.inflight.remove(&id)).collect()
        };
        for task in aborted {
            finish_aborted(&task);
        }

        // 2. Poll the next queued task, if any.
        let task = {
            let mut g = exec.ready.lock().unwrap();
            g.queue.pop_front()
        };
        if let Some(task) = task {
            if task.aborted.load(Ordering::Acquire) {
                finish_aborted(&task);
                continue;
            }
            let waker = Waker::from(task.clone());
            let mut cx = TaskContext::from_waker(&waker);
            let poll = {
                let mut guard = task.future.lock().unwrap();
                match guard.as_mut() {
                    Some(future) => future.as_mut().poll(&mut cx),
                    None => Poll::Ready(Err(PluginError::new("future already consumed"))),
                }
            };
            match poll {
                Poll::Ready(outcome) => {
                    // Consume the future: a stray duplicate re-poll
                    // (multiple wakes) must not resume it.
                    *task.future.lock().unwrap() = None;
                    {
                        let mut g = exec.ready.lock().unwrap();
                        g.inflight.remove(&task.unit.id);
                    }
                    let outcome = if task.unit.cancelled.load(Ordering::Acquire) {
                        WorkOutcome::Cancelled
                    } else {
                        match outcome {
                            Ok(value) => WorkOutcome::Done(value),
                            Err(err) => WorkOutcome::Failed(err),
                        }
                    };
                    deliver(&task.unit, outcome);
                }
                Poll::Pending => {
                    let mut g = exec.ready.lock().unwrap();
                    g.inflight.insert(task.unit.id, task);
                }
            }
            continue;
        }

        // 3. Park until a waker or submitter wakes us.
        let mut g = exec.ready.lock().unwrap();
        if g.queue.is_empty() && !g.inflight.values().any(|t| t.aborted.load(Ordering::Acquire)) {
            g = exec.signal.wait(g).unwrap();
        }
    }
}

fn finish_aborted(task: &Arc<ExecTask>) {
    // Dropping the future releases its resources; the unit's completion
    // callback observes the cancellation.
    *task.future.lock().unwrap() = None;
    deliver(&task.unit, WorkOutcome::Cancelled);
}

// ---------------------------------------------------------------------
// Core wiring
// ---------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct WorkRegistry {
    pub sn: AtomicU64,
    pub exec: OnceLock<Executor>,
}

impl WorkRegistry {
    pub fn next_work_id(&self) -> u64 {
        self.sn.fetch_add(1, Ordering::Relaxed)
    }
}
