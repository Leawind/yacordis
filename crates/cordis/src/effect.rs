//! Revertible effects: the temporal composability primitive.
//!
//! A cordis context transformation carries its own inverse: setup code
//! registers disposers while it runs, and the runtime guarantees they are
//! executed in reverse registration order when the surrounding scope is
//! unloaded. Event listeners, provided services and child plugins are all
//! built on this single mechanism.
//!
//! Nesting matches cordis v4: an effect registered while another effect's
//! setup runs is claimed by the outer effect — disposing the outer one
//! reverts the inner as well. On fiber unload everything flattens to
//! strict reverse registration order.

use std::any::Any;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::context::Context;
use crate::error::{CordisError, PluginError};
use crate::fiber::Fiber;
use crate::plugin::Plugin;

/// A single cleanup function recorded by an effect.
///
/// Disposers must not panic; panics are caught at the disposal site,
/// logged, and the remaining disposers still run.
pub type Disposer = Box<dyn FnOnce() + Send>;

/// What an effect setup hands back to the runtime in addition to the
/// disposers it recorded through the context.
#[derive(Default)]
pub enum EffectOutcome {
    /// No cleanup required.
    #[default]
    None,
    /// A single disposer.
    One(Disposer),
    /// Several disposers, disposed in reverse order.
    Many(Vec<Disposer>),
}

impl From<Disposer> for EffectOutcome {
    fn from(d: Disposer) -> Self {
        EffectOutcome::One(d)
    }
}

impl From<Option<Disposer>> for EffectOutcome {
    fn from(d: Option<Disposer>) -> Self {
        match d {
            Some(d) => EffectOutcome::One(d),
            None => EffectOutcome::None,
        }
    }
}

impl From<Vec<Disposer>> for EffectOutcome {
    fn from(mut v: Vec<Disposer>) -> Self {
        match v.len() {
            0 => EffectOutcome::None,
            1 => EffectOutcome::One(v.pop().expect("len == 1")),
            _ => EffectOutcome::Many(v),
        }
    }
}

impl From<()> for EffectOutcome {
    fn from((): ()) -> Self {
        EffectOutcome::None
    }
}

impl EffectOutcome {
    pub(crate) fn into_disposers(self) -> Vec<Disposer> {
        match self {
            EffectOutcome::None => Vec::new(),
            EffectOutcome::One(d) => vec![d],
            EffectOutcome::Many(v) => v,
        }
    }
}

/// Run disposers in reverse order, catching panics so that one failing
/// disposer cannot skip the remaining cleanup.
pub(crate) fn run_disposers(disposers: Vec<Disposer>) {
    for disposer in disposers.into_iter().rev() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(disposer));
    }
}

pub(crate) fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// A handle that disposes one effect early, before its fiber unloads.
///
/// Disposing twice is a no-op. After the owning scope unloads, the
/// handle does nothing: the effect has already been reverted.
#[derive(Clone)]
pub struct EffectHandle {
    batch: Arc<Mutex<Vec<Disposer>>>,
    alive: Arc<AtomicBool>,
    label: Arc<str>,
}

impl EffectHandle {
    pub(crate) fn new(label: impl Into<String>) -> Self {
        Self {
            batch: Arc::new(Mutex::new(Vec::new())),
            alive: Arc::new(AtomicBool::new(true)),
            label: Arc::from(label.into().as_str()),
        }
    }

    /// The label this effect was registered with.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Whether this effect has not been disposed yet.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Run this effect's disposers in reverse order.
    pub fn dispose(&self) {
        if !self.alive.swap(false, Ordering::AcqRel) {
            return;
        }
        let disposers = self.batch.lock().unwrap().drain(..).collect::<Vec<_>>();
        run_disposers(disposers);
    }

    pub(crate) fn push(&self, disposer: Disposer) {
        self.batch.lock().unwrap().push(disposer);
    }

    pub(crate) fn push_all(&self, disposers: Vec<Disposer>) {
        self.batch.lock().unwrap().extend(disposers);
    }

    pub(crate) fn take(&self) -> Vec<Disposer> {
        self.batch.lock().unwrap().drain(..).collect()
    }
}

impl std::fmt::Debug for EffectHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectHandle")
            .field("label", &self.label)
            .field("alive", &self.is_alive())
            .finish()
    }
}

/// Disposers recorded while a setup/body runs on fiber F are funneled
/// into the innermost open effect batch of F. The stack is thread-local
/// because execution is synchronous: whoever started the effect finishes
/// it on the same thread.
struct EffectScope {
    fiber_id: u64,
    batch: Arc<Mutex<Vec<Disposer>>>,
}

thread_local! {
    static SCOPES: RefCell<Vec<EffectScope>> = const { RefCell::new(Vec::new()) };
}

fn innermost_scope_of(fiber_id: u64) -> Option<Arc<Mutex<Vec<Disposer>>>> {
    SCOPES.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|scope| scope.fiber_id == fiber_id)
            .map(|scope| scope.batch.clone())
    })
}

/// Attach a fully-built effect to its fiber: claimed by the enclosing
/// effect when one is open, otherwise appended to the fiber's effect
/// list.
pub(crate) fn attach_effect(fiber: &Arc<Fiber>, handle: EffectHandle) {
    match innermost_scope_of(fiber.id()) {
        Some(batch) => {
            let claimed = handle.clone();
            batch
                .lock()
                .unwrap()
                .push(Box::new(move || claimed.dispose()));
        }
        None => fiber.push_effect_batch(handle),
    }
}

/// Open an effect batch for a plugin body run and collect everything it
/// records.
pub(crate) struct ScopeGuard;

impl ScopeGuard {
    pub(crate) fn collect<T>(fiber: &Arc<Fiber>, f: impl FnOnce() -> T) -> (EffectHandle, T) {
        let handle = EffectHandle::new("plugin body");
        SCOPES.with(|s| {
            s.borrow_mut().push(EffectScope {
                fiber_id: fiber.id(),
                batch: handle.batch.clone(),
            });
        });
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        SCOPES.with(|s| {
            s.borrow_mut().pop();
        });
        match out {
            Ok(value) => (handle, value),
            Err(payload) => {
                run_disposers(handle.take());
                std::panic::resume_unwind(payload);
            }
        }
    }
}

/// Invoke a plugin body, converting panics into plugin errors so a broken
/// plugin fails its own fiber.
pub(crate) fn run_plugin_apply(
    plugin: &Arc<dyn Plugin>,
    ctx: &Context,
    config: &Value,
) -> Result<(), PluginError> {
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| plugin.apply(ctx, config)));
    match result {
        Ok(inner) => inner,
        Err(payload) => Err(PluginError(
            format!("plugin panicked: {}", panic_message(&payload)).into(),
        )),
    }
}

impl Context {
    /// Register a revertible effect on the current fiber.
    ///
    /// The setup runs immediately; every disposer it records — directly
    /// or through nested `on`/`provide`/`effect` calls on the same fiber —
    /// is collected. Disposers run in reverse registration order when the
    /// effect is disposed or its fiber unloads.
    ///
    /// A setup that returns `Err` has whatever it already recorded
    /// reverted; the error surfaces as [`CordisError::PluginError`].
    ///
    /// Returns [`CordisError::InactiveContext`] if the current fiber is
    /// disposed or unloading.
    pub fn effect<E>(
        &self,
        label: impl Into<String>,
        setup: impl FnOnce(&Context) -> Result<E, Box<dyn std::error::Error + Send + Sync>>,
    ) -> Result<EffectHandle, CordisError>
    where
        E: Into<EffectOutcome>,
    {
        let fiber = self.active_fiber("register an effect")?;
        let handle = EffectHandle::new(label.into());

        SCOPES.with(|s| {
            s.borrow_mut().push(EffectScope {
                fiber_id: fiber.id(),
                batch: handle.batch.clone(),
            });
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| setup(self)));
        SCOPES.with(|s| {
            s.borrow_mut().pop();
        });

        match outcome {
            Ok(Ok(effect)) => {
                handle.push_all(effect.into().into_disposers());
                attach_effect(&fiber, handle.clone());
                Ok(handle)
            }
            Ok(Err(err)) => {
                // Setup failed: revert whatever it already recorded.
                run_disposers(handle.take());
                Err(CordisError::PluginError(PluginError(err)))
            }
            Err(payload) => {
                run_disposers(handle.take());
                std::panic::resume_unwind(payload);
            }
        }
    }
}
