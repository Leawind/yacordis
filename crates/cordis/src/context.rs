//! The context: a single intermediary for effects and coeffects.
//!
//! A `Context` is a cheap handle (clone it freely) pairing the shared
//! [`Core`] with the fiber whose scope it belongs to. Service resolution
//! walks the fiber parent chain — a plugin sees its own provided
//! services, its ancestors', and finally the global store.

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;

use crate::effect::{EffectHandle, attach_effect};
use crate::error::{CordisError, PluginResult};
use crate::events;
use crate::fiber::{Fiber, FiberState, ServiceImpl, disposal_disposer};
use crate::logger::{Logger, LoggerHandle, StderrLogger};
use crate::plugin::{Inject, InjectClosure, Plugin};
use crate::registry::{Registry, plugin_key};

pub(crate) struct Core {
    fiber_sn: AtomicU64,
    hook_sn: AtomicU64,
    pub(crate) root: Arc<Fiber>,
    pub(crate) registry: Mutex<Registry>,
    pub(crate) hooks: Mutex<HashMap<String, Vec<events::Hook>>>,
    pub(crate) middlewares: Mutex<HashMap<String, Vec<events::MiddlewareEntry>>>,
    pub(crate) store: Mutex<HashMap<String, Arc<ServiceImpl>>>,
    pub(crate) logger: LoggerHandle,
}

impl Core {
    fn new() -> Arc<Core> {
        let root = Fiber::new_root();
        let core = Arc::new(Core {
            fiber_sn: AtomicU64::new(1),
            hook_sn: AtomicU64::new(1),
            root: root.clone(),
            registry: Mutex::new(Registry::default()),
            hooks: Mutex::new(HashMap::new()),
            middlewares: Mutex::new(HashMap::new()),
            store: Mutex::new(HashMap::new()),
            logger: LoggerHandle::new(Arc::new(StderrLogger)),
        });
        let _ = root.ctx.set(Context {
            core: core.clone(),
            fiber: Arc::downgrade(&root),
        });
        core
    }

    pub(crate) fn next_fiber_id(&self) -> u64 {
        self.fiber_sn.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn next_hook_id(&self) -> u64 {
        self.hook_sn.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn store_impl(&self, name: &str) -> Option<Arc<ServiceImpl>> {
        self.store.lock().unwrap().get(name).cloned()
    }

    pub(crate) fn root_weak(&self) -> Weak<Fiber> {
        Arc::downgrade(&self.root)
    }
}

/// A scoped handle onto the cordis runtime: entry point for installing
/// plugins, providing services, registering events and effects.
#[derive(Clone)]
pub struct Context {
    pub(crate) core: Arc<Core>,
    pub(crate) fiber: Weak<Fiber>,
}

impl Context {
    /// Create a fresh runtime with an active root context.
    pub fn new() -> Context {
        let core = Core::new();
        Context {
            fiber: Arc::downgrade(&core.root),
            core,
        }
    }

    pub(crate) fn for_fiber(core: &Arc<Core>, fiber: &Arc<Fiber>) -> Context {
        Context {
            core: core.clone(),
            fiber: Arc::downgrade(fiber),
        }
    }

    /// The root context: outside every plugin.
    pub fn root(&self) -> Context {
        Context {
            core: self.core.clone(),
            fiber: Arc::downgrade(&self.core.root),
        }
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
    // Plugins
    // -----------------------------------------------------------------

    /// Install a plugin on the current fiber and run its body (when its
    /// required services are available).
    ///
    /// Body failures do not error here: the fiber enters
    /// [`FiberState::Failed`] and the error surfaces from
    /// [`FiberHandle::update`] / [`FiberHandle::restart`].
    pub fn plugin(
        &self,
        plugin: impl Plugin,
        config: impl Into<Value>,
    ) -> Result<FiberHandle, CordisError> {
        self.plugin_dyn(Arc::new(plugin), config.into())
    }

    /// Install an already-erased plugin. This is the entry point used by
    /// the dynamic loader for cdylib plugins.
    pub fn plugin_dyn(
        &self,
        plugin: Arc<dyn Plugin>,
        config: impl Into<Value>,
    ) -> Result<FiberHandle, CordisError> {
        let parent = self.active_fiber("install a plugin")?;
        let config = config.into();
        let name = plugin.name().unwrap_or("anonymous").to_owned();

        // Get or create the runtime entry for this plugin identity.
        let key = plugin_key(&plugin);
        let runtime = {
            let mut registry = self.core.registry.lock().unwrap();
            registry.entry(key, name.clone(), &plugin)
        };

        let fiber = Fiber::install(&self.core, Arc::downgrade(&parent), plugin, config, name);
        let _ = fiber.runtime.set(runtime.clone());
        let _ = fiber.ctx.set(Context::for_fiber(&self.core, &fiber));
        runtime.fibers.lock().unwrap().push(fiber.clone());

        // Register disposal with the parent fiber: when the parent
        // unloads, this fiber is torn down with it (cascade). Claimed by
        // the parent's open effect batch when installing from inside
        // another plugin body.
        let handle = EffectHandle::new("ctx.plugin()");
        handle.push(disposal_disposer(&fiber));
        attach_effect(&parent, handle);

        fiber.run_install_effect(&self.core);
        Ok(FiberHandle {
            fiber: Arc::downgrade(&fiber),
        })
    }

    /// Sugar for a closure plugin with declared dependencies:
    /// `ctx.inject(&["database"], |ctx| { … })`.
    pub fn inject<F>(&self, deps: Inject, f: F) -> Result<FiberHandle, CordisError>
    where
        F: Fn(&Context) -> PluginResult + Send + Sync + 'static,
    {
        self.plugin_dyn(Arc::new(InjectClosure { inject: deps, f }), Value::Null)
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
        crate::reflect::provide(&self.core, &fiber, name.into(), value)
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

    /// Resolve a service by name, walking the fiber chain first.
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
        events::middleware(&self.core, &fiber, name.into(), Arc::new(mw))
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
                "cordis/events",
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

    #[allow(dead_code)] // exercised by tests and future modules
    pub(crate) fn log_error(&self, target: &str, error: &(dyn std::error::Error + 'static)) {
        self.core.logger.log_error(target, error);
    }

    /// Dispose every installed plugin fiber (root effects survive; this is
    /// a teardown helper for hosts and tests).
    pub fn dispose_all(&self) {
        let fibers: Vec<Arc<Fiber>> = {
            let registry = self.core.registry.lock().unwrap();
            registry.all_fibers()
        };
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

/// Handle that removes a listener (an alias: listeners *are* effects).
pub type ListenerHandle = EffectHandle;

/// Handle onto an installed plugin fiber: inspect state, hot-update its
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

    /// The plugin name, or `None` when the fiber is gone.
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
    /// transform the config through the middleware chain.
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
        fiber.restart(&fiber.ctx().core)
    }

    /// Dispose the fiber: every effect is reverted in reverse order and
    /// dependent fibers deactivate. Returns false if already disposed.
    pub fn dispose(&self) -> bool {
        match self.upgrade() {
            Some(fiber) => fiber.dispose(&fiber.ctx().core),
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
