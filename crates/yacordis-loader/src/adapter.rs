//! Host-side callback wrapper and the FFI→native component adapter.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use yacordis_component_api::{Callback, RawBuffer, RawCallbacks};

use crate::handles::{HandleTable, HostObject};
use crate::lease::LibraryLease;
use crate::vtable::{HOST_VTABLE, handles};

// The lease of the library whose component code is executing on this
// thread. Callbacks born during an invocation (effect setups, listeners,
// child component applies) are tagged with it, so every component
// callback in the host is gated by its library's unload state.
thread_local! {
    static CURRENT_LEASE: RefCell<Option<Arc<LibraryLease>>> = const { RefCell::new(None) };
}

pub(crate) fn current_lease() -> Option<Arc<LibraryLease>> {
    CURRENT_LEASE.with(|cell| cell.borrow().clone())
}

/// Restores the previous thread-local lease on drop (panics inside
/// component code are caught by the trampolines; this covers the rest).
struct CurrentLeaseGuard(Option<Arc<LibraryLease>>);

impl CurrentLeaseGuard {
    fn set(lease: Option<Arc<LibraryLease>>) -> Self {
        Self(CURRENT_LEASE.with(|cell| cell.replace(lease)))
    }
}

impl Drop for CurrentLeaseGuard {
    fn drop(&mut self) {
        CURRENT_LEASE.with(|cell| *cell.borrow_mut() = self.0.take());
    }
}

/// A [`Callback`] erased from component code, safe to store in host
/// structures that require `Send + Sync`.
///
/// # Safety contract
///
/// The `unsafe impl Send/Sync` is sound because the component API requires
/// every erased closure to be `Send + Sync` before construction
/// (`Callback::listener` etc. enforce it at the type level), and the
/// host only ever touches `user_data` through the component's own
/// trampolines.
///
/// Every callback is tagged with the [`LibraryLease`] of the library its
/// code lives in: invocation goes through the lease's gate (refused once
/// the library is unloaded), and the state drop is skipped instead of
/// running component code in unmapped memory.
pub(crate) struct SendCallback {
    pub callback: Callback,
    /// The lease of the library this callback's code lives in. `None`
    /// only for callbacks produced outside any component invocation —
    /// none on current paths; untagged callbacks keep the ungated
    /// (pre-unload) behavior.
    lease: Option<Arc<LibraryLease>>,
}

unsafe impl Send for SendCallback {}
unsafe impl Sync for SendCallback {}

impl SendCallback {
    /// Wrap a callback born during a component invocation on this thread;
    /// tagged with the lease that invocation belongs to.
    pub(crate) fn capture(callback: Callback) -> Self {
        Self {
            callback,
            lease: current_lease(),
        }
    }

    /// Wrap the root apply callback at load time, where no invocation is
    /// in flight yet.
    pub(crate) fn rooted(callback: Callback, lease: Arc<LibraryLease>) -> Self {
        Self {
            callback,
            lease: Some(lease),
        }
    }

    /// Invoke the component trampoline. `ctx_handle` identifies the context
    /// the component sees inside the callback; `kind` names the call site
    /// for the unload gate's in-flight accounting.
    pub(crate) fn invoke(&self, kind: &'static str, ctx_handle: u64, args: &Value) -> Invocation {
        if self.callback.invoke.is_none() {
            return Invocation::Failed("callback has no entry point".into());
        }
        // Ownership: the trampoline frees `args_buf` (shared allocator);
        // output buffers are freed here after reading.
        let args_buf =
            yacordis_component_api::buffer_from_vec(serde_json::to_vec(args).unwrap_or_default());
        let mut out_json = RawBuffer::EMPTY;
        let mut out_callbacks = RawCallbacks::EMPTY;
        // The thread-local lease covers the whole call, including the
        // return-code handling: callbacks born there (the disposers of an
        // effect setup) must join this lease too.
        let _current = CurrentLeaseGuard::set(self.lease.clone());
        let code = match &self.lease {
            None => Self::call(
                &self.callback,
                ctx_handle,
                args_buf,
                &mut out_json,
                &mut out_callbacks,
            ),
            Some(lease) => {
                let Some(_gate) = lease.enter(kind) else {
                    // Closed library: the args buffer was never delivered,
                    // so the host frees it itself.
                    yacordis_component_api::free_buffer(args_buf);
                    return Invocation::Failed("component library is unloaded".into());
                };
                // The gate closes when the trampoline returns; everything
                // after (buffer frees, callback copies) runs no component
                // code, only the shared allocator.
                Self::call(
                    &self.callback,
                    ctx_handle,
                    args_buf,
                    &mut out_json,
                    &mut out_callbacks,
                )
            }
        };
        match code {
            0 => Invocation::Ok(None),
            1 => {
                let value =
                    serde_json::from_slice(&yacordis_component_api::buffer_into_vec(out_json)).ok();
                yacordis_component_api::free_buffer(out_json);
                Invocation::Ok(value)
            }
            2 => {
                let disposers = copy_callbacks(out_callbacks);
                yacordis_component_api::free_buffer(callbacks_as_buffer(out_callbacks));
                Invocation::Disposers(disposers)
            }
            3 => {
                let message = yacordis_component_api::buffer_to_string(out_json);
                yacordis_component_api::free_buffer(out_json);
                Invocation::Failed(message)
            }
            yacordis_component_api::CODE_PANIC => {
                Invocation::Failed("component code panicked".into())
            }
            other => Invocation::Failed(format!("component callback returned code {other}")),
        }
    }

    /// The raw boundary call. Separate so the gate setup reads linearly.
    fn call(
        callback: &Callback,
        ctx_handle: u64,
        args_buf: RawBuffer,
        out_json: &mut RawBuffer,
        out_callbacks: &mut RawCallbacks,
    ) -> i32 {
        let invoke = callback.invoke.expect("entry point checked by the caller");
        // SAFETY: user_data/invoke come from a live component callback; the
        // vtable pointer is our own static.
        unsafe {
            (invoke)(
                callback.user_data,
                &HOST_VTABLE,
                ctx_handle,
                args_buf,
                out_json,
                out_callbacks,
            )
        }
    }
}

impl Drop for SendCallback {
    fn drop(&mut self) {
        let Some(drop_fn) = self.callback.drop_fn else {
            return;
        };
        // SAFETY: user_data is owned by this callback record; moving
        // the Callback out via mem::forget keeps it alive so the
        // contract "dropped exactly once" holds even if a copy of
        // the raw value lingers elsewhere.
        let callback = std::mem::replace(
            &mut self.callback,
            Callback {
                user_data: std::ptr::null_mut(),
                invoke: None,
                drop_fn: None,
            },
        );
        match &self.lease {
            None => unsafe { drop_fn(callback.user_data) },
            Some(lease) => {
                // A closed gate skips the drop: the component-side state
                // box leaks (shared-heap bytes) instead of running code in
                // unmapped memory. On every compliant path the gate is
                // still open here.
                if let Some(_gate) = lease.enter("callback drop") {
                    let _current = CurrentLeaseGuard::set(self.lease.clone());
                    unsafe { drop_fn(callback.user_data) };
                }
            }
        }
    }
}

/// What a component trampoline answered with.
pub(crate) enum Invocation {
    Ok(Option<Value>),
    Disposers(Vec<yacordis::Disposer>),
    Failed(String),
}

fn copy_callbacks(raw: RawCallbacks) -> Vec<yacordis::Disposer> {
    if raw.ptr.is_null() || raw.len == 0 {
        return Vec::new();
    }
    // SAFETY: producer contract — `len` consecutive initialized Callbacks.
    let callbacks = unsafe { std::slice::from_raw_parts(raw.ptr, raw.len) };
    callbacks
        .iter()
        .map(|callback| {
            let this = SendCallback {
                callback: *callback,
                lease: current_lease(),
            };
            Box::new(move || {
                let _ = this.invoke("disposer", 0, &Value::Null);
            }) as yacordis::Disposer
        })
        .collect()
}

fn callbacks_as_buffer(raw: RawCallbacks) -> RawBuffer {
    RawBuffer {
        ptr: raw.ptr.cast::<u8>(),
        len: raw.len * std::mem::size_of::<Callback>(),
    }
}

/// Adapter presenting a dynamically loaded component as a native
/// `Arc<dyn yacordis::Component>`.
///
/// Dropping the last adapter of a library releases the library's lease:
/// with every fiber disposed, the library unmaps (through an explicit
/// unload or the last owner's drop). While any fiber holds an adapter,
/// the lease keeps the library mapped.
pub struct FfiComponent {
    name: Option<String>,
    inject: yacordis::Inject,
    apply: SendCallback,
    /// Fiber id → context handle, so a stashed component-side context
    /// stays valid across reloads of the same fiber.
    ctx_handles: Mutex<HashMap<u64, u64>>,
}

impl FfiComponent {
    fn new(name: Option<String>, inject: yacordis::Inject, apply: SendCallback) -> Self {
        Self {
            name,
            inject,
            apply,
            ctx_handles: Mutex::new(HashMap::new()),
        }
    }

    /// Create the adapter and register it with its lease as a live
    /// component instance (unload evidence). The lease itself is pinned by
    /// the apply callback — while any adapter is alive, the library stays
    /// mapped.
    pub(crate) fn create(
        name: Option<String>,
        inject: yacordis::Inject,
        apply: SendCallback,
        lease: Arc<LibraryLease>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| {
            lease.register_adapter(weak.clone());
            FfiComponent::new(name, inject, apply)
        })
    }

    pub(crate) fn context_handle(&self, ctx: &yacordis::Context, handles: &HandleTable) -> u64 {
        let fiber_id = ctx.fiber().id().unwrap_or(0);
        let mut cached = self.ctx_handles.lock().unwrap();
        if let Some(id) = cached.get(&fiber_id) {
            // Still resolvable (its fiber alive)? Reuse; otherwise drop
            // the stale entry and mint a fresh handle.
            if handles.get_context(*id).is_some() {
                return *id;
            }
            cached.remove(&fiber_id);
        }
        let id = handles.insert(HostObject::Context(ctx.clone()));
        cached.insert(fiber_id, id);
        id
    }
}

impl yacordis::Component for FfiComponent {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn inject(&self) -> yacordis::Inject {
        self.inject.clone()
    }

    fn apply(&self, ctx: &yacordis::Context, config: &Value) -> yacordis::ComponentResult {
        let ctx_handle = self.context_handle(ctx, handles());
        // Runs inside the core's effect-collection scope, so every
        // registration the component body makes through the vtable lands in
        // this fiber's effect list automatically.
        match self.apply.invoke("apply", ctx_handle, config) {
            Invocation::Ok(_) => Ok(()),
            Invocation::Disposers(disposers) => {
                // The apply channel does not carry disposers; if a component
                // produces them anyway, revert immediately.
                for disposer in disposers {
                    disposer();
                }
                Ok(())
            }
            Invocation::Failed(message) => Err(yacordis::ComponentError::new(message)),
        }
    }
}
