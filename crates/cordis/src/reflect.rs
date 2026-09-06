//! Service registry: provide / set / get / notify (cordis `ReflectService`).
//!
//! Services live in a global store keyed by `(name, isolate label)`;
//! each entry records the owning fiber so that withdrawing the provider
//! reverts the registration and deactivates dependents. Typed access is
//! a downcast on top of the raw channel, which is what crosses the
//! dynamic-plugin ABI.
//!
//! Visibility is a single rule: an entry is resolvable from a context
//! when the provider is an **active** fiber, or when the provider *is*
//! the requesting fiber (its own mid-body provides). No snapshot maps.

use std::any::Any;
use std::sync::{Arc, Mutex, Weak};

use crate::context::{Context, Core, IsolateLabel, ServiceKey};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::CordisError;
use crate::events;
use crate::fiber::{Fiber, FiberState, ServiceImpl, drive_ops};

/// Provide a service from `ctx`'s fiber, within `ctx`'s isolation group.
/// The withdrawal disposer routes into the fiber's current effect batch
/// (i.e. the plugin body that called us).
pub(crate) fn provide(
    core: &Arc<Core>,
    ctx: &Context,
    fiber: &Arc<Fiber>,
    name: String,
    value: Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let label = ctx.isolate_label(&name);
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
    let Some(fiber) = fiber.upgrade() else { return };
    let _ = &fiber;
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
    resolve(core, ctx, name).map(|impl_| impl_.get_value())
}

/// Recompute dependencies of every fiber injecting any of the touched
/// `(name, label)` services, drive their activation transitions, and
/// announce each change on `internal/service`. Fibers in other isolation
/// groups are never disturbed.
pub(crate) fn notify(core: &Arc<Core>, touched: &[(String, IsolateLabel)]) {
    let names: Vec<String> = touched.iter().map(|(n, _)| n.clone()).collect();
    let targets: Vec<Arc<Fiber>> = {
        let registry = core.registry.lock().unwrap();
        registry
            .all_fibers()
            .into_iter()
            .filter(|fiber| {
                let inject = fiber.inject_names();
                names
                    .iter()
                    .any(|name| inject.iter().any(|(n, _)| n == name))
            })
            .collect()
    };
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
        events::emit_service(core, name, value);
    }

    // Service availability changed: pending fibers may now form (or
    // break) a declared-provides dependency cycle.
    crate::diagnose::report_cycles(core);
}
