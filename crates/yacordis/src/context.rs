//! The context: a single intermediary for effects and coeffects.
//!
//! A `Context` is a cheap handle (clone it freely) pairing the shared
//! [`Core`] with the fiber whose scope it belongs to. Service resolution
//! is one rule: look up the name under this context's isolation label,
//! and accept it when the provider is active or is the requesting fiber
//! itself.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde_json::Value;

use crate::component::{Component, Inject, InjectClosure};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::{ComponentError, ComponentResult, CordisError};
use crate::events;
use crate::fiber::{Fiber, FiberState, ServiceImpl, disposal_disposer};
use crate::logger::{Logger, LoggerHandle, StderrLogger};
use crate::registry::{Registry, component_key};

/// Identity of one isolation group for a service name. Labels compare by
/// pointer identity; every [`Context::isolate`] derivation mints a fresh
/// one, and everything outside any isolation shares the default label.
#[derive(Clone, Debug)]
pub struct IsolateLabel(Arc<()>);

impl IsolateLabel {
    pub(crate) fn shared() -> Self {
        static SHARED: OnceLock<IsolateLabel> = OnceLock::new();
        SHARED.get_or_init(|| IsolateLabel(Arc::new(()))).clone()
    }

    pub(crate) fn fresh() -> Self {
        Self(Arc::new(()))
    }
}

impl PartialEq for IsolateLabel {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for IsolateLabel {}
impl Hash for IsolateLabel {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// Name → label map, carried by contexts and captured by fibers at
/// install time. Cheap to clone; deriving inserts one fresh label.
#[derive(Clone, Default)]
pub(crate) struct IsolateMap {
    labels: HashMap<String, IsolateLabel>,
}

impl IsolateMap {
    pub(crate) fn label(&self, name: &str) -> IsolateLabel {
        self.labels
            .get(name)
            .cloned()
            .unwrap_or_else(IsolateLabel::shared)
    }

    pub(crate) fn isolated(&self, name: &str) -> Self {
        let mut labels = self.labels.clone();
        labels.insert(name.to_owned(), IsolateLabel::fresh());
        Self { labels }
    }
}

/// Store key: a service name **within one isolation group**.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ServiceKey {
    pub name: String,
    pub label: IsolateLabel,
}

pub(crate) struct Core {
    hook_sn: AtomicU64,
    pub(crate) root: Arc<Fiber>,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) hooks: Mutex<HashMap<String, Vec<events::Hook>>>,
    pub(crate) middlewares: Mutex<HashMap<String, Vec<events::MiddlewareEntry>>>,
    pub(crate) store: Mutex<HashMap<ServiceKey, Arc<ServiceImpl>>>,
    pub(crate) logger: LoggerHandle,
    /// Cycle shapes already reported on the logger (dedup key).
    pub(crate) reported_cycles: Mutex<HashSet<String>>,
    /// Service value schemas (the `check` predicate, Rust-shaped): for
    /// each (name, label), the schema every provide/set of that JSON
    /// service must satisfy. Registered as effects.
    pub(crate) service_schemas: Mutex<HashMap<ServiceKey, crate::schema::Schema>>,
    /// Framework work substrate: id counter + lazy executor.
    pub(crate) work: crate::work::WorkRegistry,
}

impl Core {
    fn new() -> Arc<Core> {
        let root = Fiber::new_root();
        let core = Arc::new(Core {
            hook_sn: AtomicU64::new(1),
            root: root.clone(),
            registry: Mutex::new(Registry::default()),
            hooks: Mutex::new(HashMap::new()),
            middlewares: Mutex::new(HashMap::new()),
            store: Mutex::new(HashMap::new()),
            logger: LoggerHandle::new(Arc::new(StderrLogger)),
            reported_cycles: Mutex::new(HashSet::new()),
            service_schemas: Mutex::new(HashMap::new()),
            work: crate::work::WorkRegistry::default(),
        });
        let _ = root.ctx.set(Context {
            core: core.clone(),
            fiber: Arc::downgrade(&root),
            isolate: Arc::new(IsolateMap::default()),
        });
        core
    }

    /// Mint a fiber id. The counter is **process-global**, not per
    /// runtime: the loader's component-facing handle table is process-global
    /// and garbage-collects context handles by fiber id on dispose, so two
    /// runtimes in one process must never mint the same id (a collision
    /// lets one runtime's dispose audit remove the other's live context
    /// handles, which surfaces as `run_blocking failed` in a component).
    pub(crate) fn next_fiber_id(&self) -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn next_hook_id(&self) -> u64 {
        self.hook_sn.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn store_impl(&self, name: &str, label: &IsolateLabel) -> Option<Arc<ServiceImpl>> {
        let key = ServiceKey {
            name: name.to_owned(),
            label: label.clone(),
        };
        self.store.lock().unwrap().get(&key).cloned()
    }

    pub(crate) fn root_weak(&self) -> Weak<Fiber> {
        Arc::downgrade(&self.root)
    }
}

/// A scoped handle onto the cordis runtime: entry point for installing
/// components, providing services, registering events and effects.
#[derive(Clone)]
pub struct Context {
    pub(crate) core: Arc<Core>,
    pub(crate) fiber: Weak<Fiber>,
    pub(crate) isolate: Arc<IsolateMap>,
}

impl Context {
    /// Create a fresh runtime with an active root context.
    pub fn new() -> Context {
        let core = Core::new();
        Context {
            fiber: Arc::downgrade(&core.root),
            core,
            isolate: Arc::new(IsolateMap::default()),
        }
    }

    pub(crate) fn for_fiber(
        core: &Arc<Core>,
        fiber: &Arc<Fiber>,
        isolate: Arc<IsolateMap>,
    ) -> Context {
        Context {
            core: core.clone(),
            fiber: Arc::downgrade(fiber),
            isolate,
        }
    }

    /// The root context: outside every component, no isolations applied.
    pub fn root(&self) -> Context {
        Context {
            core: self.core.clone(),
            fiber: Arc::downgrade(&self.core.root),
            isolate: Arc::new(IsolateMap::default()),
        }
    }

    /// Derive a context that isolates one service name: components installed
    /// through the derived context see a **different** instance of that
    /// service than the rest of the runtime (cordis `ctx.isolate`).
    ///
    /// The same component can therefore be installed several times, each
    /// instance wired to its own instance of the isolated service.
    pub fn isolate(&self, name: impl AsRef<str>) -> Context {
        Context {
            core: self.core.clone(),
            fiber: self.fiber.clone(),
            isolate: Arc::new(self.isolate.isolated(name.as_ref())),
        }
    }

    pub(crate) fn isolate_label(&self, name: &str) -> IsolateLabel {
        self.isolate.label(name)
    }

    /// Whether the fiber this context belongs to is still installed
    /// (contexts outliving their fiber resolve nothing).
    #[must_use]
    pub fn fiber_alive(&self) -> bool {
        self.fiber.strong_count() > 0
    }

    /// Whether this context is the root context.
    pub fn is_root(&self) -> bool {
        self.current_fiber().map(|f| f.id) == Some(0)
    }

    /// A handle to the fiber this context belongs to.
    pub fn fiber(&self) -> FiberHandle {
        FiberHandle {
            fiber: self.fiber.clone(),
        }
    }

    pub(crate) fn current_fiber(&self) -> Option<Arc<Fiber>> {
        self.fiber.upgrade()
    }

    /// The current fiber, requiring it to be installed and not going away.
    pub(crate) fn active_fiber(&self, action: &'static str) -> Result<Arc<Fiber>, CordisError> {
        match self.fiber.upgrade() {
            Some(fiber) => {
                let inner = fiber.inner.lock().unwrap();
                if inner.disposed || inner.unloading {
                    return Err(CordisError::InactiveContext {
                        action,
                        fiber: Some(fiber.name.clone()),
                    });
                }
                drop(inner);
                Ok(fiber)
            }
            None => Err(CordisError::InactiveContext {
                action,
                fiber: None,
            }),
        }
    }

    // -----------------------------------------------------------------
    // Components
    // -----------------------------------------------------------------

    /// Install a component on the current fiber and run its body (when its
    /// required services are available).
    ///
    /// Body failures do not error here: the fiber enters
    /// [`FiberState::Failed`] and the error surfaces from
    /// [`FiberHandle::update`] / [`FiberHandle::restart`].
    pub fn component(
        &self,
        component: impl Component,
        config: impl Into<Value>,
    ) -> Result<FiberHandle, CordisError> {
        self.component_dyn(Arc::new(component), config.into())
    }

    /// Install an already-erased component. This is the entry point used by
    /// the dynamic loader for cdylib components.
    pub fn component_dyn(
        &self,
        component: Arc<dyn Component>,
        config: impl Into<Value>,
    ) -> Result<FiberHandle, CordisError> {
        let parent = self.active_fiber("install a component")?;
        let config = config.into();
        let name = component.name().unwrap_or("anonymous").to_owned();

        // Get or create the runtime entry for this component identity.
        let key = component_key(&component);
        let runtime = {
            let mut registry = self.core.registry.lock().unwrap();
            registry.entry(key, name.clone(), &component)
        };

        let fiber = Fiber::install(
            &self.core,
            self.isolate.clone(),
            Arc::downgrade(&parent),
            component,
            config,
            name,
        );
        *fiber.runtime.write().unwrap() = Some(runtime.clone());
        let _ = fiber
            .ctx
            .set(Context::for_fiber(&self.core, &fiber, self.isolate.clone()));
        runtime.fibers.lock().unwrap().push(fiber.clone());
        // Index the declared injects before the body runs: notifies
        // fired during the body must find this fiber.
        {
            let mut registry = self.core.registry.lock().unwrap();
            registry.register_injects(&fiber);
        }

        // Register disposal with the parent fiber: when the parent
        // unloads, this fiber is torn down with it (cascade). Claimed by
        // the parent's open effect batch when installing from inside
        // another component body.
        let handle = EffectHandle::new("ctx.component()");
        handle.push(disposal_disposer(&fiber));
        attach_effect(&parent, handle);

        fiber.run_install_effect(&self.core);
        Ok(FiberHandle {
            fiber: Arc::downgrade(&fiber),
        })
    }

    /// Sugar for a closure component with declared dependencies:
    /// `ctx.inject(Inject::required(&["database"]), |ctx| { … })`.
    pub fn inject<F>(&self, deps: Inject, f: F) -> Result<FiberHandle, CordisError>
    where
        F: Fn(&Context) -> ComponentResult + Send + Sync + 'static,
    {
        self.component_dyn(Arc::new(InjectClosure { inject: deps, f }), Value::Null)
    }

    /// Replace every fiber installed from `old` with `new`, in place
    /// (v4 `registry.replace`). Fibers keep their identity — id, parent,
    /// config — and re-run the new body through the normal unload/reload
    /// cascade, so dependents observe the service change. A failed fiber
    /// recovers. After the call, `old` is no longer a registered component
    /// identity: installing it again starts a fresh runtime.
    ///
    /// Returns handles to the affected fibers. Body failures land on the
    /// fiber ([`FiberState::Failed`]); the first such error is returned.
    pub fn replace_component(
        &self,
        old: Arc<dyn Component>,
        new: Arc<dyn Component>,
    ) -> Result<Vec<FiberHandle>, CordisError> {
        let fibers = {
            let registry = self.core.registry.lock().unwrap();
            registry.fibers_of(component_key(&old))
        }
        .ok_or_else(|| {
            CordisError::ComponentError(ComponentError(
                "replace_component: component is not installed".into(),
            ))
        })?;
        let new_key = component_key(&new);
        let new_name = new.name().unwrap_or("anonymous").to_owned();

        let mut handles = Vec::new();
        let mut first_error = None;
        for fiber in fibers {
            crate::registry::move_fiber(&self.core, &fiber, new_key, new_name.clone(), &new);
            if let Err(err) = fiber.replace_component(&self.core, new.clone()) {
                first_error.get_or_insert(err);
            }
            handles.push(FiberHandle {
                fiber: Arc::downgrade(&fiber),
            });
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(handles),
        }
    }

    // -----------------------------------------------------------------
    // Services
    // -----------------------------------------------------------------

    /// Provide a service from the current fiber. The registration is an
    /// effect: it is withdrawn (and dependents deactivated) when the
    /// fiber unloads.
    pub fn provide_raw(
        &self,
        name: impl Into<String>,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), CordisError> {
        let fiber = self.active_fiber("provide a service")?;
        crate::reflect::provide(&self.core, self, &fiber, name.into(), value)
    }

    /// Typed convenience over [`Context::provide_raw`].
    pub fn provide<T: Send + Sync + 'static>(
        &self,
        name: impl Into<String>,
        value: Arc<T>,
    ) -> Result<(), CordisError> {
        self.provide_raw(name, value as Arc<dyn Any + Send + Sync>)
    }

    /// Replace the value of a service previously provided by this fiber.
    pub fn set_raw(
        &self,
        name: &str,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), CordisError> {
        crate::reflect::set(&self.core, self, name, value)
    }

    /// Typed convenience over [`Context::set_raw`].
    pub fn set<T: Send + Sync + 'static>(
        &self,
        name: &str,
        value: Arc<T>,
    ) -> Result<(), CordisError> {
        self.set_raw(name, value as Arc<dyn Any + Send + Sync>)
    }

    /// Set an intercept override for a JSON service: JSON values
    /// resolved through this context are merged with the intercept
    /// layers along the fiber chain, the nearest fiber winning
    /// (shallow object merge, non-object layers replace). The entry is
    /// an effect of the current fiber — unload removes it — and never
    /// affects epochs or notifications (view-layer only).
    pub fn intercept(
        &self,
        name: impl Into<String>,
        value: impl Into<Value>,
    ) -> Result<(), CordisError> {
        let fiber = self.active_fiber("set an intercept")?;
        crate::reflect::set_intercept(&fiber, name.into(), value.into())
    }

    /// Register a value schema for a JSON service (the `check`
    /// predicate): every subsequent provide/set of that service — from
    /// any fiber in this isolation group — must satisfy it, or the
    /// call fails with the validation diagnostics. The registration is
    /// an effect of the current fiber and dies with it.
    pub fn provide_schema(
        &self,
        name: impl Into<String>,
        schema: crate::schema::Schema,
    ) -> Result<(), CordisError> {
        let fiber = self.active_fiber("register a service schema")?;
        crate::reflect::provide_schema(&self.core, self, &fiber, name.into(), schema)
    }

    /// Resolve a service by name within this context's isolation group.
    pub fn get_raw(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        crate::reflect::resolve_value(&self.core, self, name)
    }

    /// Resolve a service and downcast to its concrete type.
    pub fn get<T: Send + Sync + 'static>(&self, name: &str) -> Option<Arc<T>> {
        self.get_raw(name)?.downcast::<T>().ok()
    }

    /// Whether a service is currently resolvable from this context.
    pub fn has(&self, name: &str) -> bool {
        self.get_raw(name).is_some()
    }

    // -----------------------------------------------------------------
    // Events (raw, string-keyed)
    // -----------------------------------------------------------------

    /// Register a listener; returns a handle that removes it when called.
    pub fn on<F>(&self, name: impl Into<String>, listener: F) -> Result<ListenerHandle, CordisError>
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        let fiber = self.active_fiber("register a listener")?;
        events::on(&self.core, &fiber, name.into(), Arc::new(listener), false)
    }

    /// Register a listener that runs before previously registered ones.
    pub fn on_prepend<F>(
        &self,
        name: impl Into<String>,
        listener: F,
    ) -> Result<ListenerHandle, CordisError>
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        let fiber = self.active_fiber("register a listener")?;
        events::on(&self.core, &fiber, name.into(), Arc::new(listener), true)
    }

    /// Register a listener that removes itself after the first dispatch.
    pub fn once<F>(&self, name: impl Into<String>, listener: F) -> Result<EffectHandle, CordisError>
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        let fiber = self.active_fiber("register a listener")?;
        events::once(&self.core, &fiber, name.into(), Arc::new(listener))
    }

    /// Dispatch an event: all listeners run in registration order,
    /// return values ignored.
    pub fn emit(&self, name: &str, args: &Value) {
        events::emit(&self.core, self, name, args);
    }

    /// Dispatch an event and return the first non-`None` listener result,
    /// short-circuiting (v4 `bail`/`serial` in a synchronous world).
    pub fn bail(&self, name: &str, args: &Value) -> Option<Value> {
        events::bail(&self.core, self, name, args)
    }

    /// Register a middleware participating in [`Context::waterfall`].
    pub fn middleware<F>(&self, name: impl Into<String>, mw: F) -> Result<EffectHandle, CordisError>
    where
        F: Fn(&Context, Value, events::Next<'_>) -> Result<Value, CordisError>
            + Send
            + Sync
            + 'static,
    {
        let fiber = self.active_fiber("register a middleware")?;
        events::middleware(&self.core, &fiber, name.into(), Arc::new(mw), false)
    }

    /// Register a middleware that runs before previously registered
    /// ones on the same chain (upstream `{ prepend: true }`): among
    /// prepended entries the most recently registered runs first.
    pub fn middleware_prepend<F>(
        &self,
        name: impl Into<String>,
        mw: F,
    ) -> Result<EffectHandle, CordisError>
    where
        F: Fn(&Context, Value, events::Next<'_>) -> Result<Value, CordisError>
            + Send
            + Sync
            + 'static,
    {
        let fiber = self.active_fiber("register a middleware")?;
        events::middleware(&self.core, &fiber, name.into(), Arc::new(mw), true)
    }

    /// Run the middleware chain registered under `name`, ending at
    /// `fallback` when every middleware delegated. A panicking middleware
    /// aborts the chain and surfaces as an error (never as a value).
    pub fn waterfall(
        &self,
        name: &str,
        init: Value,
        fallback: impl FnOnce(Value) -> Value,
    ) -> Result<Value, CordisError> {
        events::waterfall(&self.core, self, name, init, Box::new(fallback))
    }

    /// Convenience for events carrying a single serializable argument.
    pub fn emit_args(&self, name: &str, args: impl serde::Serialize) {
        match serde_json::to_value(args) {
            Ok(value) => self.emit(name, &value),
            Err(_) => self.core.logger.log(
                crate::logger::LogLevel::Warn,
                "yacordis/events",
                "failed to serialize event args",
            ),
        }
    }

    // -----------------------------------------------------------------
    // Misc
    // -----------------------------------------------------------------

    /// Swap the diagnostic logger.
    pub fn set_logger(&self, logger: Arc<dyn Logger>) {
        self.core.logger.set(logger);
    }

    /// Introspection: every component identity with installed fibers, and
    /// the state of each fiber. Hosts and debug UIs use this to answer
    /// "what is loaded right now?".
    pub fn installed_components(&self) -> Vec<ComponentSummary> {
        let registry = self.core.registry.lock().unwrap();
        registry.summaries()
    }

    /// Introspection: every currently-provided service, across all
    /// isolation groups (labels are not part of the summary; compare by
    /// owner instead).
    pub fn provided_services(&self) -> Vec<ServiceSummary> {
        let store = self.core.store.lock().unwrap();
        store
            .values()
            .map(|impl_| ServiceSummary {
                name: impl_.name.clone(),
                owner: impl_.owner_name.clone(),
                active: impl_
                    .owner
                    .upgrade()
                    .map(|fiber| fiber.state() == FiberState::Active)
                    .unwrap_or(false),
            })
            .collect()
    }

    /// Introspection: groups of installed fibers that mutually wait for
    /// each other's declared provides — none of them can ever activate
    /// (the counterpart of cordis v4 issue #66, which pends silently).
    /// Empty unless [`Component::provides`](crate::Component::provides)
    /// declarations close a loop among pending fibers; the same groups
    /// are logged (once per shape) as they form.
    pub fn dependency_cycles(&self) -> Vec<Vec<String>> {
        crate::diagnose::dependency_cycles(&self.core)
    }

    #[allow(dead_code)] // exercised by tests and future modules
    pub(crate) fn log_error(&self, target: &str, error: &(dyn std::error::Error + 'static)) {
        self.core.logger.log_error(target, error);
    }

    /// Block until every lifecycle operation in the runtime has finished
    /// (the synchronous counterpart of cordis v4's async `inertia`).
    ///
    /// Install/dispose drive their own cascades synchronously on the
    /// calling thread when uncontended; under concurrent lifecycle
    /// activity, some cascaded transitions may complete shortly after the
    /// triggering call returns. Hosts that need a globally stable view
    /// (assertions, snapshots, shutdown) call this first.
    pub fn quiesce(&self) {
        self.core.quiesce();
    }

    /// Dispose every installed component fiber in reverse installation
    /// order (root effects survive; this is a teardown helper for hosts
    /// and tests).
    pub fn dispose_all(&self) {
        let mut fibers: Vec<Arc<Fiber>> = {
            let registry = self.core.registry.lock().unwrap();
            registry.all_fibers()
        };
        // Later installations revert first — the global counterpart of the
        // per-fiber reverse-registration discipline. Fiber ids are handed
        // out in installation order.
        fibers.sort_by_key(|fiber| std::cmp::Reverse(fiber.id));
        for fiber in fibers {
            fiber.dispose(&self.core);
        }
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Core {
    /// Wait until every scheduled lifecycle op in the runtime has
    /// finished executing.
    pub(crate) fn quiesce(&self) {
        let mut spins = 0u32;
        while crate::fiber::pending_ops() > 0 {
            std::thread::sleep(std::time::Duration::from_micros(50));
            spins += 1;
            if spins == 40 {
                eprintln!(
                    "[yacordis quiesce] stuck: pending_ops={} fibers={}",
                    crate::fiber::pending_ops(),
                    {
                        let registry = self.registry.lock().unwrap();
                        registry
                            .all_fibers()
                            .iter()
                            .map(|f| {
                                format!(
                                    "{}[{}]state={:?} idle={}",
                                    f.name,
                                    f.id,
                                    f.inner.lock().unwrap().state,
                                    f.schedule_debug_idle()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                );
            }
            if spins > 20_000_000 {
                return;
            }
        }
    }
}

/// Handle that removes a listener (an alias: listeners *are* effects).
pub type ListenerHandle = EffectHandle;

/// Summary of one installed component fiber.
#[derive(Clone, Debug)]
pub struct FiberSummary {
    /// The fiber id.
    pub id: u64,
    /// The component name (or `anonymous`).
    pub name: String,
    /// Current lifecycle state.
    pub state: FiberState,
}

/// Summary of one component identity and its installed fibers.
#[derive(Clone, Debug)]
pub struct ComponentSummary {
    /// Component name, when declared.
    pub name: Option<String>,
    /// Every fiber installed from this component.
    pub fibers: Vec<FiberSummary>,
}

/// Summary of one provided service.
#[derive(Clone, Debug)]
pub struct ServiceSummary {
    /// Service name.
    pub name: String,
    /// Name of the owning component fiber.
    pub owner: String,
    /// Whether the owner fiber is currently active.
    pub active: bool,
}

/// Handle onto an installed component fiber: inspect state, hot-update its
/// config, restart, or dispose (dynamic uninstall).
#[derive(Clone)]
pub struct FiberHandle {
    pub(crate) fiber: Weak<Fiber>,
}

impl FiberHandle {
    pub(crate) fn upgrade(&self) -> Option<Arc<Fiber>> {
        self.fiber.upgrade()
    }

    fn require(&self, action: &'static str) -> Result<Arc<Fiber>, CordisError> {
        self.upgrade().ok_or(CordisError::InactiveContext {
            action,
            fiber: None,
        })
    }

    /// The fiber id, or `None` when the fiber is gone.
    pub fn id(&self) -> Option<u64> {
        self.upgrade().map(|f| f.id)
    }

    /// The component name, or `None` when the fiber is gone.
    pub fn name(&self) -> Option<String> {
        self.upgrade().map(|f| f.name.clone())
    }

    /// Current lifecycle state; [`FiberState::Disposed`] once gone.
    pub fn state(&self) -> FiberState {
        match self.upgrade() {
            Some(fiber) => fiber.inner.lock().unwrap().state,
            None => FiberState::Disposed,
        }
    }

    /// Whether the fiber is currently active.
    pub fn is_active(&self) -> bool {
        self.state() == FiberState::Active
    }

    /// Block until the fiber reaches `state`, or `None` on timeout.
    /// The eventually-consistent counterpart of `state()` for use under
    /// concurrent lifecycle activity (sync counterpart of awaiting v4's
    /// fiber `inertia`).
    pub fn wait_for(&self, state: FiberState, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.state() == state {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        self.state() == state
    }

    /// The failure message when the fiber is in `Failed` state.
    pub fn error(&self) -> Option<String> {
        let fiber = self.upgrade()?;
        fiber
            .inner
            .lock()
            .unwrap()
            .error
            .as_ref()
            .map(|e| e.to_string())
    }

    /// The fiber's current config.
    pub fn config(&self) -> Option<Value> {
        let fiber = self.upgrade()?;
        Some(fiber.inner.lock().unwrap().config.clone())
    }

    /// Update the config and restart the fiber (v4 `fiber.update`).
    /// Clears a previous failure. Listeners on `internal/update` may
    /// transform the config through the middleware chain. When the
    /// component declares a schema, the (waterfall-transformed) config is
    /// validated and resolved first; a rejected config is an error and
    /// the fiber keeps its previous config and state.
    pub fn update(&self, config: impl Into<Value>) -> Result<(), CordisError> {
        let fiber = self.require("update a fiber")?;
        if matches!(fiber.inner.lock().unwrap().state, FiberState::Disposed) {
            return Err(CordisError::InactiveContext {
                action: "update a fiber",
                fiber: Some(fiber.name.clone()),
            });
        }
        let config = events::waterfall(
            &fiber.ctx().core,
            fiber.ctx(),
            events::INTERNAL_UPDATE,
            config.into(),
            Box::new(std::convert::identity),
        )?;
        // Resolve through the component's schema before committing: a
        // rejected update must not touch the running fiber.
        let config = {
            let component = fiber.inner.lock().unwrap().component.clone();
            match component.and_then(|p| p.schema()) {
                Some(schema) => schema.resolve(&config).map_err(|diagnostics| {
                    CordisError::ComponentError(ComponentError(
                        diagnostics
                            .iter()
                            .map(|d| d.to_string())
                            .collect::<Vec<_>>()
                            .join("; ")
                            .into(),
                    ))
                })?,
                None => config,
            }
        };
        {
            let mut inner = fiber.inner.lock().unwrap();
            inner.config = config;
            inner.error = None;
        }
        fiber.restart(&fiber.ctx().core)
    }

    /// Stop and start the fiber with the current config.
    pub fn restart(&self) -> Result<(), CordisError> {
        let fiber = self.require("restart a fiber")?;
        let result = fiber.restart(&fiber.ctx().core);
        if !crate::fiber::executing_op_on_this_thread() {
            fiber.ctx().core.quiesce();
        }
        result
    }

    /// Dispose the fiber: every effect is reverted in reverse order and
    /// dependent fibers deactivate. Returns false if already disposed.
    ///
    /// The synchronous contract: when this returns (with `true`), this
    /// fiber and every fiber the cascade touched have no lifecycle work
    /// in flight — dependents are already deactivated.
    pub fn dispose(&self) -> bool {
        match self.upgrade() {
            Some(fiber) => {
                let disposed = fiber.dispose(&fiber.ctx().core);
                // Skipped when called from inside a cascading disposer:
                // waiting for the outer op there would self-deadlock.
                if !crate::fiber::executing_op_on_this_thread() {
                    fiber.ctx().core.quiesce();
                }
                disposed
            }
            // Fully torn down and deregistered: idempotent success.
            None => false,
        }
    }
}

impl std::fmt::Debug for FiberHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => write!(f, "FiberHandle <{}#{}>", name, self.id().unwrap_or(0)),
            None => write!(f, "FiberHandle <disposed>"),
        }
    }
}
