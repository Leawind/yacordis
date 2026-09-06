//! Plugin registry: identity-keyed runtimes aggregating installed fibers
//! (cordis `RegistryService`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::context::Core;
use crate::fiber::Fiber;
use crate::plugin::Plugin;

/// Identity of a plugin: the data address of its `Arc<dyn Plugin>`.
///
/// Two installs of the same `Arc` share one runtime (v4 keys by the
/// resolved plugin function); re-wrapping the same value in a fresh `Arc`
/// creates a distinct runtime, which is the meaningful notion of plugin
/// identity for erased, dynamically loaded plugins.
pub(crate) type PluginKey = usize;

pub(crate) fn plugin_key(plugin: &Arc<dyn Plugin>) -> PluginKey {
    // Drop the vtable, keep the data pointer: stable for the Arc's lifetime.
    Arc::as_ptr(plugin) as *const () as usize
}

pub(crate) struct Runtime {
    pub key: PluginKey,
    #[allow(dead_code)] // introspection; fibers carry their own copy
    pub name: String,
    pub fibers: Mutex<Vec<Arc<Fiber>>>,
}

#[derive(Default)]
pub(crate) struct Registry {
    map: HashMap<PluginKey, Arc<Runtime>>,
}

impl Registry {
    pub fn entry(
        &mut self,
        key: PluginKey,
        name: String,
        _plugin: &Arc<dyn Plugin>,
    ) -> Arc<Runtime> {
        self.map
            .entry(key)
            .or_insert_with(|| {
                Arc::new(Runtime {
                    key,
                    name,
                    fibers: Mutex::new(Vec::new()),
                })
            })
            .clone()
    }

    pub fn all_fibers(&self) -> Vec<Arc<Fiber>> {
        let mut out = Vec::new();
        for runtime in self.map.values() {
            out.extend(runtime.fibers.lock().unwrap().iter().cloned());
        }
        out
    }

    #[allow(dead_code)] // used by tests
    pub fn runtime_count(&self) -> usize {
        self.map.len()
    }

    fn remove(&mut self, key: PluginKey) {
        self.map.remove(&key);
    }
}

/// Remove a disposed fiber from its runtime; drop the runtime entry when
/// the last fiber goes away.
pub(crate) fn remove_fiber(core: &Arc<Core>, fiber: &Arc<Fiber>) {
    let runtime = match fiber.runtime() {
        Some(runtime) => runtime,
        None => return,
    };
    {
        let mut fibers = runtime.fibers.lock().unwrap();
        fibers.retain(|f| !Arc::ptr_eq(f, fiber));
        if !fibers.is_empty() {
            return;
        }
    }
    let mut registry = core.registry.lock().unwrap();
    // Re-check under the registry lock: a concurrent install may have
    // added a fiber to this runtime in the meantime.
    if runtime.fibers.lock().unwrap().is_empty() {
        registry.remove(runtime.key);
    }
}
