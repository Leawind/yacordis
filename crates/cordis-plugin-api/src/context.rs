//! The plugin-side context: a thin, `Copy` wrapper over a host handle
//! that forwards every operation through the host vtable.

use std::os::raw::c_void;

use serde_json::{Value, json};

use crate::abi::{Callback, HostVTable, RawBuffer, log_level};
use crate::buffer;
use crate::callback::Disposable;
use crate::plugin::{Inject, Plugin};

/// A scoped handle onto the host runtime, as seen from inside a plugin.
#[derive(Clone, Copy)]
pub struct Context {
    pub(crate) env: *const HostVTable,
    pub(crate) handle: u64,
}

impl Context {
    pub(crate) fn from_raw(env: *const HostVTable, handle: u64) -> Self {
        Self { env, handle }
    }

    /// The raw host handle for this context (usable across the ABI).
    #[must_use]
    pub fn handle(&self) -> u64 {
        self.handle
    }

    fn name_buf(name: &str) -> RawBuffer {
        buffer::buffer_from_vec(name.as_bytes().to_vec())
    }

    fn free(&self, buffer: RawBuffer) {
        // SAFETY: env is a valid host vtable provided at callback entry.
        unsafe { ((*self.env).free_buffer)(buffer) };
    }

    /// Register a listener. Returns a handle that removes it when
    /// disposed; it is removed automatically when the fiber unloads.
    pub fn on<F>(&self, name: &str, listener: F) -> Result<ListenerHandle, String>
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        // The host frees `name_buf` inside the call (callee frees inputs).
        let name_buf = Self::name_buf(name);
        let id = unsafe { ((*self.env).on)(self.handle, name_buf, Callback::listener(listener)) };
        ListenerHandle::new(self.env, id).ok_or_else(|| "cordis: ctx.on failed".into())
    }

    /// Register a one-shot listener.
    pub fn once<F>(&self, name: &str, listener: F) -> Result<ListenerHandle, String>
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        let name_buf = Self::name_buf(name);
        let id = unsafe { ((*self.env).once)(self.handle, name_buf, Callback::listener(listener)) };
        ListenerHandle::new(self.env, id).ok_or_else(|| "cordis: ctx.once failed".into())
    }

    /// Register a revertible effect; returned disposers run in reverse
    /// order when the effect is disposed or the fiber unloads.
    pub fn effect<F>(&self, label: &str, setup: F) -> Result<EffectHandle, String>
    where
        F: FnOnce(&Context) -> Result<Vec<Disposable>, Box<dyn std::error::Error + Send + Sync>>
            + Send
            + 'static,
    {
        let label_buf = Self::name_buf(label);
        let id =
            unsafe { ((*self.env).effect)(self.handle, label_buf, Callback::effect_setup(setup)) };
        EffectHandle::new(self.env, id).ok_or_else(|| "cordis: ctx.effect failed".into())
    }

    /// Provide a JSON service from this context's fiber.
    pub fn provide(&self, name: &str, value: Value) -> Result<(), String> {
        let name_buf = Self::name_buf(name);
        let value_buf = buffer::buffer_from_vec(serde_json::to_vec(&value).unwrap_or_default());
        let code = unsafe { ((*self.env).provide)(self.handle, name_buf, value_buf) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("cordis: provide `{name}` failed (code {code})"))
        }
    }

    /// Replace the value of a service provided by the same fiber.
    pub fn set(&self, name: &str, value: Value) -> Result<(), String> {
        let name_buf = Self::name_buf(name);
        let value_buf = buffer::buffer_from_vec(serde_json::to_vec(&value).unwrap_or_default());
        let code = unsafe { ((*self.env).set)(self.handle, name_buf, value_buf) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("cordis: set `{name}` failed (code {code})"))
        }
    }

    /// Read a service as JSON; `None` when absent or not JSON-typed.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Value> {
        let name_buf = Self::name_buf(name);
        let mut out = RawBuffer::EMPTY;
        let found = unsafe { ((*self.env).get)(self.handle, name_buf, &mut out) };
        if found {
            let value = serde_json::from_slice(&buffer::buffer_into_vec(out)).ok();
            self.free(out);
            value
        } else {
            None
        }
    }

    /// Whether a service is resolvable from this context.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        let name_buf = Self::name_buf(name);
        let found = unsafe { ((*self.env).has)(self.handle, name_buf) };
        self.free(name_buf);
        found
    }

    /// Dispatch an event.
    pub fn emit(&self, name: &str, args: &Value) {
        let name_buf = Self::name_buf(name);
        let args_buf = buffer::buffer_from_vec(serde_json::to_vec(args).unwrap_or_default());
        unsafe { ((*self.env).emit)(self.handle, name_buf, args_buf) };
    }

    /// Dispatch with bail semantics: first non-null listener result wins.
    #[must_use]
    pub fn bail(&self, name: &str, args: &Value) -> Option<Value> {
        let name_buf = Self::name_buf(name);
        let args_buf = buffer::buffer_from_vec(serde_json::to_vec(args).unwrap_or_default());
        let mut out = RawBuffer::EMPTY;
        let filled = unsafe { ((*self.env).bail)(self.handle, name_buf, args_buf, &mut out) };
        if filled {
            let value = serde_json::from_slice(&buffer::buffer_into_vec(out)).ok();
            self.free(out);
            value
        } else {
            None
        }
    }

    /// Install a sub-plugin from inside this plugin; returns the child
    /// fiber handle (usable with [`Context::fiber_dispose`]).
    pub fn plugin<P: Plugin>(&self, plugin: P, config: &Value) -> Result<u64, String> {
        let manifest = build_manifest(plugin.name(), &plugin.inject());
        let apply = Callback::apply(move |ctx, config| plugin.apply(ctx, config).map_err(|e| e.0));
        let config_buf = buffer::buffer_from_vec(serde_json::to_vec(config).unwrap_or_default());
        let fiber = unsafe { ((*self.env).plugin)(self.handle, manifest, apply, config_buf) };
        if fiber == 0 {
            Err("cordis: child plugin install failed".into())
        } else {
            Ok(fiber)
        }
    }

    /// Dispose a fiber handle previously returned by [`Context::plugin`].
    pub fn fiber_dispose(&self, handle: u64) -> Result<bool, String> {
        let code = unsafe { ((*self.env).fiber_dispose)(handle) };
        match code {
            0 => Ok(true),
            1 => Ok(false),
            other => Err(format!("cordis: fiber_dispose failed (code {other})")),
        }
    }

    /// Write a log line into the host's logger.
    pub fn log(&self, level: LogLevel, message: &str) {
        let target = Self::name_buf("plugin");
        let message_buf = Self::name_buf(message);
        unsafe { ((*self.env).log)(level as i32, target, message_buf) };
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("handle", &self.handle)
            .finish()
    }
}

/// Log levels, mirroring the host's `LogLevel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum LogLevel {
    /// Verbose diagnostics.
    Debug = log_level::DEBUG,
    /// Normal operational messages.
    Info = log_level::INFO,
    /// Something looks wrong.
    Warn = log_level::WARN,
    /// An operation failed.
    Error = log_level::ERROR,
}

/// Handle that removes a listener when disposed.
pub struct ListenerHandle {
    env: *const HostVTable,
    id: u64,
}

impl ListenerHandle {
    fn new(env: *const HostVTable, id: u64) -> Option<Self> {
        (id != 0).then_some(Self { env, id })
    }

    /// Remove the listener.
    pub fn dispose(&self) -> Result<(), String> {
        // SAFETY: env is a valid host vtable.
        let code = unsafe { ((*self.env).dispose_handle)(self.id) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("cordis: listener dispose failed (code {code})"))
        }
    }
}

/// Handle that disposes an effect early.
pub struct EffectHandle {
    env: *const HostVTable,
    id: u64,
}

impl EffectHandle {
    fn new(env: *const HostVTable, id: u64) -> Option<Self> {
        (id != 0).then_some(Self { env, id })
    }

    /// Run this effect's disposers in reverse order.
    pub fn dispose(&self) -> Result<(), String> {
        // SAFETY: env is a valid host vtable.
        let code = unsafe { ((*self.env).dispose_handle)(self.id) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("cordis: effect dispose failed (code {code})"))
        }
    }
}

/// Serialize a plugin manifest for the boundary.
pub(crate) fn build_manifest(name: Option<&str>, inject: &Inject) -> RawBuffer {
    let required: Vec<&str> = inject.required_names().collect();
    let optional: Vec<&str> = inject.optional_names().collect();
    let doc = json!({
        "name": name,
        "inject": { "required": required, "optional": optional },
        "abi": crate::ABI_VERSION,
    });
    buffer::buffer_from_vec(serde_json::to_vec(&doc).unwrap_or_default())
}

// Keep c_void import used even if layout tweaks remove direct uses.
#[allow(unused)]
fn _assert_c_void() -> Option<c_void> {
    None
}
