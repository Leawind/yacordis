//! Service registry: provide / set / get / notify (cordis `ReflectService`).
//!
//! Services live in a global store keyed by `(name, isolate label)`;
//! each entry records the owning fiber so that withdrawing the provider
//! reverts the registration and deactivates dependents. Typed access is
//! a downcast on top of the raw channel, which is what crosses the
//! dynamic-component ABI.
//!
//! Visibility is a single rule: an entry is resolvable from a context
//! when the provider is an **active** fiber, or when the provider *is*
//! the requesting fiber (its own mid-body provides). No snapshot maps.

use std::any::Any;
use std::sync::{Arc, Mutex, Weak};

use serde_json::Value;

use crate::context::{Context, Core, IsolateLabel, ServiceKey};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::CordisError;
use crate::events;
use crate::fiber::{Fiber, FiberState, ServiceImpl, drive_ops};

/// Provide a service from `ctx`'s fiber, within `ctx`'s isolation group.
/// The withdrawal disposer routes into the fiber's current effect batch
/// (i.e. the component body that called us).
pub(crate) fn provide(
    core: &Arc<Core>,
    ctx: &Context,
    fiber: &Arc<Fiber>,
    name: String,
    value: Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let label = ctx.isolate_label(&name);
    check_service_value(core, &name, &label, &value)?;
    let impl_ = Arc::new(ServiceImpl {
        name: name.clone(),
        value: Mutex::new(value),
        owner: Arc::downgrade(fiber),
        owner_name: fiber.name.clone(),
        label: label.clone(),
    });
    let key = ServiceKey {
        name: name.clone(),
        label: label.clone(),
    };

    {
        let mut store = core.store.lock().unwrap();
        if let Some(existing) = store.get(&key) {
            return Err(CordisError::ServiceConflict {
                name,
                owner: existing.owner_name.clone(),
            });
        }
        store.insert(key, impl_.clone());
    }

    fiber.note_provided(&name, label.clone());

    let handle = EffectHandle::new(format!("ctx.provide({name})"));
    let disposer: crate::effect::Disposer = {
        let core = core.clone();
        let fiber = Arc::downgrade(fiber);
        let impl_ = impl_.clone();
        Box::new(move || withdraw(&core, &fiber, &impl_))
    };
    handle.push(disposer);
    attach_effect(fiber, handle);

    // Providing while already active (e.g. from an event listener) takes
    // effect immediately; during a body run, activation notifies instead.
    if fiber.state() == FiberState::Active {
        notify(core, &[(name, label)]);
    }
    Ok(())
}

fn withdraw(core: &Arc<Core>, fiber: &Weak<Fiber>, impl_: &Arc<ServiceImpl>) {
    // A withdrawn provider whose fiber is already gone has nothing left
    // to withdraw: its store entry and notification both die with it.
    if fiber.upgrade().is_none() {
        return;
    }
    {
        let mut store = core.store.lock().unwrap();
        let key = ServiceKey {
            name: impl_.name.clone(),
            label: impl_.label.clone(),
        };
        // Only remove if this exact registration is still current: a
        // re-provide after a reload owns the slot now.
        let stale = match store.get(&key) {
            Some(current) => Arc::ptr_eq(current, impl_),
            None => false,
        };
        if stale {
            store.remove(&key);
        } else {
            return;
        }
    }
    notify(core, &[(impl_.name.clone(), impl_.label.clone())]);
}

/// Replace the value of a service owned by the current fiber.
pub(crate) fn set(
    core: &Arc<Core>,
    ctx: &Context,
    name: &str,
    value: Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let label = ctx.isolate_label(name);
    let impl_ = core
        .store_impl(name, &label)
        .ok_or_else(|| CordisError::NotProvided {
            name: name.to_owned(),
        })?;
    let owner = impl_
        .owner
        .upgrade()
        .ok_or_else(|| CordisError::NotProvided {
            name: name.to_owned(),
        })?;
    let current = ctx.current_fiber();
    if current.as_ref().map(|f| f.id) != Some(owner.id) {
        return Err(CordisError::ForeignService {
            name: name.to_owned(),
            owner: impl_.owner_name.clone(),
        });
    }
    check_service_value(core, name, &label, &value)?;
    *impl_.value.lock().unwrap() = value;
    Ok(())
}

/// Resolve a service: one rule — present in the store under the
/// context's isolation label, and its provider is active **or** is the
/// requesting fiber itself (its own mid-body provides).
pub(crate) fn resolve(core: &Arc<Core>, ctx: &Context, name: &str) -> Option<Arc<ServiceImpl>> {
    let label = ctx.isolate_label(name);
    let impl_ = core.store_impl(name, &label)?;
    let owner = impl_.owner.upgrade()?;
    if owner.state() != FiberState::Active {
        // Visible only to the providing fiber's own body.
        if ctx.current_fiber().map(|f| f.id) != Some(owner.id) {
            return None;
        }
    }
    Some(impl_)
}

pub(crate) fn resolve_value(
    core: &Arc<Core>,
    ctx: &Context,
    name: &str,
) -> Option<Arc<dyn Any + Send + Sync>> {
    let base = resolve(core, ctx, name).map(|impl_| impl_.get_value())?;
    apply_intercepts(ctx, name, base)
}

/// Register an intercept override on the current fiber. The
/// entry is an effect: unloading the fiber removes it, keeping I2's
/// provable cleanup. Only JSON services observe intercepts.
pub(crate) fn set_intercept(
    fiber: &Arc<Fiber>,
    name: String,
    value: Value,
) -> Result<(), CordisError> {
    {
        let mut intercepts = fiber.intercepts.lock().unwrap();
        intercepts.insert(name.clone(), value);
    }
    let handle = crate::effect::EffectHandle::new(format!("ctx.intercept({name})"));
    let cleanup_fiber = fiber.clone();
    handle.push(Box::new(move || {
        if let Some(fiber) = cleanup_fiber
            .self_weak
            .get()
            .and_then(std::sync::Weak::upgrade)
        {
            fiber.intercepts.lock().unwrap().remove(&name);
        }
    }));
    crate::effect::attach_effect(fiber, handle);
    Ok(())
}

/// Register a value schema for a JSON service (the `check` predicate,
/// Rust-shaped): every subsequent provide/set of that service — in this
/// fiber's isolation group — must satisfy the schema. The registration
/// is an effect of the current fiber and dies with it.
pub(crate) fn provide_schema(
    core: &Arc<Core>,
    ctx: &Context,
    fiber: &Arc<Fiber>,
    name: String,
    schema: crate::schema::Schema,
) -> Result<(), CordisError> {
    let label = ctx.isolate_label(&name);
    let key = ServiceKey {
        name: name.clone(),
        label: label.clone(),
    };
    let node_ptr = schema.node_ptr();
    {
        let mut schemas = core.service_schemas.lock().unwrap();
        schemas.insert(key.clone(), schema);
    }
    let handle = crate::effect::EffectHandle::new(format!("ctx.provide_schema({name})"));
    let core = core.clone();
    handle.push(Box::new(move || {
        let mut schemas = core.service_schemas.lock().unwrap();
        // Remove only our own registration: a later re-registration by
        // another fiber owns the slot now.
        if schemas
            .get(&key)
            .map(|current| current.node_ptr() == node_ptr)
            .unwrap_or(false)
        {
            schemas.remove(&key);
        }
    }));
    crate::effect::attach_effect(fiber, handle);
    Ok(())
}

/// Enforce a registered service value schema (JSON services only —
/// typed services bypass, mirroring intercepts).
fn check_service_value(
    core: &Arc<Core>,
    name: &str,
    label: &IsolateLabel,
    value: &Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let schema = {
        let key = ServiceKey {
            name: name.to_owned(),
            label: label.clone(),
        };
        core.service_schemas.lock().unwrap().get(&key).cloned()
    };
    let Some(schema) = schema else {
        return Ok(());
    };
    let Ok(json_value) = value.clone().downcast::<Value>() else {
        return Ok(());
    };
    let diagnostics = schema.validate(&json_value);
    if diagnostics.is_empty() {
        return Ok(());
    }
    Err(CordisError::ComponentError(crate::error::ComponentError(
        format!(
            "service `{name}` rejected: {}",
            diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        )
        .into(),
    )))
}

/// Collect intercept values for `name` along the fiber chain, nearest
/// fiber first.
fn collect_intercepts(ctx: &Context, name: &str) -> Vec<Value> {
    let mut values = Vec::new();
    let mut current = ctx.fiber.upgrade();
    let mut hops = 0;
    while let Some(fiber) = current {
        if let Some(value) = fiber.intercepts.lock().unwrap().get(name) {
            values.push(value.clone());
        }
        current = fiber.parent.as_ref().and_then(std::sync::Weak::upgrade);
        hops += 1;
        if hops > 4096 {
            // Malformed parent chain: stop rather than spin forever.
            break;
        }
    }
    values
}

/// Intercept view (upstream PR #90/#206 adapted): JSON services
/// are merged with the intercept layers along the fiber chain, nearest
/// fiber winning. Objects merge shallowly (Object.assign semantics);
/// a non-object layer replaces the result entirely. Typed services are
/// returned untouched. View-layer only: no epoch or notify impact.
fn apply_intercepts(
    ctx: &Context,
    name: &str,
    base: Arc<dyn Any + Send + Sync>,
) -> Option<Arc<dyn Any + Send + Sync>> {
    let layers = collect_intercepts(ctx, name);
    if layers.is_empty() {
        return Some(base);
    }
    let Ok(base_value) = base.clone().downcast::<Value>() else {
        // Typed service: intercepts do not participate.
        return Some(base);
    };
    // Fold farthest → nearest so the nearest fiber wins.
    let mut merged = (*base_value).clone();
    for layer in layers.iter().rev() {
        match (&mut merged, layer) {
            (Value::Object(map), Value::Object(layer_map)) => {
                for (key, value) in layer_map {
                    map.insert(key.clone(), value.clone());
                }
            }
            _ => merged = layer.clone(),
        }
    }
    Some(Arc::new(merged))
}

/// Recompute dependencies of every fiber injecting any of the touched
/// `(name, label)` services, drive their activation transitions, and
/// announce each change on `internal/service`. Fibers in other isolation
/// groups are never disturbed.
pub(crate) fn notify(core: &Arc<Core>, touched: &[(String, IsolateLabel)]) {
    let names: Vec<String> = touched.iter().map(|(n, _)| n.clone()).collect();
    // Targeted lookup through the inject index (name → injecting
    // fibers); a fiber injecting several touched names is deduped and
    // checked once.
    let mut targets: Vec<Arc<Fiber>> = {
        let mut registry = core.registry.lock().unwrap();
        let mut out: Vec<Arc<Fiber>> = Vec::new();
        for name in &names {
            out.extend(registry.injectors_of(name));
        }
        out
    };
    targets.sort_by_key(|fiber| fiber.id);
    targets.dedup_by(|a, b| a.id == b.id);
    let mut ops = Vec::new();
    for fiber in targets {
        if fiber.check_impls(core, &names) {
            ops.extend(fiber.refresh_ops());
        }
    }
    drive_ops(ops);

    for (name, label) in touched {
        // The raw service channel carries `Arc<dyn Any>`; only services
        // whose value is a `serde_json::Value` surface on the event bus.
        let value = core
            .store_impl(name, label)
            .map(|impl_| impl_.get_value())
            .and_then(|v| v.downcast::<serde_json::Value>().ok())
            .map(|boxed| (*boxed).clone());
        events::emit_service(core, name, label, value);
    }

    // Service availability changed: pending fibers may now form (or
    // break) a declared-provides dependency cycle.
    crate::diagnose::report_cycles(core);
}
