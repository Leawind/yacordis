//! The event system: five dispatch modes from cordis, reduced to the
//! synchronous three.
//!
//! - `emit`: run every listener in registration order, ignore results.
//! - `bail`: run in order, short-circuit on the first `Some` (this is
//!   v4's `bail`/`serial`; the async `parallel`/`serial` aggregation
//!   arrives with the async milestone).
//! - `waterfall`: a middleware chain; each middleware may transform the
//!   value and decide whether to delegate to `next`.
//!
//! Listeners are string-keyed and receive a `serde_json::Value` payload;
//! the [`Event`] trait adds a typed, compile-checked layer on top. All
//! dispatches snapshot the hook list and invoke user code with no locks
//! held, which is what keeps FFI component callbacks re-entrant. Removal is
//! honored inside an active dispatch: each snapshot copy observes a
//! shared liveness flag, so a listener removed mid-dispatch is skipped
//! in the same cycle (cordis TS #242).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::context::{Context, Core};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::CordisError;
use crate::fiber::Fiber;

pub type Listener = Arc<dyn Fn(&Context, &Value) -> Option<Value> + Send + Sync>;
/// The chain continuation handed to a [`Middleware`].
pub type Next<'a> = Box<dyn FnOnce(Value) -> Result<Value, CordisError> + 'a>;
/// A middleware participates in [`Context::waterfall`]: transform the
/// value and delegate via `next(value)?`, or short-circuit with a value.
pub type Middleware =
    Arc<dyn for<'a> Fn(&Context, Value, Next<'a>) -> Result<Value, CordisError> + Send + Sync>;

pub(crate) struct Hook {
    pub id: u64,
    /// Liveness guard shared with every snapshot copy of the listener;
    /// flipped by [`remove_hook`] so active dispatches skip removals.
    pub live: Arc<AtomicBool>,
    pub listener: Listener,
    /// The isolation map of the context that registered the listener.
    /// `internal/service` dispatches compare it against the touched
    /// service's label, so service events stay inside their isolation
    /// group (upstream filters them the same way).
    pub isolate: Arc<crate::context::IsolateMap>,
}

pub(crate) struct MiddlewareEntry {
    pub id: u64,
    pub mw: Middleware,
}

/// Upstream cordis#307 renamed plugin→component but kept this event
/// name, so the string stays as-is.
pub(crate) const INTERNAL_COMPONENT: &str = "internal/plugin";
pub(crate) const INTERNAL_STATUS: &str = "internal/status";
pub(crate) const INTERNAL_SERVICE: &str = "internal/service";
pub(crate) const INTERNAL_UPDATE: &str = "internal/update";
pub(crate) const INTERNAL_REPLACE: &str = "internal/replace";
pub(crate) const INTERNAL_DISPATCH: &str = "internal/dispatch";

fn snapshot(core: &Arc<Core>, name: &str) -> Vec<Listener> {
    let hooks = core.hooks.lock().unwrap();
    hooks
        .get(name)
        .map(|list| list.iter().map(|hook| hook.listener.clone()).collect())
        .unwrap_or_default()
}

fn call(
    core: &Arc<Core>,
    target: &str,
    listener: &Listener,
    ctx: &Context,
    args: &Value,
) -> Option<Value> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(ctx, args)));
    match result {
        Ok(value) => value,
        Err(payload) => {
            let message = crate::effect::panic_message(&payload);
            core.logger.log(
                crate::logger::LogLevel::Error,
                target,
                &format!("listener panicked: {message}"),
            );
            None
        }
    }
}

/// Snapshot-and-call dispatch. Non-internal events are announced to
/// `internal/dispatch` observers first (cordis `internal/dispatch`).
fn resolve_dispatch(
    core: &Arc<Core>,
    ctx: &Context,
    mode: &str,
    name: &str,
    args: &Value,
) -> Vec<Listener> {
    if !name.starts_with("internal/") {
        for observer in snapshot(core, INTERNAL_DISPATCH) {
            call(
                core,
                "yacordis/dispatch",
                &observer,
                ctx,
                &json!([mode, name, args]),
            );
        }
    }
    snapshot(core, name)
}

pub(crate) fn emit(core: &Arc<Core>, ctx: &Context, name: &str, args: &Value) {
    for listener in resolve_dispatch(core, ctx, "emit", name, args) {
        let _ = call(core, "yacordis/emit", &listener, ctx, args);
    }
}

pub(crate) fn bail(core: &Arc<Core>, ctx: &Context, name: &str, args: &Value) -> Option<Value> {
    for listener in resolve_dispatch(core, ctx, "bail", name, args) {
        if let Some(value) = call(core, "yacordis/bail", &listener, ctx, args) {
            return Some(value);
        }
    }
    None
}

pub(crate) fn waterfall(
    core: &Arc<Core>,
    ctx: &Context,
    name: &str,
    init: Value,
    fallback: Box<dyn FnOnce(Value) -> Value + '_>,
) -> Result<Value, CordisError> {
    let middlewares: Vec<Middleware> = core
        .middlewares
        .lock()
        .unwrap()
        .get(name)
        .map(|list| list.iter().map(|entry| entry.mw.clone()).collect())
        .unwrap_or_default();
    run_chain(core, ctx, name, &middlewares, init, fallback)
}

fn run_chain<'a>(
    core: &'a Arc<Core>,
    ctx: &'a Context,
    name: &'a str,
    middlewares: &'a [Middleware],
    value: Value,
    fallback: Box<dyn FnOnce(Value) -> Value + 'a>,
) -> Result<Value, CordisError> {
    let Some((first, rest)) = middlewares.split_first() else {
        return Ok(fallback(value));
    };
    let next: Next<'a> = Box::new(move |value| run_chain(core, ctx, name, rest, value, fallback));
    // Keep a copy of the value: a panicking middleware consumes its
    // arguments during the unwind, and the chain continuation (`next`)
    // goes down with it.
    let call_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        first(ctx, value.clone(), next)
    }));
    match call_result {
        Ok(value) => value,
        Err(payload) => {
            let message = crate::effect::panic_message(&payload);
            core.logger.log(
                crate::logger::LogLevel::Error,
                "yacordis/waterfall",
                &format!("middleware panicked: {message}; chain aborted (event: {name})"),
            );
            Err(CordisError::ComponentError(crate::error::ComponentError(
                format!("middleware panicked: {message}").into(),
            )))
        }
    }
}

// ---------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------

fn register_hook(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    name: String,
    id: u64,
    listener: Listener,
    prepend: bool,
) -> EffectHandle {
    // Liveness guard: once removed, the listener returns `None` without
    // running. For `emit` this skips it entirely; for `bail` this keeps
    // the dispatch moving to the next listener — both match "honor
    // listener removal in active dispatch snapshots" (upstream #242).
    let live = Arc::new(AtomicBool::new(true));
    let live_guard = live.clone();
    let guarded: Listener = Arc::new(move |ctx, args| {
        if live_guard.load(Ordering::Acquire) {
            listener(ctx, args)
        } else {
            None
        }
    });
    {
        let mut hooks = core.hooks.lock().unwrap();
        let list = hooks.entry(name.clone()).or_default();
        let hook = Hook {
            id,
            listener: guarded,
            live,
            isolate: fiber.ctx().isolate.clone(),
        };
        if prepend {
            list.insert(0, hook);
        } else {
            list.push(hook);
        }
    }
    let core2 = core.clone();
    let name2 = name.clone();
    let handle = EffectHandle::new(format!("ctx.on({name2})"));
    handle.push(Box::new(move || {
        remove_hook(&core2, &name2, id);
    }));
    attach_effect(fiber, handle.clone());
    handle
}

pub(crate) fn remove_hook(core: &Arc<Core>, name: &str, id: u64) -> bool {
    let mut hooks = core.hooks.lock().unwrap();
    let Some(list) = hooks.get_mut(name) else {
        return false;
    };
    let before = list.len();
    // Flip liveness before dropping the entries: listeners already cloned
    // into an active dispatch observe the removal and skip themselves.
    for hook in list.iter_mut().filter(|hook| hook.id == id) {
        hook.live.store(false, Ordering::Release);
    }
    list.retain(|hook| hook.id != id);
    let removed = before != list.len();
    let now_empty = list.is_empty();
    if now_empty {
        hooks.remove(name);
    }
    removed
}

pub(crate) fn on(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    name: String,
    listener: Listener,
    prepend: bool,
) -> Result<EffectHandle, CordisError> {
    let id = core.next_hook_id();
    Ok(register_hook(core, fiber, name, id, listener, prepend))
}

pub(crate) fn once(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    name: String,
    listener: Listener,
) -> Result<EffectHandle, CordisError> {
    let id = core.next_hook_id();
    let core2 = core.clone();
    let name2 = name.clone();
    let wrapped: Listener = Arc::new(move |ctx, args| {
        remove_hook(&core2, &name2, id);
        listener(ctx, args)
    });
    Ok(register_hook(core, fiber, name, id, wrapped, false))
}

/// Register a middleware on the waterfall chain for `name`. With
/// `prepend` the entry is inserted at the head (upstream
/// `DisposableList.unshift`, PR #296): prepended entries run before
/// appended ones, most recently prepended first; appended entries keep
/// registration order.
pub(crate) fn middleware(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    name: String,
    mw: Middleware,
    prepend: bool,
) -> Result<EffectHandle, CordisError> {
    let id = core.next_hook_id();
    {
        let mut middlewares = core.middlewares.lock().unwrap();
        let list = middlewares.entry(name.clone()).or_default();
        if prepend {
            list.insert(0, MiddlewareEntry { id, mw });
        } else {
            list.push(MiddlewareEntry { id, mw });
        }
    }
    let core2 = core.clone();
    let name2 = name.clone();
    let handle = EffectHandle::new(format!("ctx.middleware({name})"));
    handle.push(Box::new(move || {
        let mut middlewares = core2.middlewares.lock().unwrap();
        if let Some(list) = middlewares.get_mut(&name2) {
            list.retain(|entry| entry.id != id);
            if list.is_empty() {
                middlewares.remove(&name2);
            }
        }
    }));
    attach_effect(fiber, handle.clone());
    Ok(handle)
}

// ---------------------------------------------------------------------
// Internal event emitters
// ---------------------------------------------------------------------

fn root_ctx(core: &Arc<Core>) -> Context {
    Context {
        core: core.clone(),
        fiber: core.root_weak(),
        isolate: Arc::new(crate::context::IsolateMap::default()),
    }
}

pub(crate) fn emit_component(core: &Arc<Core>, fiber: &Fiber) {
    let ctx = root_ctx(core);
    emit(core, &ctx, INTERNAL_COMPONENT, &json!([fiber.summary()]));
}

pub(crate) fn emit_replace(core: &Arc<Core>, fiber: &Fiber) {
    let ctx = root_ctx(core);
    emit(core, &ctx, INTERNAL_REPLACE, &json!([fiber.summary()]));
}

pub(crate) fn emit_status(core: &Arc<Core>, fiber: &Fiber, old: crate::fiber::FiberState) {
    let ctx = root_ctx(core);
    emit(
        core,
        &ctx,
        INTERNAL_STATUS,
        &json!([fiber.summary(), old.as_str()]),
    );
}

/// Announce a service change on `internal/service`. Only listeners whose
/// registering context shares the touched service's isolation group
/// observe the event — the same per-group visibility as resolution
/// itself (upstream dispatches with an equivalent filter).
pub(crate) fn emit_service(
    core: &Arc<Core>,
    name: &str,
    label: &crate::context::IsolateLabel,
    value: Option<Value>,
) {
    let listeners: Vec<Listener> = {
        let hooks = core.hooks.lock().unwrap();
        hooks
            .get(INTERNAL_SERVICE)
            .map(|list| {
                list.iter()
                    .filter(|hook| hook.isolate.label(name) == *label)
                    .map(|hook| hook.listener.clone())
                    .collect()
            })
            .unwrap_or_default()
    };
    let ctx = root_ctx(core);
    for listener in &listeners {
        call(
            core,
            "yacordis/events",
            listener,
            &ctx,
            &json!([name, value]),
        );
    }
}

// ---------------------------------------------------------------------
// Typed events
// ---------------------------------------------------------------------

/// Compile-checked event definition over the string-keyed bus.
pub trait Event {
    /// The string key this event dispatches under.
    const NAME: &'static str;
    /// Payload type handed to listeners.
    type Args: serde::Serialize + serde::de::DeserializeOwned;
    /// Result type produced by bail semantics.
    type Ret: serde::Serialize + serde::de::DeserializeOwned;
}

impl Context {
    /// Register a listener for a typed event.
    pub fn on_event<E: Event>(
        &self,
        listener: impl Fn(&Context, &E::Args) -> Option<E::Ret> + Send + Sync + 'static,
    ) -> Result<EffectHandle, CordisError> {
        self.on(E::NAME, move |ctx, raw| {
            let Ok(args) = serde_json::from_value::<E::Args>(raw.clone()) else {
                return None;
            };
            listener(ctx, &args).map(|ret| serde_json::to_value(ret).unwrap_or(Value::Null))
        })
    }

    /// Register a one-shot listener for a typed event.
    pub fn once_event<E: Event>(
        &self,
        listener: impl Fn(&Context, &E::Args) -> Option<E::Ret> + Send + Sync + 'static,
    ) -> Result<EffectHandle, CordisError> {
        self.once(E::NAME, move |ctx, raw| {
            let Ok(args) = serde_json::from_value::<E::Args>(raw.clone()) else {
                return None;
            };
            listener(ctx, &args).map(|ret| serde_json::to_value(ret).unwrap_or(Value::Null))
        })
    }

    /// Emit a typed event.
    pub fn emit_event<E: Event>(&self, args: &E::Args) {
        match serde_json::to_value(args) {
            Ok(value) => self.emit(E::NAME, &value),
            Err(_) => self.core.logger.log(
                crate::logger::LogLevel::Warn,
                "yacordis/events",
                "failed to serialize event args",
            ),
        }
    }

    /// Emit a typed event with bail semantics.
    pub fn bail_event<E: Event>(&self, args: &E::Args) -> Option<E::Ret> {
        let value = serde_json::to_value(args).ok()?;
        self.bail(E::NAME, &value)
            .and_then(|ret| serde_json::from_value(ret).ok())
    }
}
