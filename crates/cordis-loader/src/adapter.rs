//! Host-side callback wrapper and the FFI→native plugin adapter.

use std::collections::HashMap;
use std::sync::Mutex;

use cordis_plugin_api::{Callback, RawBuffer, RawCallbacks};
use serde_json::Value;

use crate::handles::{HandleTable, HostObject};
use crate::vtable::{HOST_VTABLE, handles};

/// A [`Callback`] erased from plugin code, safe to store in host
/// structures that require `Send + Sync`.
///
/// # Safety contract
///
/// The `unsafe impl Send/Sync` is sound because the plugin API requires
/// every erased closure to be `Send + Sync` before construction
/// (`Callback::listener` etc. enforce it at the type level), and the
/// host only ever touches `user_data` through the plugin's own
/// trampolines.
pub(crate) struct SendCallback {
    pub callback: Callback,
}

unsafe impl Send for SendCallback {}
unsafe impl Sync for SendCallback {}

impl SendCallback {
    pub(crate) fn new(callback: Callback) -> Self {
        Self { callback }
    }
}

impl Drop for SendCallback {
    fn drop(&mut self) {
        if let Some(drop_fn) = self.callback.drop_fn {
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
            unsafe { drop_fn(callback.user_data) };
        }
    }
}

impl SendCallback {
    /// Invoke the plugin trampoline. `ctx_handle` identifies the context
    /// the plugin sees inside the callback.
    pub(crate) fn invoke(&self, ctx_handle: u64, args: &Value) -> Invocation {
        let Some(invoke) = self.callback.invoke else {
            return Invocation::Failed("callback has no entry point".into());
        };
        // Ownership: the trampoline frees `args_buf` (shared allocator);
        // output buffers are freed here after reading.
        let args_buf =
            cordis_plugin_api::buffer_from_vec(serde_json::to_vec(args).unwrap_or_default());
        let mut out_json = RawBuffer::EMPTY;
        let mut out_callbacks = RawCallbacks::EMPTY;
        // SAFETY: user_data/invoke come from a live plugin callback; the
        // vtable pointer is our own static.
        let code = unsafe {
            (invoke)(
                self.callback.user_data,
                &HOST_VTABLE,
                ctx_handle,
                args_buf,
                &mut out_json,
                &mut out_callbacks,
            )
        };
        match code {
            0 => Invocation::Ok(None),
            1 => {
                let value =
                    serde_json::from_slice(&cordis_plugin_api::buffer_into_vec(out_json)).ok();
                cordis_plugin_api::free_buffer(out_json);
                Invocation::Ok(value)
            }
            2 => {
                let disposers = copy_callbacks(out_callbacks);
                cordis_plugin_api::free_buffer(callbacks_as_buffer(out_callbacks));
                Invocation::Disposers(disposers)
            }
            3 => {
                let message = cordis_plugin_api::buffer_to_string(out_json);
                cordis_plugin_api::free_buffer(out_json);
                Invocation::Failed(message)
            }
            cordis_plugin_api::CODE_PANIC => Invocation::Failed("plugin code panicked".into()),
            other => Invocation::Failed(format!("plugin callback returned code {other}")),
        }
    }
}

/// What a plugin trampoline answered with.
pub(crate) enum Invocation {
    Ok(Option<Value>),
    Disposers(Vec<cordis::Disposer>),
    Failed(String),
}

fn copy_callbacks(raw: RawCallbacks) -> Vec<cordis::Disposer> {
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
            };
            Box::new(move || {
                let _ = this.invoke(0, &Value::Null);
            }) as cordis::Disposer
        })
        .collect()
}

fn callbacks_as_buffer(raw: RawCallbacks) -> RawBuffer {
    RawBuffer {
        ptr: raw.ptr.cast::<u8>(),
        len: raw.len * std::mem::size_of::<Callback>(),
    }
}

/// Adapter presenting a dynamically loaded plugin as a native
/// `Arc<dyn cordis::Plugin>`.
pub struct FfiPlugin {
    name: Option<String>,
    inject: cordis::Inject,
    apply: SendCallback,
    /// Fiber id → context handle, so a stashed plugin-side context
    /// stays valid across reloads of the same fiber.
    ctx_handles: Mutex<HashMap<u64, u64>>,
}

impl FfiPlugin {
    pub(crate) fn new(name: Option<String>, inject: cordis::Inject, apply: SendCallback) -> Self {
        Self {
            name,
            inject,
            apply,
            ctx_handles: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn context_handle(&self, ctx: &cordis::Context, handles: &HandleTable) -> u64 {
        let fiber_id = ctx.fiber().id().unwrap_or(0);
        *self
            .ctx_handles
            .lock()
            .unwrap()
            .entry(fiber_id)
            .or_insert_with(|| handles.insert(HostObject::Context(ctx.clone())))
    }
}

impl cordis::Plugin for FfiPlugin {
    fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn inject(&self) -> cordis::Inject {
        self.inject.clone()
    }

    fn apply(&self, ctx: &cordis::Context, config: &Value) -> cordis::PluginResult {
        let ctx_handle = self.context_handle(ctx, handles());
        // Runs inside the core's effect-collection scope, so every
        // registration the plugin body makes through the vtable lands in
        // this fiber's effect list automatically.
        match self.apply.invoke(ctx_handle, config) {
            Invocation::Ok(_) => Ok(()),
            Invocation::Disposers(disposers) => {
                // The apply channel does not carry disposers; if a plugin
                // produces them anyway, revert immediately.
                for disposer in disposers {
                    disposer();
                }
                Ok(())
            }
            Invocation::Failed(message) => Err(cordis::PluginError::new(message)),
        }
    }
}
