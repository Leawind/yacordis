//! Service registry: provide / set / get / notify (cordis `ReflectService`).
//!
//! Services live in a global store keyed by name; each entry records the
//! owning fiber so that withdrawing the provider reverts the registration
//! and deactivates dependents. Typed access is a downcast on top of the
//! raw channel, which is what crosses the dynamic-plugin ABI.

use std::any::Any;
use std::sync::{Arc, Weak};

use crate::context::{Context, Core};
use crate::effect::{EffectHandle, attach_effect};
use crate::error::CordisError;
use crate::events;
use crate::fiber::{Fiber, FiberState, ServiceImpl, drive_ops};

/// Provide a service from `fiber`. The withdrawal disposer routes into the
/// fiber's current effect batch (i.e. the plugin body that called us).
pub(crate) fn provide(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    name: String,
    value: Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let impl_ = Arc::new(ServiceImpl {
        name: name.clone(),
        value: std::sync::Mutex::new(value),
        owner: Arc::downgrade(fiber),
        owner_name: fiber.name.clone(),
    });

    {
        let mut store = core.store.lock().unwrap();
        if let Some(existing) = store.get(&name) {
            return Err(CordisError::ServiceConflict {
                name,
                owner: existing.owner_name.clone(),
            });
        }
        store.insert(name.clone(), impl_.clone());
    }

    fiber.note_provided(&name, impl_.clone());

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
        notify(core, &[name]);
    }
    Ok(())
}

fn withdraw(core: &Arc<Core>, fiber: &Weak<Fiber>, impl_: &Arc<ServiceImpl>) {
    let Some(fiber) = fiber.upgrade() else { return };
    {
        let mut store = core.store.lock().unwrap();
        // Only remove if this exact registration is still current: a
        // re-provide after a reload owns the slot now.
        let stale = match store.get(&impl_.name) {
            Some(current) => Arc::ptr_eq(current, impl_),
            None => false,
        };
        if stale {
            store.remove(&impl_.name);
        } else {
            return;
        }
    }
    fiber.drop_visible(&impl_.name);
    notify(core, std::slice::from_ref(&impl_.name));
}

/// Replace the value of a service owned by the current fiber.
pub(crate) fn set(
    core: &Arc<Core>,
    ctx: &Context,
    name: &str,
    value: Arc<dyn Any + Send + Sync>,
) -> Result<(), CordisError> {
    let impl_ = core
        .store_impl(name)
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

/// Resolve a service: the fiber chain (visibility snapshots) first, then
/// the global store with a strict "provider is active" check.
pub(crate) fn resolve(core: &Arc<Core>, ctx: &Context, name: &str) -> Option<Arc<ServiceImpl>> {
    let mut fiber = ctx.current_fiber();
    while let Some(f) = fiber {
        if let Some(impl_) = f.visible(name) {
            return Some(impl_);
        }
        fiber = f.parent.as_ref().and_then(Weak::upgrade);
    }
    let impl_ = core.store_impl(name)?;
    let owner = impl_.owner.upgrade()?;
    if owner.state() != crate::fiber::FiberState::Active {
        return None;
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

/// Recompute dependencies of every fiber injecting any of `names`, drive
/// their activation transitions, and announce the change on
/// `internal/service`.
pub(crate) fn notify(core: &Arc<Core>, names: &[String]) {
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
        if fiber.check_impls(core, names) {
            ops.extend(fiber.refresh_ops());
        }
    }
    drive_ops(ops);

    for name in names {
        // The raw service channel carries `Arc<dyn Any>`; only services
        // whose value is a `serde_json::Value` surface on the event bus.
        let value = core
            .store_impl(name)
            .map(|impl_| impl_.get_value())
            .and_then(|v| v.downcast::<serde_json::Value>().ok())
            .map(|boxed| (*boxed).clone());
        events::emit_service(core, name, value);
    }
}
