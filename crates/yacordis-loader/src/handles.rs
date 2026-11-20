//! Host-side handle table: u64 ids the component sees, real cordis objects
//! the host keeps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use yacordis::{Context, EffectHandle, FiberHandle, WorkHandle};

pub(crate) enum HostObject {
    Context(Context),
    Fiber(FiberHandle),
    Effect(EffectHandle),
    Work(WorkHandle),
}

pub(crate) struct HandleTable {
    next: AtomicU64,
    map: Mutex<HashMap<u64, HostObject>>,
}

impl HandleTable {
    fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    /// The process-global handle table (components are process-wide).
    pub(crate) fn global() -> &'static Self {
        static HANDLES: OnceLock<HandleTable> = OnceLock::new();
        HANDLES.get_or_init(Self::new)
    }

    pub(crate) fn insert(&self, object: HostObject) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.map.lock().unwrap().insert(id, object);
        id
    }

    /// Look up a context handle. Entries whose fiber has been torn down
    /// are garbage-collected on sight, so the table cannot grow without
    /// bound across install/dispose cycles.
    pub(crate) fn get_context(&self, id: u64) -> Option<Context> {
        let mut map = self.map.lock().unwrap();
        match map.get(&id) {
            Some(HostObject::Context(ctx)) => {
                if ctx.fiber_alive() {
                    Some(ctx.clone())
                } else {
                    map.remove(&id);
                    None
                }
            }
            _ => None,
        }
    }

    /// Remove a handle, returning the owned effect/fiber/work handle
    /// when the entry held one. Context handles are simply forgotten.
    pub(crate) fn take_lifecycle(&self, id: u64) -> Option<LifecycleHandle> {
        match self.map.lock().unwrap().remove(&id)? {
            HostObject::Effect(e) => Some(LifecycleHandle::Effect(e)),
            HostObject::Fiber(f) => Some(LifecycleHandle::Fiber(f)),
            HostObject::Work(w) => Some(LifecycleHandle::Work(w)),
            HostObject::Context(_) => None,
        }
    }

    /// Dispose-time audit (I2, resource ownership — host half): remove every context
    /// handle bound to `fiber_id` and return how many were collected.
    /// `get_context` garbage-collects on sight; this closes the hole of
    /// handles never queried again after their fiber goes away.
    ///
    /// This relies on fiber ids being process-global (`Core::next_fiber_id`):
    /// with per-runtime counters, a dispose in one runtime would collect
    /// another runtime's live context handles.
    pub(crate) fn audit_fiber(&self, fiber_id: u64) -> usize {
        let mut map = self.map.lock().unwrap();
        let stale: Vec<u64> = map
            .iter()
            .filter_map(|(id, object)| match object {
                HostObject::Context(ctx) => {
                    let bound = ctx.fiber().id().unwrap_or(u64::MAX);
                    (bound == fiber_id).then_some(*id)
                }
                _ => None,
            })
            .collect();
        for id in &stale {
            map.remove(id);
        }
        stale.len()
    }
}

pub(crate) enum LifecycleHandle {
    Effect(EffectHandle),
    Fiber(FiberHandle),
    Work(WorkHandle),
}
