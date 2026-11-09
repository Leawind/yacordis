//! Fibers: the lifecycle of one installed plugin instance.
//!
//! Mirrors cordis v4's `Fiber`: a fiber owns the effects created by its
//! plugin body, tracks its required dependencies, and derives an epoch
//! (the list of dependency-owner fiber ids) that drives automatic
//! activation and deactivation. With a synchronous core the v4
//! LOADING/UNLOADING states collapse into direct transitions between
//! `Pending`, `Active`, `Failed` and `Disposed`.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};

use serde_json::{Value, json};

use crate::context::{Context, Core};
use crate::effect::{EffectHandle, ScopeGuard, run_plugin_apply};
use crate::error::{CordisError, PluginError};
use crate::events;
use crate::logger::LogLevel;
use crate::plugin::{Inject, Plugin};

/// Lifecycle state of a fiber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FiberState {
    /// Installed but not running: some required service is missing.
    Pending,
    /// The plugin body ran and its effects are live.
    Active,
    /// The plugin body failed; effects were rolled back. Recoverable
    /// through `update()`.
    Failed,
    /// Disposed: effects reverted, deregistered from the registry.
    Disposed,
}

impl FiberState {
    /// Lowercase name used in `internal/status` payloads.
    pub fn as_str(&self) -> &'static str {
        match self {
            FiberState::Pending => "pending",
            FiberState::Active => "active",
            FiberState::Failed => "failed",
            FiberState::Disposed => "disposed",
        }
    }
}

/// Per-fiber lifecycle tracing for post-mortem diagnostics (stress
/// tests). Lock discipline: this module never locks `Fiber::inner` —
/// callers may already hold it, and the mutex is not re-entrant.
pub(crate) mod lifecycle_trace {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    const CAP_PER_FIBER: usize = 64;
    const MAX_FIBERS: usize = 4096;

    type Ring = Mutex<(HashMap<u64, Vec<String>>, usize)>;

    fn ring() -> &'static Ring {
        static RING: OnceLock<Ring> = OnceLock::new();
        RING.get_or_init(|| Mutex::new((HashMap::new(), 0)))
    }

    pub(crate) fn trace(event: &str, fiber: &super::Fiber) {
        let tid = std::thread::current().id();
        let line = format!("T{tid:?} {event}");
        let mut guard = ring().lock().unwrap();
        let (map, total) = &mut *guard;
        if *total >= MAX_FIBERS * 2 {
            return; // saturated; post-mortem covers the earliest fibers only
        }
        let entry = map.entry(fiber.id).or_default();
        if entry.len() >= CAP_PER_FIBER {
            entry.remove(0);
        }
        entry.push(line);
        *total += 1;
    }

    pub(crate) fn dump_fiber(id: u64) -> Vec<String> {
        ring()
            .lock()
            .unwrap()
            .0
            .get(&id)
            .cloned()
            .unwrap_or_default()
    }
}

/// A service implementation registered by a fiber.
pub(crate) struct ServiceImpl {
    pub name: String,
    pub value: Mutex<Arc<dyn Any + Send + Sync>>,
    pub owner: Weak<Fiber>,
    pub owner_name: String,
    pub label: crate::context::IsolateLabel,
}

impl ServiceImpl {
    pub fn get_value(&self) -> Arc<dyn Any + Send + Sync> {
        self.value.lock().unwrap().clone()
    }
}

pub(crate) struct FiberInner {
    pub state: FiberState,
    pub config: Value,
    /// Dependency signature: owner fiber ids of required deps in
    /// declaration order, or `None` when a required dep is missing.
    pub epoch: Option<Vec<u64>>,
    pub error: Option<PluginError>,
    /// The plugin this fiber runs. Held under the inner lock so
    /// `replace_plugin` can swap it against concurrent `reload` reads.
    pub plugin: Option<Arc<dyn Plugin>>,
    /// Declared dependencies, cached from the plugin (re-cached by
    /// `replace_plugin`).
    pub inject: Inject,
    /// Declared provides (`Plugin::provides`), used by dependency-cycle
    /// diagnostics (re-cached by `replace_plugin`).
    pub provides: Vec<String>,
    /// Checked dependency impls, keyed by name (labels are fixed per
    /// fiber, so the name suffices within one fiber).
    pub dep_store: HashMap<String, Arc<ServiceImpl>>,
    /// Effects owned by this fiber, disposed in reverse order.
    pub batches: Vec<EffectHandle>,
    /// Services provided during the current body run, notified on
    /// activation.
    pub provided: Vec<(String, crate::context::IsolateLabel)>,
    /// In-flight framework work units (`run_blocking`/`spawn_future`);
    /// cancelled and joined before effects are rolled back.
    pub works: Vec<Arc<crate::work::WorkUnit>>,
    pub unloading: bool,
    pub disposed: bool,
}

pub(crate) static PENDING_OPS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

pub(crate) fn pending_ops() -> i64 {
    PENDING_OPS.load(Ordering::SeqCst)
}

pub(crate) struct Fiber {
    pub id: u64,
    pub name: String,
    #[allow(dead_code)] // parent link used by future scope introspection
    pub parent: Option<Weak<Fiber>>,
    pub runtime: RwLock<Option<Arc<crate::registry::Runtime>>>,
    pub self_weak: OnceLock<Weak<Fiber>>,
    pub ctx: OnceLock<Context>,
    pub inner: Mutex<FiberInner>,
    /// Serializes lifecycle ops on this fiber. `busy` + the deferred queue
    /// live under one lock so a racer can never push an op after the
    /// holder released the flag without it being drained (lost wakeup).
    schedule: Mutex<ScheduleState>,
    /// The isolation group map this fiber was installed under.
    #[allow(dead_code)] // read via contexts; kept for scope introspection
    pub(crate) isolate: Arc<crate::context::IsolateMap>,
    /// Service intercept overrides (§2.7): name → JSON value, merged
    /// nearest-fiber-first over the provided base at resolution time.
    /// Entries are effects; unload removes them.
    pub(crate) intercepts: Arc<Mutex<HashMap<String, Value>>>,
}

#[derive(Default)]
struct ScheduleState {
    busy: bool,
    queue: Vec<Op>,
}

pub(crate) struct OpGuard;

impl Drop for OpGuard {
    fn drop(&mut self) {
        EXECUTING_OPS.with(|c| c.set(c.get().saturating_sub(1)));
        PENDING_OPS.fetch_sub(1, Ordering::SeqCst);
    }
}

thread_local! {
    static EXECUTING_OPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether this thread is currently inside a lifecycle op. A `quiesce`
/// called from within an op (cascading dispose inside a disposer) would
/// wait for itself — callers skip it in that case; the outer op's
/// completion is what makes the cascade observable.
pub(crate) fn executing_op_on_this_thread() -> bool {
    EXECUTING_OPS.with(|c| c.get() > 0)
}

fn op_epoch(op: &Op) -> &Option<Vec<u64>> {
    match op {
        Op::Reload { epoch } => epoch,
        Op::Unload { epoch, .. } => epoch,
    }
}

pub(crate) enum Op {
    Reload {
        /// The epoch this op was scheduled against; dropped as stale when
        /// the fiber's epoch has moved on by the time the op runs.
        epoch: Option<Vec<u64>>,
    },
    Unload {
        keep_alive: bool,
        /// See `Reload::epoch`.
        epoch: Option<Vec<u64>>,
    },
}

fn fresh_inner(
    state: FiberState,
    config: Value,
    plugin: Option<Arc<dyn Plugin>>,
    inject: Inject,
    provides: Vec<String>,
) -> FiberInner {
    FiberInner {
        state,
        config,
        epoch: if matches!(state, FiberState::Active) {
            Some(Vec::new())
        } else {
            None
        },
        error: None,
        plugin,
        inject,
        provides,
        dep_store: HashMap::new(),
        batches: Vec::new(),
        provided: Vec::new(),
        works: Vec::new(),
        unloading: false,
        disposed: false,
    }
}

impl Fiber {
    pub(crate) fn new_root() -> Arc<Fiber> {
        Arc::new_cyclic(|weak| Fiber {
            id: 0,
            name: "root".into(),
            parent: None,
            runtime: RwLock::new(None),
            self_weak: OnceLock::from(weak.clone()),
            ctx: OnceLock::new(),
            inner: Mutex::new(fresh_inner(
                FiberState::Active,
                Value::Null,
                None,
                Inject::none(),
                Vec::new(),
            )),
            schedule: Mutex::new(ScheduleState::default()),
            isolate: Arc::new(crate::context::IsolateMap::default()),
            intercepts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(crate) fn install(
        core: &Arc<Core>,
        isolate: Arc<crate::context::IsolateMap>,
        parent: Weak<Fiber>,
        plugin: Arc<dyn Plugin>,
        config: Value,
        name: String,
    ) -> Arc<Fiber> {
        let inject = plugin.inject();
        let provides = plugin.provides();
        Arc::new_cyclic(|weak| Fiber {
            id: core.next_fiber_id(),
            name,
            parent: Some(parent),
            runtime: RwLock::new(None),
            self_weak: OnceLock::from(weak.clone()),
            ctx: OnceLock::new(),
            inner: Mutex::new(fresh_inner(
                FiberState::Pending,
                config,
                Some(plugin),
                inject,
                provides,
            )),
            schedule: Mutex::new(ScheduleState::default()),
            isolate,
            intercepts: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    #[allow(dead_code)] // used by diagnostics and tests
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn ctx(&self) -> &Context {
        self.ctx
            .get()
            .expect("fiber context initialized after install")
    }

    pub(crate) fn strong(&self) -> Arc<Fiber> {
        self.self_weak
            .get()
            .and_then(Weak::upgrade)
            .expect("fiber Arc alive while operating on it")
    }

    pub(crate) fn runtime(&self) -> Option<Arc<crate::registry::Runtime>> {
        self.runtime.read().unwrap().clone()
    }

    pub(crate) fn state(&self) -> FiberState {
        self.inner.lock().unwrap().state
    }

    /// Record a service provided from this fiber so activation notifies
    /// its dependents (services provided while already active notify
    /// immediately from `provide` itself).
    pub(crate) fn note_provided(&self, name: &str, label: crate::context::IsolateLabel) {
        let mut inner = self.inner.lock().unwrap();
        if inner.state == FiberState::Pending {
            inner.provided.push((name.to_owned(), label));
        }
    }

    pub(crate) fn refresh_ops(&self) -> Vec<(Arc<Fiber>, Op)> {
        self.refresh()
    }

    pub(crate) fn summary(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        json!({ "id": self.id, "name": self.name, "state": inner.state.as_str() })
    }

    /// Append an effect batch to this fiber's effect list. If the fiber is
    /// going away, the effect is reverted immediately instead of leaking.
    pub(crate) fn push_effect_batch(&self, handle: EffectHandle) {
        let revert_now = {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed || inner.unloading {
                true
            } else {
                inner.batches.push(handle.clone());
                false
            }
        };
        if revert_now {
            handle.dispose();
        }
    }

    fn set_state(&self, state: FiberState) -> Option<FiberState> {
        let mut inner = self.inner.lock().unwrap();
        let old = inner.state;
        if old == state {
            return None;
        }
        inner.state = state;
        Some(old)
    }

    fn log(&self, core: &Arc<Core>, level: LogLevel, message: &str) {
        core.logger
            .log(level, &format!("cordis/{}", self.name), message);
    }

    /// All declared inject names (required and optional), in order.
    /// Cached at install time and re-cached by `replace_plugin`;
    /// `Plugin::inject` is never re-queried outside those two points.
    pub(crate) fn inject_names(&self) -> Vec<(String, bool)> {
        self.inner.lock().unwrap().inject.entries.clone()
    }

    /// Service names declared via `Plugin::provides`, in order. Read by
    /// dependency-cycle diagnostics.
    pub(crate) fn declared_provides(&self) -> Vec<String> {
        self.inner.lock().unwrap().provides.clone()
    }

    /// Re-read the given services (within this fiber's isolation group)
    /// from the global store into `dep_store`. Returns true if any
    /// relevant entry changed.
    pub(crate) fn check_impls(&self, core: &Arc<Core>, names: &[String]) -> bool {
        let inject: Vec<String> = self.inject_names().iter().map(|(n, _)| n.clone()).collect();
        let mut changed = false;
        let mut inner = self.inner.lock().unwrap();
        for name in names {
            if !inject.iter().any(|n| n == name) {
                continue;
            }
            let label = self.ctx().isolate_label(name);
            let fresh = core.store_impl(name, &label);
            match (inner.dep_store.get(name), fresh) {
                (Some(old), Some(new)) if Arc::ptr_eq(old, &new) => {}
                (_, Some(new)) => {
                    inner.dep_store.insert(name.clone(), new);
                    changed = true;
                }
                (Some(_), None) => {
                    inner.dep_store.remove(name);
                    changed = true;
                }
                (None, None) => {}
            }
        }
        changed
    }

    /// Recompute the epoch; returns ops to drive when it changed.
    fn refresh(&self) -> Vec<(Arc<Fiber>, Op)> {
        let mut inner = self.inner.lock().unwrap();
        if inner.disposed || inner.error.is_some() {
            return Vec::new();
        }
        let required: Vec<String> = inner
            .inject
            .required_names()
            .map(str::to_owned)
            .collect();
        let new_epoch = if required.iter().all(|n| inner.dep_store.contains_key(n)) {
            Some(
                required
                    .iter()
                    .map(|n| {
                        inner.dep_store[n]
                            .owner
                            .upgrade()
                            .map(|f| f.id)
                            .unwrap_or(u64::MAX)
                    })
                    .collect::<Vec<u64>>(),
            )
        } else {
            None
        };
        let old_epoch = inner.epoch.clone();
        let changed = new_epoch != old_epoch;
        lifecycle_trace::trace(
            &format!("refresh {old_epoch:?} -> {new_epoch:?} changed={changed}"),
            self,
        );
        if !changed {
            return Vec::new();
        }
        inner.epoch = new_epoch.clone();
        let epoch = new_epoch.clone();
        match (old_epoch, new_epoch) {
            (None, Some(_)) => vec![(self.strong(), Op::Reload { epoch })],
            (Some(_), None) => vec![(
                self.strong(),
                Op::Unload {
                    keep_alive: true,
                    epoch: None,
                },
            )],
            (Some(_), Some(_)) => vec![
                (
                    self.strong(),
                    Op::Unload {
                        keep_alive: true,
                        epoch: None,
                    },
                ),
                (self.strong(), Op::Reload { epoch }),
            ],
            (None, None) => Vec::new(),
        }
    }

    /// The install-time effect body: announce, check deps, and activate
    /// when possible. The fiber must already be registered in its runtime.
    pub(crate) fn run_install_effect(&self, core: &Arc<Core>) {
        events::emit_plugin(core, self);
        let names: Vec<String> = self.inject_names().iter().map(|(n, _)| n.clone()).collect();
        self.check_impls(core, &names);
        let ops = self.refresh();
        drive_ops(ops);
        // Synchronous install contract: `ctx.plugin()` must not return
        // while this fiber's initial (or externally triggered) transition
        // is still in flight on another thread. Skipped when this install
        // runs inside another fiber's op (cascading install in a plugin
        // body) — waiting there would wait for our own outer op.
        if !executing_op_on_this_thread() {
            core.quiesce();
        }
        // A fresh install can complete a declared-provides cycle (nothing
        // was provided or withdrawn, so no notify fires).
        crate::diagnose::report_cycles(core);
    }

    fn reload(&self, core: &Arc<Core>) {
        // Re-validate the dependency snapshot immediately before executing
        // the body: a queued Reload op can be stale by the time it runs
        // (a concurrent withdrawal may have emptied the store under us),
        // and running then would activate a plugin whose required
        // services are gone. Recomputing the epoch either confirms the
        // reload or hands control to the follow-up transitions.
        self.revalidate(core);
        let (plugin, config) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed || inner.error.is_some() || inner.epoch.is_none() {
                return;
            }
            inner.provided.clear();
            (inner.plugin.clone(), inner.config.clone())
        };
        let Some(plugin) = plugin else { return };
        // Schema resolution runs before the body: a rejected config
        // leaves the fiber Failed with no side effects, an accepted one
        // is stored resolved (defaults filled) so restarts persist them.
        let config = match plugin.schema() {
            Some(schema) => match schema.resolve(&config) {
                Ok(resolved) => {
                    let mut inner = self.inner.lock().unwrap();
                    if inner.disposed {
                        return;
                    }
                    inner.config = resolved.clone();
                    resolved
                }
                Err(diagnostics) => {
                    let message = diagnostics
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join("; ");
                    self.log(
                        core,
                        LogLevel::Error,
                        &format!("config rejected: {message}"),
                    );
                    {
                        let mut inner = self.inner.lock().unwrap();
                        inner.error = Some(PluginError::new(format!(
                            "config rejected: {message}"
                        )));
                        inner.epoch = None;
                    }
                    if let Some(old) = self.set_state(FiberState::Failed) {
                        events::emit_status(core, self, old);
                    }
                    return;
                }
            },
            None => config,
        };
        lifecycle_trace::trace("reload-enter", self);
        self.log(core, LogLevel::Debug, "reloading");

        let ctx = self.ctx().clone();
        let (batch, result) =
            ScopeGuard::collect(&self.strong(), || run_plugin_apply(&plugin, &ctx, &config));

        match result {
            Ok(()) => {
                self.push_effect_batch(batch);
                if let Some(old) = self.set_state(FiberState::Active) {
                    events::emit_status(core, self, old);
                }
                // Activate dependents waiting on the services we provide.
                let provided: Vec<(String, crate::context::IsolateLabel)> = {
                    let mut inner = self.inner.lock().unwrap();
                    std::mem::take(&mut inner.provided)
                };
                if !provided.is_empty() {
                    crate::reflect::notify(core, &provided);
                }
            }
            Err(err) => {
                // Roll back everything the failed body already recorded.
                // (v4 keeps partial effects until the next unload; we revert
                // eagerly so a Failed fiber has no live side effects.)
                // Work units the body started are drained first, so their
                // completion callbacks observe a live fiber and rollback
                // runs without concurrent plugin code.
                crate::work::drain_fiber_works(self);
                batch.dispose();
                self.log(core, LogLevel::Error, &format!("plugin failed: {err}"));
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.error = Some(err);
                    inner.epoch = None;
                }
                if let Some(old) = self.set_state(FiberState::Failed) {
                    events::emit_status(core, self, old);
                }
            }
        }
    }

    /// Whether no lifecycle op is queued or running on this fiber.
    pub(crate) fn schedule_debug_idle(&self) -> bool {
        let state = self.schedule.lock().unwrap();
        !state.busy && state.queue.is_empty()
    }

    /// Re-read dependencies and recompute the epoch, driving any
    /// follow-up transitions, so stale queued ops self-correct.
    fn revalidate(&self, core: &Arc<Core>) {
        let names: Vec<String> = self.inject_names().iter().map(|(n, _)| n.clone()).collect();
        if self.check_impls(core, &names) {
            let ops = self.refresh_ops();
            drive_ops(ops);
        }
    }

    fn unload(&self, core: &Arc<Core>, keep_alive: bool) {
        let batches = {
            let mut inner = self.inner.lock().unwrap();
            if inner.unloading {
                return;
            }
            inner.unloading = true;
            std::mem::take(&mut inner.batches)
        };
        // Drain-before-rollback (§4.1 I1): no plugin code may run
        // concurrently with the disposers below.
        crate::work::drain_fiber_works(self);
        lifecycle_trace::trace("unload-enter", self);
        self.log(core, LogLevel::Debug, "unloading");
        for batch in batches {
            batch.dispose();
        }
        let mut inner = self.inner.lock().unwrap();
        inner.unloading = false;
        let new_state = if inner.disposed || !keep_alive {
            FiberState::Disposed
        } else {
            FiberState::Pending
        };
        drop(inner);
        if let Some(old) = self.set_state(new_state) {
            events::emit_status(core, self, old);
        }
    }

    /// Dispose: deregister, announce, revert all effects. Service
    /// withdrawal happens inside the provide-disposers, cascading to
    /// dependent fibers before we finish.
    pub(crate) fn dispose(self: &Arc<Fiber>, core: &Arc<Core>) -> bool {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed || inner.unloading {
                return false;
            }
            inner.disposed = true;
            inner.error = None;
        }
        lifecycle_trace::trace("dispose", self);
        events::emit_plugin(core, self);
        crate::registry::remove_fiber(core, self);
        self.unload(core, false);
        true
    }

    /// Force stop + start with the current config (v4 `fiber.restart`).
    /// The epoch is forced through an inactive round-trip so the body
    /// re-runs even when the dependency set did not change.
    pub(crate) fn restart(&self, core: &Arc<Core>) -> Result<(), CordisError> {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed {
                return self.check_failed();
            }
            inner.epoch = None;
        }
        self.unload(core, true);
        let names: Vec<String> = self.inject_names().iter().map(|(n, _)| n.clone()).collect();
        self.check_impls(core, &names);
        let ops = self.refresh();
        drive_ops(ops);
        self.check_failed()
    }

    /// Swap the plugin in place (upstream `registry.replace` /
    /// `fiber._replace`): the fiber keeps its identity — id, parent,
    /// config — and the new body re-runs through the normal
    /// unload/reload cascade, so dependents observe the service change.
    /// A previous failure is cleared, letting `replace` recover a
    /// `Failed` fiber. The runtime registration must already point at
    /// the new plugin's runtime (`registry::move_fiber`).
    pub(crate) fn replace_plugin(
        self: &Arc<Fiber>,
        core: &Arc<Core>,
        new: Arc<dyn Plugin>,
    ) -> Result<(), CordisError> {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed {
                return Err(CordisError::InactiveContext {
                    action: "replace a disposed fiber",
                    fiber: Some(self.name.clone()),
                });
            }
            inner.plugin = Some(new.clone());
            inner.inject = new.inject();
            inner.provides = new.provides();
            inner.error = None;
        }
        // The declared inject may have changed: re-index for targeted
        // notify.
        {
            let mut registry = core.registry.lock().unwrap();
            registry.unregister_fiber(self);
            registry.register_injects(self);
        }
        events::emit_replace(core, self);
        self.restart(core)
    }

    pub(crate) fn check_failed(&self) -> Result<(), CordisError> {
        let inner = self.inner.lock().unwrap();
        match &inner.error {
            Some(e) => Err(CordisError::PluginError(PluginError(
                format!("fiber `{}` failed: {}", self.name, e).into(),
            ))),
            None => Ok(()),
        }
    }

    // -----------------------------------------------------------------
    // Op scheduling
    // -----------------------------------------------------------------

    fn schedule(&self, op: Op) {
        crate::fiber::PENDING_OPS.fetch_add(1, Ordering::SeqCst);
        let desc = match &op {
            Op::Reload { .. } => "Reload",
            Op::Unload { .. } => "Unload",
        };
        lifecycle_trace::trace(&format!("schedule({desc}) epoch={:?}", op_epoch(&op)), self);
        // Push and busy-check share one lock: a racer either observes
        // `busy == false` and drives the op itself, or queues under the
        // same lock the holder drains from. No lost-wakeup window.
        let mut state = self.schedule.lock().unwrap();
        state.queue.push(op);
        if state.busy {
            return;
        }
        state.busy = true;
        loop {
            let queue = std::mem::take(&mut state.queue);
            drop(state);
            for queued in queue {
                self.run_op(queued);
            }
            state = self.schedule.lock().unwrap();
            if state.queue.is_empty() {
                state.busy = false;
                return;
            }
        }
    }

    fn run_op(&self, op: Op) {
        EXECUTING_OPS.with(|c| c.set(c.get() + 1));
        let _op_guard = crate::fiber::OpGuard;
        // Stale-op guard: a queued op was scheduled against an epoch
        // snapshot. If the fiber's epoch has moved on, the op's premise no
        // longer holds — executing it could re-activate an unloaded fiber
        // or unload a re-activated one. The current epoch's own transitions
        // are (or will be) scheduled separately.
        let stale = {
            let inner = self.inner.lock().unwrap();
            let current = &inner.epoch;
            match &op {
                Op::Reload { epoch } => epoch != current,
                Op::Unload { epoch, .. } => epoch.is_some() && epoch != current,
            }
        };
        if stale {
            lifecycle_trace::trace("run_op(STALE, dropped)", self);
            return;
        }
        let core = self.ctx().core.clone();
        match op {
            Op::Reload { .. } => self.reload(&core),
            Op::Unload { keep_alive, .. } => self.unload(&core, keep_alive),
        }
    }
}

/// Execute pending epoch transitions outside of any lock.
pub(crate) fn drive_ops(ops: Vec<(Arc<Fiber>, Op)>) {
    for (fiber, op) in ops {
        fiber.schedule(op);
    }
}

pub(crate) fn disposal_disposer(fiber: &Arc<Fiber>) -> crate::effect::Disposer {
    let core = fiber.ctx().core.clone();
    let fiber = Arc::downgrade(fiber);
    Box::new(move || {
        // Panics inside disposer chains are caught by run_disposers; the
        // upgrade failing means the fiber was already torn down.
        let _ = fiber.upgrade().map(|fiber| fiber.dispose(&core));
    })
}
