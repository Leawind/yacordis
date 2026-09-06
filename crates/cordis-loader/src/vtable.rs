//! The single static host vtable handed to every plugin callback, and
//! the `extern "C"` functions behind it.
//!
//! Buffer ownership recap (see `cordis-plugin-api`): every buffer is
//! freed exactly once with the shared allocator, by whoever is done
//! with it. Input buffers handed to us by plugins are freed here after
//! parsing; output buffers we fill are freed by the plugin after
//! reading.

use std::sync::Arc;

use cordis_plugin_api::{ABI_VERSION, Callback, HostVTable, RawBuffer, log_level};
use serde_json::Value;

use crate::adapter::{Invocation, SendCallback};
use crate::handles::{HandleTable, HostObject};
use cordis::{Context, EffectOutcome, LogLevel};

pub(crate) fn handles() -> &'static HandleTable {
    HandleTable::global()
}

/// Log lines from plugins go straight to stderr; a richer logger can be
/// layered on the host side later.
fn plugin_log(level: i32, message: &str) {
    let level = match level {
        log_level::DEBUG => LogLevel::Debug,
        log_level::INFO => LogLevel::Info,
        log_level::WARN => LogLevel::Warn,
        _ => LogLevel::Error,
    };
    let name = match level {
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
    };
    eprintln!("[{name}] [cordis/plugin] {message}");
}

fn take_string(buffer: RawBuffer) -> String {
    let text = cordis_plugin_api::buffer_to_string(buffer);
    cordis_plugin_api::free_buffer(buffer);
    text
}

fn take_json(buffer: RawBuffer) -> Value {
    let value =
        serde_json::from_slice(&cordis_plugin_api::buffer_into_vec(buffer)).unwrap_or(Value::Null);
    cordis_plugin_api::free_buffer(buffer);
    value
}

fn make_json(value: &Value) -> RawBuffer {
    cordis_plugin_api::buffer_from_vec(serde_json::to_vec(value).unwrap_or_default())
}

unsafe extern "C" fn v_free_buffer(buffer: RawBuffer) {
    cordis_plugin_api::free_buffer(buffer);
}

unsafe extern "C" fn v_log(level: i32, target: RawBuffer, message: RawBuffer) {
    let _target = take_string(target);
    let message = take_string(message);
    plugin_log(level, &message);
}

unsafe extern "C" fn v_effect(ctx_handle: u64, label: RawBuffer, setup: Callback) -> u64 {
    let label = take_string(label);
    let setup = SendCallback::new(setup);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        // Dropping `setup` releases the plugin-side state via Drop.
        return 0;
    };
    let result = ctx.effect(&label, move |_| {
        let outcome = match setup.invoke(ctx_handle, &Value::Null) {
            Invocation::Disposers(disposers) => EffectOutcome::Many(disposers),
            Invocation::Failed(message) => return Err(message.into()),
            Invocation::Ok(_) => EffectOutcome::None,
        };
        Ok(outcome)
    });
    match result {
        Ok(handle) => handles().insert(HostObject::Effect(handle)),
        Err(_) => 0,
    }
}

unsafe extern "C" fn v_on(ctx_handle: u64, name: RawBuffer, listener: Callback) -> u64 {
    on_impl(ctx_handle, name, listener, false)
}

unsafe extern "C" fn v_once(ctx_handle: u64, name: RawBuffer, listener: Callback) -> u64 {
    on_impl(ctx_handle, name, listener, true)
}

fn on_impl(ctx_handle: u64, name: RawBuffer, listener: Callback, once: bool) -> u64 {
    let name = take_string(name);
    let listener = SendCallback::new(listener);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return 0;
    };
    let registration_result = if once {
        ctx.once(&name, move |_, args| {
            invoke_listener(&listener, ctx_handle, args)
        })
    } else {
        ctx.on(&name, move |_, args| {
            invoke_listener(&listener, ctx_handle, args)
        })
    };
    match registration_result {
        Ok(handle) => handles().insert(HostObject::Effect(handle)),
        Err(_) => 0,
    }
}

fn invoke_listener(listener: &SendCallback, ctx_handle: u64, args: &Value) -> Option<Value> {
    match listener.invoke(ctx_handle, args) {
        Invocation::Ok(value) => value,
        Invocation::Disposers(_) => None,
        Invocation::Failed(message) => {
            plugin_log(log_level::ERROR, &format!("listener failed: {message}"));
            None
        }
    }
}

unsafe extern "C" fn v_dispose_handle(handle: u64) -> i32 {
    match handles().take_lifecycle(handle) {
        Some(crate::handles::LifecycleHandle::Effect(effect)) => {
            effect.dispose();
            0
        }
        Some(crate::handles::LifecycleHandle::Fiber(fiber)) => {
            // false = already disposed, still a success for the caller.
            u8::from(!fiber.dispose()) as i32
        }
        None => -2,
    }
}

unsafe extern "C" fn v_provide(ctx_handle: u64, name: RawBuffer, value: RawBuffer) -> i32 {
    let name = take_string(name);
    let value: Value = take_json(value);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return -1;
    };
    match ctx.provide(&name, Arc::new(value)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

unsafe extern "C" fn v_set(ctx_handle: u64, name: RawBuffer, value: RawBuffer) -> i32 {
    let name = take_string(name);
    let value: Value = take_json(value);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return -1;
    };
    match ctx.set(&name, Arc::new(value)) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

unsafe extern "C" fn v_get(ctx_handle: u64, name: RawBuffer, out: *mut RawBuffer) -> bool {
    let name = take_string(name);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return false;
    };
    let found = ctx
        .get_raw(&name)
        .and_then(|any| any.downcast::<Value>().ok())
        .map(|boxed| make_json(&boxed));
    match found {
        Some(buffer) => {
            // SAFETY: the host always passes a valid out-pointer.
            unsafe {
                *out = buffer;
            }
            true
        }
        None => false,
    }
}

unsafe extern "C" fn v_has(ctx_handle: u64, name: RawBuffer) -> bool {
    let name = take_string(name);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return false;
    };
    ctx.has(&name)
}

unsafe extern "C" fn v_emit(ctx_handle: u64, name: RawBuffer, args: RawBuffer) {
    let name = take_string(name);
    let args = take_json(args);
    if let Some(ctx) = handles().get_context(ctx_handle) {
        ctx.emit(&name, &args);
    }
}

unsafe extern "C" fn v_bail(
    ctx_handle: u64,
    name: RawBuffer,
    args: RawBuffer,
    out: *mut RawBuffer,
) -> bool {
    let name = take_string(name);
    let args = take_json(args);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return false;
    };
    match ctx.bail(&name, &args) {
        Some(value) => {
            // SAFETY: the host always passes a valid out-pointer.
            unsafe {
                *out = make_json(&value);
            }
            true
        }
        None => false,
    }
}

unsafe extern "C" fn v_plugin(
    ctx_handle: u64,
    manifest: RawBuffer,
    apply: Callback,
    config: RawBuffer,
) -> u64 {
    let manifest_text = cordis_plugin_api::buffer_to_string(manifest);
    cordis_plugin_api::free_buffer(manifest);
    let config = take_json(config);
    let Ok(doc) = serde_json::from_str::<Value>(&manifest_text) else {
        return 0;
    };
    let name = doc["name"].as_str().map(str::to_owned);
    let mut inject = cordis::Inject::none();
    for entry in doc["inject"]["required"]
        .as_array()
        .unwrap_or(&Vec::new())
        .clone()
    {
        if let Some(n) = entry.as_str() {
            inject = inject.req(n);
        }
    }
    for entry in doc["inject"]["optional"]
        .as_array()
        .unwrap_or(&Vec::new())
        .clone()
    {
        if let Some(n) = entry.as_str() {
            inject = inject.opt(n);
        }
    }
    let child = crate::adapter::FfiPlugin::new(name, inject, SendCallback::new(apply));
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return 0;
    };
    match ctx.plugin_dyn(Arc::new(child), config) {
        Ok(handle) => handles().insert(HostObject::Fiber(handle)),
        Err(_) => 0,
    }
}

unsafe extern "C" fn v_fiber_dispose(handle: u64) -> i32 {
    match handles().take_lifecycle(handle) {
        Some(crate::handles::LifecycleHandle::Fiber(fiber)) => {
            // false = already disposed, still a success for the caller.
            u8::from(!fiber.dispose()) as i32
        }
        Some(crate::handles::LifecycleHandle::Effect(effect)) => {
            effect.dispose();
            0
        }
        None => -2,
    }
}

/// The vtable pointer passed to every plugin callback.
pub(crate) static HOST_VTABLE: HostVTable = HostVTable {
    abi_version: ABI_VERSION,
    free_buffer: v_free_buffer,
    log: v_log,
    effect: v_effect,
    on: v_on,
    once: v_once,
    dispose_handle: v_dispose_handle,
    provide: v_provide,
    set: v_set,
    get: v_get,
    has: v_has,
    emit: v_emit,
    bail: v_bail,
    plugin: v_plugin,
    fiber_dispose: v_fiber_dispose,
};

// Context is referenced in doc comments and the adapter lives beside us.
#[allow(unused)]
fn _context_type_used() -> Option<Context> {
    None
}
