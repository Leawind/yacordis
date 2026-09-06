//! Host-side handle table: u64 ids the plugin sees, real cordis objects
//! the host keeps.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use cordis::{Context, EffectHandle, FiberHandle};

pub(crate) enum HostObject {
    Context(Context),
    Fiber(FiberHandle),
    Effect(EffectHandle),
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

    /// The process-global handle table (plugins are process-wide).
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

    /// Remove a handle, returning the owned effect/fiber handle when the
    /// entry held one. Context handles are simply forgotten.
    pub(crate) fn take_lifecycle(&self, id: u64) -> Option<LifecycleHandle> {
        match self.map.lock().unwrap().remove(&id)? {
            HostObject::Effect(e) => Some(LifecycleHandle::Effect(e)),
            HostObject::Fiber(f) => Some(LifecycleHandle::Fiber(f)),
            HostObject::Context(_) => None,
        }
    }
}

pub(crate) enum LifecycleHandle {
    Effect(EffectHandle),
    Fiber(FiberHandle),
}
