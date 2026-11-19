//! The single static host vtable handed to every component callback, and
//! the `extern "C"` functions behind it.
//!
//! Buffer ownership recap (see `yacordis-component-api`): every buffer is
//! freed exactly once with the shared allocator, by whoever is done
//! with it. Input buffers handed to us by components are freed here after
//! parsing; output buffers we fill are freed by the component after
//! reading.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use yacordis_component_api::{ABI_VERSION, Callback, HostVTable, RawBuffer, log_level};

use crate::adapter::{Invocation, SendCallback};
use crate::handles::{HandleTable, HostObject};
use yacordis::{CancellationToken, Context, EffectOutcome, LogLevel};

pub(crate) fn handles() -> &'static HandleTable {
    HandleTable::global()
}

/// Log lines from components go straight to stderr; a richer logger can be
/// layered on the host side later.
fn component_log(level: i32, message: &str) {
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
    eprintln!("[{name}] [cordis/component] {message}");
}

fn take_string(buffer: RawBuffer) -> String {
    let text = yacordis_component_api::buffer_to_string(buffer);
    yacordis_component_api::free_buffer(buffer);
    text
}

fn take_json(buffer: RawBuffer) -> Value {
    let value = serde_json::from_slice(&yacordis_component_api::buffer_into_vec(buffer))
        .unwrap_or(Value::Null);
    yacordis_component_api::free_buffer(buffer);
    value
}

fn make_json(value: &Value) -> RawBuffer {
    yacordis_component_api::buffer_from_vec(serde_json::to_vec(value).unwrap_or_default())
}

unsafe extern "C" fn v_free_buffer(buffer: RawBuffer) {
    yacordis_component_api::free_buffer(buffer);
}

unsafe extern "C" fn v_log(level: i32, target: RawBuffer, message: RawBuffer) {
    let _target = take_string(target);
    let message = take_string(message);
    component_log(level, &message);
}

unsafe extern "C" fn v_effect(ctx_handle: u64, label: RawBuffer, setup: Callback) -> u64 {
    let label = take_string(label);
    let setup = SendCallback::capture(setup);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        // Dropping `setup` releases the component-side state via Drop.
        return 0;
    };
    let result = ctx.effect(&label, move |_| {
        let outcome = match setup.invoke("effect setup", ctx_handle, &Value::Null) {
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
    let listener = SendCallback::capture(listener);
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
    match listener.invoke("listener", ctx_handle, args) {
        Invocation::Ok(value) => value,
        Invocation::Disposers(_) => None,
        Invocation::Failed(message) => {
            component_log(log_level::ERROR, &format!("listener failed: {message}"));
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
        Some(crate::handles::LifecycleHandle::Work(work)) => {
            // Disposing a work handle cancels the unit.
            work.cancel();
            0
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

unsafe extern "C" fn v_component(
    ctx_handle: u64,
    manifest: RawBuffer,
    apply: Callback,
    config: RawBuffer,
) -> u64 {
    let manifest_text = yacordis_component_api::buffer_to_string(manifest);
    yacordis_component_api::free_buffer(manifest);
    let config = take_json(config);
    let Ok(doc) = serde_json::from_str::<Value>(&manifest_text) else {
        return 0;
    };
    let name = doc["name"].as_str().map(str::to_owned);
    let mut inject = yacordis::Inject::none();
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
    // The child's apply callback points into the parent's library: the
    // child adapter joins the same lease's unload evidence.
    let Some(lease) = crate::adapter::current_lease() else {
        return 0;
    };
    let child = crate::adapter::FfiComponent::create(
        name.clone(),
        inject,
        SendCallback::capture(apply),
        lease.clone(),
    );
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return 0;
    };
    match ctx.component_dyn(child, config) {
        Ok(handle) => {
            lease.register_fiber(handle.clone(), name);
            handles().insert(HostObject::Fiber(handle))
        }
        Err(_) => 0,
    }
}

unsafe extern "C" fn v_fiber_dispose(handle: u64) -> i32 {
    match handles().take_lifecycle(handle) {
        Some(crate::handles::LifecycleHandle::Fiber(fiber)) => {
            // false = already disposed, still a success for the caller.
            let disposed_now = !fiber.dispose();
            // §4.1 I2 host half: dispose-time audit collects context
            // handles bound to the dead fiber.
            let collected = handles().audit_fiber(fiber.id().unwrap_or(u64::MAX));
            if collected > 0 {
                component_log(
                    log_level::DEBUG,
                    &format!("dispose audit: collected {collected} stale context handles"),
                );
            }
            u8::from(disposed_now) as i32
        }
        Some(crate::handles::LifecycleHandle::Effect(effect)) => {
            effect.dispose();
            0
        }
        Some(crate::handles::LifecycleHandle::Work(work)) => {
            work.cancel();
            0
        }
        None => -2,
    }
}

// The token registry hands each FFI work unit a pollable id: the component
// side sees the token handle as its work callback's `ctx` argument.
fn work_tokens() -> &'static Mutex<(AtomicU64, HashMap<u64, CancellationToken>)> {
    static TOKENS: OnceLock<Mutex<(AtomicU64, HashMap<u64, CancellationToken>)>> = OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new((AtomicU64::new(1), HashMap::new())))
}

fn register_token(token: CancellationToken) -> u64 {
    let mut tokens = work_tokens().lock().unwrap();
    let id = tokens.0.fetch_add(1, Ordering::Relaxed);
    tokens.1.insert(id, token);
    id
}

fn take_token(id: u64) -> Option<CancellationToken> {
    work_tokens().lock().unwrap().1.remove(&id)
}

unsafe extern "C" fn v_run_blocking(
    ctx_handle: u64,
    label: RawBuffer,
    work: Callback,
    on_done: Callback,
) -> u64 {
    let label = take_string(label);
    let work = SendCallback::capture(work);
    let on_done = SendCallback::capture(on_done);
    let Some(ctx) = handles().get_context(ctx_handle) else {
        return 0;
    };
    let result = ctx.run_blocking(
        &label,
        move |token| {
            let token_id = register_token(token.clone());
            // The work callback's `ctx` argument is the token handle.
            let invocation = work.invoke("work", token_id, &Value::Null);
            take_token(token_id);
            match invocation {
                Invocation::Ok(value) => Ok(value.unwrap_or(Value::Null)),
                Invocation::Failed(message) => Err(yacordis::ComponentError::new(message)),
                Invocation::Disposers(_) => Ok(Value::Null),
            }
        },
        move |_, outcome| {
            let envelope = match &outcome {
                yacordis::WorkOutcome::Done(value) => json!({ "tag": "done", "value": value }),
                yacordis::WorkOutcome::Cancelled => json!({ "tag": "cancelled" }),
                yacordis::WorkOutcome::Failed(message) => {
                    json!({ "tag": "failed", "message": message.to_string() })
                }
                yacordis::WorkOutcome::Panicked(message) => {
                    json!({ "tag": "panicked", "message": message })
                }
            };
            let _ = on_done.invoke("work completion", ctx_handle, &envelope);
        },
    );
    match result {
        Ok(handle) => handles().insert(HostObject::Work(handle)),
        Err(_) => 0,
    }
}

unsafe extern "C" fn v_work_cancelled(token: u64) -> i32 {
    match work_tokens().lock().unwrap().1.get(&token) {
        Some(t) => i32::from(t.is_cancelled()),
        None => -1,
    }
}

unsafe extern "C" fn v_work_cancel(handle: u64) -> i32 {
    match handles().take_lifecycle(handle) {
        Some(crate::handles::LifecycleHandle::Work(work)) => {
            work.cancel();
            0
        }
        Some(_) => -1,
        None => -2,
    }
}

/// The vtable pointer passed to every component callback.
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
    component: v_component,
    fiber_dispose: v_fiber_dispose,
    run_blocking: v_run_blocking,
    work_cancelled: v_work_cancelled,
    work_cancel: v_work_cancel,
};

// Context is referenced in doc comments and the adapter lives beside us.
#[allow(unused)]
fn _context_type_used() -> Option<Context> {
    None
}
