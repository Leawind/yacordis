//! Component registry: identity-keyed runtimes aggregating installed fibers
//! (cordis `RegistryService`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use crate::component::Component;
use crate::context::Core;
use crate::fiber::Fiber;

/// Identity of a component: the data address of its `Arc<dyn Component>`.
///
/// Two installs of the same `Arc` share one runtime (v4 keys by the
/// resolved component function); re-wrapping the same value in a fresh `Arc`
/// creates a distinct runtime, which is the meaningful notion of component
/// identity for erased, dynamically loaded components.
pub(crate) type ComponentKey = usize;

pub(crate) fn component_key(component: &Arc<dyn Component>) -> ComponentKey {
    // Drop the vtable, keep the data pointer: stable for the Arc's lifetime.
    Arc::as_ptr(component) as *const () as usize
}

pub(crate) struct Runtime {
    pub key: ComponentKey,
    /// Component name, reported by [`Registry::summaries`].
    pub name: String,
    pub fibers: Mutex<Vec<Arc<Fiber>>>,
}

#[derive(Default)]
pub(crate) struct Registry {
    map: HashMap<ComponentKey, Arc<Runtime>>,
    /// name → fibers injecting it (required or optional). Notify walks
    /// this index; entries are weak and
    /// pruned on sight, disposal unregisters explicitly.
    inject_index: HashMap<String, Vec<Weak<Fiber>>>,
}

impl Registry {
    pub fn entry(&mut self, key: ComponentKey, name: String) -> Arc<Runtime> {
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

    /// Index a fiber under every name it injects. Called at install and
    /// whenever `replace_component` swaps the declared inject.
    pub fn register_injects(&mut self, fiber: &Arc<Fiber>) {
        for (name, _) in fiber.inject_names() {
            let list = self.inject_index.entry(name).or_default();
            if !list
                .iter()
                .any(|weak| weak.upgrade().as_ref().map(|f| f.id) == Some(fiber.id))
            {
                list.push(Arc::downgrade(fiber));
            }
        }
    }

    /// Drop a fiber from the inject index (dispose, or inject swap).
    pub fn unregister_fiber(&mut self, fiber: &Arc<Fiber>) {
        for (name, _) in fiber.inject_names() {
            if let Some(list) = self.inject_index.get_mut(name.as_str()) {
                list.retain(|weak| weak.upgrade().as_ref().map(|f| f.id) != Some(fiber.id));
                if list.is_empty() {
                    self.inject_index.remove(name.as_str());
                }
            }
        }
    }

    /// Fibers injecting `name`, with stale weak entries pruned.
    pub fn injectors_of(&mut self, name: &str) -> Vec<Arc<Fiber>> {
        let Some(list) = self.inject_index.get_mut(name) else {
            return Vec::new();
        };
        list.retain(|weak| weak.upgrade().is_some());
        if list.is_empty() {
            self.inject_index.remove(name);
            return Vec::new();
        }
        list.iter().filter_map(|weak| weak.upgrade()).collect()
    }

    pub fn all_fibers(&self) -> Vec<Arc<Fiber>> {
        let mut out = Vec::new();
        for runtime in self.map.values() {
            out.extend(runtime.fibers.lock().unwrap().iter().cloned());
        }
        out
    }

    /// Snapshot of the fibers installed from one component identity.
    pub fn fibers_of(&self, key: ComponentKey) -> Option<Vec<Arc<Fiber>>> {
        self.map
            .get(&key)
            .map(|runtime| runtime.fibers.lock().unwrap().clone())
    }

    /// Introspection view for `Context::installed_components`.
    pub(crate) fn summaries(&self) -> Vec<crate::context::ComponentSummary> {
        let mut out: Vec<_> = self
            .map
            .values()
            .map(|runtime| crate::context::ComponentSummary {
                name: Some(runtime.name.clone()),
                fibers: runtime
                    .fibers
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|fiber| {
                        let inner = fiber.inner.lock().unwrap();
                        crate::context::FiberSummary {
                            id: fiber.id,
                            name: fiber.name.clone(),
                            state: inner.state,
                        }
                    })
                    .collect(),
            })
            .collect();
        out.sort_by_key(|summary| summary.name.clone().unwrap_or_default());
        out
    }

    fn remove(&mut self, key: ComponentKey) {
        self.map.remove(&key);
    }
}

/// Remove a disposed fiber from its runtime and the inject index; drop
/// the runtime entry when the last fiber goes away.
pub(crate) fn remove_fiber(core: &Arc<Core>, fiber: &Arc<Fiber>) {
    let runtime = match fiber.runtime() {
        Some(runtime) => runtime,
        None => return,
    };
    // Lock discipline: registry before per-runtime fiber lists — the same
    // order as the introspection paths, never the reverse.
    let mut registry = core.registry.lock().unwrap();
    registry.unregister_fiber(fiber);
    let now_empty = {
        let mut fibers = runtime.fibers.lock().unwrap();
        fibers.retain(|f| !Arc::ptr_eq(f, fiber));
        fibers.is_empty()
    };
    if now_empty {
        // Re-check under the registry lock: a concurrent install may have
        // added a fiber to this runtime in the meantime.
        if runtime.fibers.lock().unwrap().is_empty() {
            registry.remove(runtime.key);
        }
    }
}

/// Move a fiber to the runtime of its (new) component identity
/// (`registry.replace` bookkeeping). The old runtime entry is dropped
/// when the last fiber leaves it. No-op when the identity is unchanged.
pub(crate) fn move_fiber(
    core: &Arc<Core>,
    fiber: &Arc<Fiber>,
    new_key: ComponentKey,
    new_name: String,
) {
    let new_runtime = {
        let mut registry = core.registry.lock().unwrap();
        registry.entry(new_key, new_name)
    };
    if let Some(old) = fiber.runtime() {
        if old.key != new_runtime.key {
            let now_empty = {
                let mut fibers = old.fibers.lock().unwrap();
                fibers.retain(|f| !Arc::ptr_eq(f, fiber));
                fibers.is_empty()
            };
            if now_empty {
                let mut registry = core.registry.lock().unwrap();
                // Re-check under the registry lock: a concurrent install
                // may have added a fiber to this runtime in the meantime.
                if old.fibers.lock().unwrap().is_empty() {
                    registry.remove(old.key);
                }
            }
        }
    }
    *fiber.runtime.write().unwrap() = Some(new_runtime.clone());
    new_runtime.fibers.lock().unwrap().push(fiber.clone());
}
