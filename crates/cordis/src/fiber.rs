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
use std::sync::{Arc, Mutex, OnceLock, Weak};

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

/// A service implementation registered by a fiber.
pub(crate) struct ServiceImpl {
    pub name: String,
    pub value: Mutex<Arc<dyn Any + Send + Sync>>,
    pub owner: Weak<Fiber>,
    pub owner_name: String,
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
    /// Checked dependency impls (`_store` in v4).
    pub dep_store: HashMap<String, Arc<ServiceImpl>>,
    /// Visibility snapshot used by context service resolution
    /// (`store` in v4).
    pub visible: HashMap<String, Arc<ServiceImpl>>,
    /// Effects owned by this fiber, disposed in reverse order.
    pub batches: Vec<EffectHandle>,
    /// Services provided during the current body run, notified on
    /// activation.
    pub provided: Vec<String>,
    pub unloading: bool,
    pub disposed: bool,
}

pub(crate) struct Fiber {
    pub id: u64,
    pub name: String,
    pub parent: Option<Weak<Fiber>>,
    pub plugin: Option<Arc<dyn Plugin>>,
    pub runtime: OnceLock<Arc<crate::registry::Runtime>>,
    pub self_weak: OnceLock<Weak<Fiber>>,
    pub ctx: OnceLock<Context>,
    pub inner: Mutex<FiberInner>,
    /// Serializes lifecycle ops on this fiber. `busy` + the deferred queue
    /// live under one lock so a racer can never push an op after the
    /// holder released the flag without it being drained (lost wakeup).
    schedule: Mutex<ScheduleState>,
    /// Declared dependencies, cached at install (the plugin trait is
    /// queried exactly once).
    inject: Inject,
}

#[derive(Default)]
struct ScheduleState {
    busy: bool,
    queue: Vec<Op>,
}

pub(crate) enum Op {
    Reload,
    Unload { keep_alive: bool },
}

fn fresh_inner(state: FiberState, config: Value) -> FiberInner {
    FiberInner {
        state,
        config,
        epoch: if matches!(state, FiberState::Active) {
            Some(Vec::new())
        } else {
            None
        },
        error: None,
        dep_store: HashMap::new(),
        visible: HashMap::new(),
        batches: Vec::new(),
        provided: Vec::new(),
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
            plugin: None,
            runtime: OnceLock::new(),
            self_weak: OnceLock::from(weak.clone()),
            ctx: OnceLock::new(),
            inner: Mutex::new(fresh_inner(FiberState::Active, Value::Null)),
            schedule: Mutex::new(ScheduleState::default()),
            inject: Inject::none(),
        })
    }

    pub(crate) fn install(
        core: &Arc<Core>,
        parent: Weak<Fiber>,
        plugin: Arc<dyn Plugin>,
        config: Value,
        name: String,
    ) -> Arc<Fiber> {
        let inject = plugin.inject();
        Arc::new_cyclic(|weak| Fiber {
            id: core.next_fiber_id(),
            name,
            parent: Some(parent),
            plugin: Some(plugin),
            runtime: OnceLock::new(),
            self_weak: OnceLock::from(weak.clone()),
            ctx: OnceLock::new(),
            inner: Mutex::new(fresh_inner(FiberState::Pending, config)),
            schedule: Mutex::new(ScheduleState::default()),
            inject,
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
        self.runtime.get().cloned()
    }

    pub(crate) fn state(&self) -> FiberState {
        self.inner.lock().unwrap().state
    }

    /// Record a service provided from this fiber: visible immediately,
    /// queued for activation notify while a body is still running.
    pub(crate) fn note_provided(&self, name: &str, impl_: Arc<ServiceImpl>) {
        let mut inner = self.inner.lock().unwrap();
        inner.visible.insert(name.to_owned(), impl_);
        if inner.state == FiberState::Pending {
            inner.provided.push(name.to_owned());
        }
    }

    pub(crate) fn visible(&self, name: &str) -> Option<Arc<ServiceImpl>> {
        self.inner.lock().unwrap().visible.get(name).cloned()
    }

    pub(crate) fn drop_visible(&self, name: &str) {
        self.inner.lock().unwrap().visible.remove(name);
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
    /// Cached at install time; `Plugin::inject` is never re-queried.
    pub(crate) fn inject_names(&self) -> &[(String, bool)] {
        &self.inject.entries
    }

    /// Re-read the given services from the global store into `dep_store`.
    /// Returns true if any relevant entry changed.
    pub(crate) fn check_impls(&self, core: &Arc<Core>, names: &[String]) -> bool {
        let inject: Vec<String> = self.inject_names().iter().map(|(n, _)| n.clone()).collect();
        let mut changed = false;
        let mut inner = self.inner.lock().unwrap();
        for name in names {
            if !inject.iter().any(|n| n == name) {
                continue;
            }
            let fresh = core.store_impl(name);
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
        let required: Vec<String> = self.inject.required_names().map(str::to_owned).collect();
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
        if new_epoch == old_epoch {
            return Vec::new();
        }
        inner.epoch = new_epoch.clone();
        match (old_epoch, new_epoch) {
            (None, Some(_)) => vec![(self.strong(), Op::Reload)],
            (Some(_), None) => vec![(self.strong(), Op::Unload { keep_alive: true })],
            (Some(_), Some(_)) => vec![
                (self.strong(), Op::Unload { keep_alive: true }),
                (self.strong(), Op::Reload),
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
    }

    fn reload(&self, core: &Arc<Core>) {
        let (plugin, config) = {
            let mut inner = self.inner.lock().unwrap();
            if inner.disposed || inner.error.is_some() {
                return;
            }
            // Refresh the visibility snapshot from checked deps, as v4 does
            // at the start of `_reload`.
            inner.visible = inner.dep_store.clone();
            inner.provided.clear();
            (self.plugin.clone(), inner.config.clone())
        };
        let Some(plugin) = plugin else { return };
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
                let provided: Vec<String> = {
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

    fn unload(&self, core: &Arc<Core>, keep_alive: bool) {
        let batches = {
            let mut inner = self.inner.lock().unwrap();
            if inner.unloading {
                return;
            }
            inner.unloading = true;
            inner.visible.clear();
            std::mem::take(&mut inner.batches)
        };
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
        let core = self.ctx().core.clone();
        match op {
            Op::Reload => self.reload(&core),
            Op::Unload { keep_alive } => self.unload(&core, keep_alive),
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
