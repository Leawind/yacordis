//! [`Callback`] constructors: erase component closures into C-ABI
//! function-pointer bundles the host can invoke (and drop) later.
//!
//! Every trampoline catches panics (`-1`), frees the `args` buffer the
//! host handed over (through the host's own `free_buffer`), and mutates
//! its state in place — the state box itself stays allocated until the
//! host calls `drop_fn`, so removal after a one-shot invocation is
//! always safe. A one-shot callback invoked twice answers
//! [`CODE_REUSED`](crate::CODE_REUSED).

use std::os::raw::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::Value;

use crate::abi::{CODE_PANIC, CODE_REUSED, Callback, HostVTable, RawBuffer, RawCallbacks};
use crate::buffer;
use crate::context::Context;

/// A cleanup function the component hands to the host; runs when the
/// surrounding effect or fiber is reverted.
pub struct Disposable(Callback);

impl Disposable {
    /// Wrap a cleanup closure.
    pub fn new<F: FnOnce() + Send + 'static>(f: F) -> Self {
        Self(erase(f, disposer_trampoline::<F>))
    }

    pub(crate) fn into_raw(self) -> Callback {
        self.0
    }
}

/// Erased callback state. `f` is `take()`n by one-shot trampolines; the
/// box itself is only freed by `drop_fn`.
struct State<F> {
    f: Option<F>,
}

fn erase<F>(
    f: F,
    invoke: unsafe extern "C" fn(
        *mut c_void,
        *const HostVTable,
        u64,
        RawBuffer,
        *mut RawBuffer,
        *mut RawCallbacks,
    ) -> i32,
) -> Callback
where
    F: Send + 'static,
{
    let user_data = Box::into_raw(Box::new(State { f: Some(f) })).cast::<c_void>();
    Callback {
        user_data,
        invoke: Some(invoke),
        drop_fn: Some(drop_state::<F>),
    }
}

unsafe extern "C" fn drop_state<F>(user_data: *mut c_void) {
    // SAFETY: produced by `erase` for the matching state type; called
    // exactly once by the host.
    unsafe {
        drop(Box::from_raw(user_data.cast::<State<F>>()));
    }
}

/// # Safety
/// `state_ptr` must be the user_data of a live `State<F>`.
unsafe fn with_state<F>(state_ptr: *mut c_void, run: impl FnOnce(&mut State<F>) -> i32) -> i32 {
    // SAFETY: initialized box owned by the host's callback record; the
    // host serializes invocations of a single callback record.
    let state = unsafe { &mut *state_ptr.cast::<State<F>>() };
    match catch_unwind(AssertUnwindSafe(|| run(state))) {
        Ok(code) => code,
        Err(_) => CODE_PANIC,
    }
}

fn write_json(out: *mut RawBuffer, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    // SAFETY: the host always passes a valid out-pointer.
    unsafe {
        *out = buffer::buffer_from_vec(bytes);
    }
}

fn write_error_message(out: *mut RawBuffer, message: &str) {
    // SAFETY: the host always passes a valid out-pointer.
    unsafe {
        *out = buffer::buffer_from_vec(message.as_bytes().to_vec());
    }
}

/// Free the `args` buffer the host handed to us. Boundary buffers use
/// the shared `malloc`/`free`, so freeing from the component side is always
/// correct regardless of allocator.
fn release_args(args: RawBuffer) {
    buffer::free_buffer(args);
}

impl Callback {
    /// A callback that never does anything (placeholder for empty
    /// protocol slots).
    pub const NULL: Callback = Callback {
        user_data: std::ptr::null_mut(),
        invoke: None,
        drop_fn: None,
    };

    /// Erase a listener: invoked on every dispatch with the event
    /// payload; a returned value becomes the bail result.
    pub fn listener<F>(f: F) -> Callback
    where
        F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
    {
        erase(f, listener_trampoline::<F>)
    }

    /// Erase a component body: invoked once per (re)load with the config.
    pub fn apply<F>(f: F) -> Callback
    where
        F: Fn(&Context, &Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
            + Send
            + Sync
            + 'static,
    {
        erase(f, apply_trampoline::<F>)
    }

    /// Erase an effect setup: invoked once when the effect is created;
    /// returned disposers are adopted by the host's effect tracking.
    pub fn effect_setup<F>(f: F) -> Callback
    where
        F: FnOnce(&Context) -> Result<Vec<Disposable>, Box<dyn std::error::Error + Send + Sync>>
            + Send
            + 'static,
    {
        erase(f, effect_setup_trampoline::<F>)
    }

    /// Erase a blocking-work closure: invoked once on a framework worker
    /// thread; the `ctx` argument it sees is the cancellation-token
    /// handle (not a context) — poll it through
    /// [`HostVTable::work_cancelled`](crate::HostVTable::work_cancelled).
    /// A returned value becomes the work result; an error becomes the
    /// failure message.
    pub fn work_fn<F>(f: F) -> Callback
    where
        F: Fn(u64, &'static HostVTable) -> Result<Value, String> + Send + Sync + 'static,
    {
        erase(f, work_trampoline::<F>)
    }
}

extern "C" fn disposer_trampoline<F>(
    user_data: *mut c_void,
    _env: *const HostVTable,
    _ctx: u64,
    args: RawBuffer,
    _out_json: *mut RawBuffer,
    _out_callbacks: *mut RawCallbacks,
) -> i32
where
    F: FnOnce() + Send + 'static,
{
    // SAFETY: state matches the constructor.
    let code = unsafe {
        with_state::<F>(user_data, |state| {
            let Some(f) = state.f.take() else {
                return CODE_REUSED;
            };
            f();
            0
        })
    };
    release_args(args);
    code
}

extern "C" fn listener_trampoline<F>(
    user_data: *mut c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    _out_callbacks: *mut RawCallbacks,
) -> i32
where
    F: Fn(&Context, &Value) -> Option<Value> + Send + Sync + 'static,
{
    let payload = parse_args(args);
    // SAFETY: state matches the constructor.
    let code = unsafe {
        with_state::<F>(user_data, |state| {
            let Some(f) = state.f.as_ref() else {
                return CODE_REUSED;
            };
            let context = Context::from_raw(env, ctx);
            match f(&context, &payload) {
                Some(value) => {
                    write_json(out_json, &value);
                    1
                }
                None => 0,
            }
        })
    };
    release_args(args);
    code
}

extern "C" fn apply_trampoline<F>(
    user_data: *mut c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    _out_callbacks: *mut RawCallbacks,
) -> i32
where
    F: Fn(&Context, &Value) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
        + Send
        + Sync
        + 'static,
{
    let config = parse_args(args);
    // SAFETY: state matches the constructor.
    let code = unsafe {
        with_state::<F>(user_data, |state| {
            let Some(f) = state.f.as_ref() else {
                return CODE_REUSED;
            };
            let context = Context::from_raw(env, ctx);
            match f(&context, &config) {
                Ok(()) => 0,
                Err(err) => {
                    write_error_message(out_json, &err.to_string());
                    3
                }
            }
        })
    };
    release_args(args);
    code
}

extern "C" fn effect_setup_trampoline<F>(
    user_data: *mut c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    out_callbacks: *mut RawCallbacks,
) -> i32
where
    F: FnOnce(&Context) -> Result<Vec<Disposable>, Box<dyn std::error::Error + Send + Sync>>
        + Send
        + 'static,
{
    // SAFETY: state matches the constructor.
    let code = unsafe {
        with_state::<F>(user_data, |state| {
            // One-shot: take the closure, leave the (now empty) state box
            // for drop_fn to release.
            let Some(f) = state.f.take() else {
                return CODE_REUSED;
            };
            let context = Context::from_raw(env, ctx);
            match f(&context) {
                Ok(disposers) => {
                    let callbacks: Vec<Callback> =
                        disposers.into_iter().map(Disposable::into_raw).collect();
                    let len = callbacks.len();
                    let ptr = buffer::alloc_buffer(len * std::mem::size_of::<Callback>())
                        as *mut Callback;
                    if len > 0 {
                        std::ptr::copy_nonoverlapping(callbacks.as_ptr(), ptr, len);
                    }
                    // SAFETY: the host always passes a valid out-pointer.
                    *out_callbacks = RawCallbacks { ptr, len };
                    2
                }
                Err(err) => {
                    write_error_message(out_json, &err.to_string());
                    3
                }
            }
        })
    };
    release_args(args);
    code
}

extern "C" fn work_trampoline<F>(
    user_data: *mut c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    _out_callbacks: *mut RawCallbacks,
) -> i32
where
    F: Fn(u64, &'static HostVTable) -> Result<Value, String> + Send + Sync + 'static,
{
    // SAFETY: the host passes the process-static vtable pointer.
    let env_ref: &'static HostVTable = unsafe { &*env };
    let code = unsafe {
        with_state::<F>(user_data, |state| {
            let Some(f) = state.f.as_ref() else {
                return CODE_REUSED;
            };
            match f(ctx, env_ref) {
                Ok(value) => {
                    write_json(out_json, &value);
                    1
                }
                Err(message) => {
                    write_error_message(out_json, &message);
                    3
                }
            }
        })
    };
    release_args(args);
    code
}

fn parse_args(args: RawBuffer) -> Value {
    serde_json::from_slice(&buffer::buffer_into_vec(args)).unwrap_or(Value::Null)
}
