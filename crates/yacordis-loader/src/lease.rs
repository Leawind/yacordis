//! The unload gate: one [`LibraryLease`] per mapped component binary.
//!
//! Unloading a cdylib is only honest on a *proven* quiet state: no
//! component code in flight, no fiber that can still run component code.
//! The lease collects exactly that evidence while the library is mapped:
//!
//! - a **gate** that every entry into component code passes through
//!   (apply, listeners, effect setups, work units, disposers, callback
//!   drops), counting in-flight calls per kind and refusing entries once
//!   the library is closed;
//! - a **ledger of adapters** (the root one created at load time and one
//!   per child install from component code), so a fiber still holding a
//!   component instance is observable;
//! - a **ledger of fibers** installed through the official install paths,
//!   for naming what blocks an unload.
//!
//! Closing is serialized against entering on the same gate lock, so no
//! call can jump into unmapped code: either the close observed an empty
//! in-flight count and shut the gate first, or an entry got through and
//! the close refuses. A refused unload keeps the library mapped — the
//! keep-mapped strategy is the backstop that turns contract violations
//! into memory leaks instead of crashes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use libloading::Library;
use yacordis::{Component, FiberHandle, FiberState};

use crate::adapter::FfiComponent;

/// Why an unload was refused.
#[derive(Debug)]
pub enum UnloadError {
    /// Component code of this library is still executing: an in-flight
    /// framework call that has not been drained yet, or a call that
    /// escaped the framework scheduling domain (an I1 violation).
    /// Names the call kinds and their counts.
    InFlight {
        /// e.g. `["work × 1"]`.
        calls: Vec<String>,
    },
    /// A fiber installed from this library has not been disposed; unloading
    /// would revoke code a live fiber can still run. Names the fibers.
    FibersAlive {
        /// e.g. `["demo-greeter#3 (active)"]`.
        fibers: Vec<String>,
    },
    /// A component instance of this library is still held outside every
    /// tracked fiber — installed outside the official install paths, or a
    /// child component instance not yet torn down. Names the components.
    ComponentsAlive {
        /// e.g. `["demo-greeter"]`.
        components: Vec<String>,
    },
    /// The OS refused to unmap the library. The gate stays closed
    /// (fail-closed: every later entry into the library is refused) and
    /// the mapping degrades to keep-mapped.
    System(String),
}

impl std::fmt::Display for UnloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnloadError::InFlight { calls } => {
                write!(f, "cannot unload: component code is still executing (")?;
                for (index, call) in calls.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{call}")?;
                }
                write!(f, "); dispose the owning fibers first")
            }
            UnloadError::FibersAlive { fibers } => {
                write!(
                    f,
                    "cannot unload: fibers are still installed from this library ("
                )?;
                for (index, fiber) in fibers.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{fiber}")?;
                }
                write!(f, "); dispose them first")
            }
            UnloadError::ComponentsAlive { components } => {
                write!(f, "cannot unload: component instances are still held (")?;
                for (index, component) in components.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{component}")?;
                }
                write!(f, "); dispose their fibers first")
            }
            UnloadError::System(error) => {
                write!(
                    f,
                    "cannot unload: the OS refused to unmap the library: {error}"
                )
            }
        }
    }
}

impl std::error::Error for UnloadError {}

struct GateState {
    closed: bool,
    /// In-flight component-code entries by call kind ("apply", "listener",
    /// "work", …). Empty ⇔ no component code is executing anywhere.
    in_flight: HashMap<&'static str, usize>,
}

pub(crate) struct LibraryLease {
    /// Entry gate into component code. `enter` and `close` serialize here,
    /// so closing can never race a call into unmapped code.
    gate: Mutex<GateState>,
    /// The mapped library; taken out at close and unmapped by the closer.
    library: Mutex<Option<Library>>,
    /// Adapters born from this library (root + one per child install),
    /// weakly — dying adapters stop counting on their own.
    adapters: Mutex<Vec<Weak<FfiComponent>>>,
    /// Fibers installed through the official install paths, for violation
    /// reporting. `FiberHandle::state()` self-reports `Disposed`.
    fibers: Mutex<Vec<(FiberHandle, String)>>,
    /// The artifact path, for diagnostics.
    path: PathBuf,
    /// Whether closing the lease also deletes the mapped file (an
    /// ephemeral materialized copy).
    remove_on_close: AtomicBool,
}

// SAFETY: the Library is only ever taken out and unmapped behind its
// mutex (close and Drop serialize on it), and libloading already
// guarantees `Send`.
unsafe impl Send for LibraryLease {}
unsafe impl Sync for LibraryLease {}

impl LibraryLease {
    pub(crate) fn new(library: Library, path: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(GateState {
                closed: false,
                in_flight: HashMap::new(),
            }),
            library: Mutex::new(Some(library)),
            adapters: Mutex::new(Vec::new()),
            fibers: Mutex::new(Vec::new()),
            path,
            remove_on_close: AtomicBool::new(false),
        })
    }

    /// Delete the mapped file when the lease closes.
    ///
    /// Set by ephemeral materialized loads: the copy exists only for the
    /// lifetime of the mapping.
    pub(crate) fn mark_remove_on_close(&self) {
        self.remove_on_close.store(true, Ordering::Relaxed);
    }

    /// Best effort: a copy still mapped elsewhere, or one the platform
    /// keeps locked, stays for `Loader::prune_materialized`.
    fn remove_copy(&self) {
        if self.remove_on_close.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// The artifact path this lease was loaded from.
    pub(crate) fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Whether the library is closed (successfully unloaded, or sealed
    /// after a failed unmap).
    pub(crate) fn is_closed(&self) -> bool {
        self.gate.lock().unwrap().closed
    }

    /// Enter component code of this library. `None` means the gate is
    /// closed: the call must not run.
    pub(crate) fn enter(&self, kind: &'static str) -> Option<EntryGuard<'_>> {
        let mut gate = self.gate.lock().unwrap();
        if gate.closed {
            return None;
        }
        *gate.in_flight.entry(kind).or_insert(0) += 1;
        Some(EntryGuard { lease: self, kind })
    }

    /// Register a component adapter born from this library.
    pub(crate) fn register_adapter(&self, adapter: Weak<FfiComponent>) {
        let mut adapters = self.adapters.lock().unwrap();
        adapters.retain(|entry| entry.strong_count() > 0);
        adapters.push(adapter);
    }

    /// Register a fiber installed through an official install path
    /// ([`ComponentLibrary::install`](crate::ComponentLibrary::install) or
    /// a component-code child install). Disposed fibers are pruned here.
    pub(crate) fn register_fiber(&self, handle: FiberHandle, name: Option<String>) {
        let mut fibers = self.fibers.lock().unwrap();
        fibers.retain(|(handle, _)| handle.state() != FiberState::Disposed);
        fibers.push((handle, name.unwrap_or_else(|| "unnamed".to_owned())));
    }

    /// Fast pre-check: refuse while component code is in flight, without
    /// waiting for anything.
    pub(crate) fn check_in_flight(&self) -> Result<(), UnloadError> {
        let gate = self.gate.lock().unwrap();
        if gate.in_flight.is_empty() {
            Ok(())
        } else {
            Err(in_flight_error(&gate.in_flight))
        }
    }

    /// Close the lease: unmap the library. Every piece of quiet-state
    /// evidence is (re-)checked under the gate lock, so the decision is
    /// atomic against new entries into component code. After a successful
    /// close, every later entry is refused (and callback drops skip
    /// component-side cleanup) — the gate is the reason unmapping is safe.
    ///
    /// `root` is the library's own adapter; any other live adapter is a
    /// violation.
    pub(crate) fn close(&self, root: &Arc<FfiComponent>) -> Result<(), UnloadError> {
        let mut gate = self.gate.lock().unwrap();
        if !gate.in_flight.is_empty() {
            return Err(in_flight_error(&gate.in_flight));
        }
        if let Some(fibers) = live_fibers(&self.fibers) {
            return Err(UnloadError::FibersAlive { fibers });
        }
        // Every fiber holding a component instance holds a clone of some
        // adapter. Child adapters are ledgered; the root is exempt from
        // the ledger (it belongs to the library handle), so its own strong
        // count is the evidence that no fiber holds a clone of it.
        let mut held: Vec<String> = Vec::new();
        if Arc::strong_count(root) > 1 {
            held.push(
                root.name()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "unnamed".to_owned()),
            );
        }
        if let Some(components) = live_adapters(&self.adapters, root) {
            held.extend(components);
        }
        if !held.is_empty() {
            return Err(UnloadError::ComponentsAlive { components: held });
        }
        gate.closed = true;
        drop(gate);
        match self.library.lock().unwrap().take() {
            // A concurrent unload won the race; the outcome is the same.
            None => Ok(()),
            // On failure the gate stays closed (fail-closed): the mapping
            // degrades to keep-mapped and every later entry is refused.
            Some(library) => {
                let result = library
                    .close()
                    .map_err(|error| UnloadError::System(error.to_string()));
                if result.is_ok() {
                    self.remove_copy();
                }
                result
            }
        }
    }
}

impl Drop for LibraryLease {
    fn drop(&mut self) {
        // Last owner gone (the library handle and every adapter dropped):
        // no fiber can run component code again, so the mapping goes with
        // the lease. A live entry guard would pin an `Arc` of this lease,
        // so the in-flight check below is unreachable by construction —
        // if that ever changes, keep-mapped is the backstop, not a crash.
        let library = self.library.lock().unwrap().take();
        if self.gate.lock().unwrap().in_flight.is_empty() {
            drop(library);
            self.remove_copy();
        } else {
            std::mem::forget(library);
        }
    }
}

/// Holds one counted entry into component code. Dropping it leaves.
pub(crate) struct EntryGuard<'a> {
    lease: &'a LibraryLease,
    kind: &'static str,
}

impl Drop for EntryGuard<'_> {
    fn drop(&mut self) {
        let mut gate = self.lease.gate.lock().unwrap();
        if let Some(count) = gate.in_flight.get_mut(self.kind) {
            *count -= 1;
            if *count == 0 {
                gate.in_flight.remove(self.kind);
            }
        }
    }
}

fn in_flight_error(in_flight: &HashMap<&'static str, usize>) -> UnloadError {
    let mut calls: Vec<String> = in_flight
        .iter()
        .map(|(kind, count)| format!("{kind} × {count}"))
        .collect();
    calls.sort();
    UnloadError::InFlight { calls }
}

fn live_fibers(fibers: &Mutex<Vec<(FiberHandle, String)>>) -> Option<Vec<String>> {
    let mut fibers = fibers.lock().unwrap();
    fibers.retain(|(handle, _)| handle.state() != FiberState::Disposed);
    if fibers.is_empty() {
        return None;
    }
    Some(
        fibers
            .iter()
            .map(|(handle, name)| {
                format!(
                    "{name}#{} ({})",
                    handle.id().unwrap_or(0),
                    handle.state().as_str()
                )
            })
            .collect(),
    )
}

fn live_adapters(
    adapters: &Mutex<Vec<Weak<FfiComponent>>>,
    root: &Arc<FfiComponent>,
) -> Option<Vec<String>> {
    let mut adapters = adapters.lock().unwrap();
    adapters.retain(|entry| entry.strong_count() > 0);
    let mut alive: Vec<String> = Vec::new();
    for entry in adapters.iter() {
        let Some(adapter) = entry.upgrade() else {
            continue;
        };
        if Arc::ptr_eq(root, &adapter) {
            continue;
        }
        alive.push(
            adapter
                .name()
                .map(str::to_owned)
                .unwrap_or_else(|| "unnamed".to_owned()),
        );
    }
    if alive.is_empty() { None } else { Some(alive) }
}
