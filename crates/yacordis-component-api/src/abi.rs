//! C-ABI types shared by host and components: buffers, callbacks, vtable.
//!
//! This module is compiled into both sides; its layout is frozen by
//! [`ABI_VERSION`].

use std::os::raw::c_void;

/// Version of the ABI implemented by this crate. The host refuses to
/// load components whose exported version differs. v2 added the
/// `run_blocking`/`work_cancelled`/`work_cancel` host functions; v3
/// renamed the exported symbols from `cordis_plugin_*` to
/// `cordis_component_*` (upstream cordis#307 terminology); v4 renamed
/// them to `yacordis_component_*` when the project became yacordis.
pub const ABI_VERSION: u32 = 4;

/// A byte buffer crossing the boundary.
///
/// Allocated by one side (length `len`, at least one byte), consumed by
/// the other, and always freed by the allocating side through its own
/// exported free function.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawBuffer {
    /// Pointer to the bytes. Allocated buffers never use null — an empty
    /// buffer still allocates one byte — so null identifies
    /// [`RawBuffer::EMPTY`], the output placeholder a callee fills in.
    pub ptr: *mut u8,
    /// Length in bytes.
    pub len: usize,
}

impl RawBuffer {
    /// An empty buffer to be filled by the callee.
    pub const EMPTY: RawBuffer = RawBuffer {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
}

/// A callee-owned array of [`Callback`] values (e.g. the disposers an
/// effect setup produced). Freed by the producing side's free function
/// after the consumer has copied the values out.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawCallbacks {
    /// Pointer to `len` consecutive `Callback` values.
    pub ptr: *mut Callback,
    /// Number of callbacks.
    pub len: usize,
}

impl RawCallbacks {
    /// An empty array to be filled by the callee.
    pub const EMPTY: RawCallbacks = RawCallbacks {
        ptr: std::ptr::null_mut(),
        len: 0,
    };
}

/// Component code the host can invoke (and drop) later.
///
/// Constructed only through [`crate::callback`] constructors; the raw
/// struct is `Copy` so arrays of callbacks can cross the boundary.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callback {
    /// Erased state owned by the component side.
    pub user_data: *mut c_void,
    /// Invocation entry point; `None` marks an inert placeholder.
    pub invoke: Option<RawInvoke>,
    /// Releases `user_data`; called exactly once by the host when the
    /// callback record is removed.
    pub drop_fn: Option<unsafe extern "C" fn(user_data: *mut c_void)>,
}

/// The function the host calls to execute component code.
///
/// Return codes:
///
/// - `0` — success, no output;
/// - `1` — `out_json` holds a JSON result (listener bail value);
/// - `2` — `out_callbacks` holds disposers (effect setup / apply);
/// - `3` — `out_json` holds a UTF-8 error message (failure);
/// - `-1` — the component panicked; message unavailable;
/// - `-2` — one-shot callback invoked again after completion.
///
/// # Safety (contract)
///
/// `user_data` must have been produced by this crate's constructors.
/// `env` must be the host vtable pointer the callback was registered
/// with. The callee must free `args` through `env.free_buffer`.
pub type RawInvoke = unsafe extern "C" fn(
    user_data: *mut c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    out_callbacks: *mut RawCallbacks,
) -> i32;

/// Functions the host exposes to components. A single static instance is
/// shared by everything the host loads; its layout is frozen by
/// [`ABI_VERSION`].
#[repr(C)]
pub struct HostVTable {
    /// Must equal [`ABI_VERSION`].
    pub abi_version: u32,
    /// Free any boundary buffer (allocated by either side; backed by the
    /// process `malloc`/`free`, which both binaries share).
    pub free_buffer: unsafe extern "C" fn(RawBuffer),
    /// Forward a component log line into the host's logger.
    pub log: unsafe extern "C" fn(level: i32, target: RawBuffer, message: RawBuffer),
    /// Register a revertible effect on the context's fiber; returns the
    /// effect handle id (0 on failure).
    pub effect: unsafe extern "C" fn(ctx: u64, label: RawBuffer, setup: Callback) -> u64,
    /// Register a listener; returns the listener handle id (0 on failure).
    pub on: unsafe extern "C" fn(ctx: u64, name: RawBuffer, listener: Callback) -> u64,
    /// Register a one-shot listener; returns the handle id (0 on failure).
    pub once: unsafe extern "C" fn(ctx: u64, name: RawBuffer, listener: Callback) -> u64,
    /// Remove a listener/effect handle previously returned by `on`/`once`/`effect`.
    pub dispose_handle: unsafe extern "C" fn(handle: u64) -> i32,
    /// Provide a JSON service from the context's fiber (0 on success).
    pub provide: unsafe extern "C" fn(ctx: u64, name: RawBuffer, value: RawBuffer) -> i32,
    /// Replace the value of a service provided by the same fiber (0 on success).
    pub set: unsafe extern "C" fn(ctx: u64, name: RawBuffer, value: RawBuffer) -> i32,
    /// Read a service as JSON; returns false when absent or not JSON.
    pub get: unsafe extern "C" fn(ctx: u64, name: RawBuffer, out: *mut RawBuffer) -> bool,
    /// Whether a service is resolvable from this context.
    pub has: unsafe extern "C" fn(ctx: u64, name: RawBuffer) -> bool,
    /// Dispatch an event.
    pub emit: unsafe extern "C" fn(ctx: u64, name: RawBuffer, args: RawBuffer),
    /// Dispatch with bail semantics; returns true when `out` was filled.
    pub bail: unsafe extern "C" fn(
        ctx: u64,
        name: RawBuffer,
        args: RawBuffer,
        out: *mut RawBuffer,
    ) -> bool,
    /// Install a sub-component (manifest + apply callback + JSON config);
    /// returns the child fiber handle id (0 on failure).
    pub component: unsafe extern "C" fn(
        ctx: u64,
        manifest: RawBuffer,
        apply: Callback,
        config: RawBuffer,
    ) -> u64,
    /// Dispose a fiber previously returned by `component` (0 = disposed now,
    /// 1 = already disposed, negative = error).
    pub fiber_dispose: unsafe extern "C" fn(handle: u64) -> i32,
    /// Run a work unit on a framework thread. For the `work` callback,
    /// the `ctx` argument carries the cancellation-token handle (poll it
    /// via `work_cancelled`), not a context handle. Returns the work
    /// handle id (0 on failure).
    pub run_blocking:
        unsafe extern "C" fn(ctx: u64, label: RawBuffer, work: Callback, on_done: Callback) -> u64,
    /// Whether a work cancellation token (the handle passed to the work
    /// callback as `ctx`) has been cancelled: 1 = cancelled, 0 = not,
    /// negative = unknown token.
    pub work_cancelled: unsafe extern "C" fn(token: u64) -> i32,
    /// Request cancellation of a work unit returned by `run_blocking`
    /// (0 = cancelled, negative = unknown handle).
    pub work_cancel: unsafe extern "C" fn(handle: u64) -> i32,
}

/// Log levels for [`HostVTable::log`], mirroring `yacordis::LogLevel`.
pub mod log_level {
    /// Verbose diagnostics.
    pub const DEBUG: i32 = 0;
    /// Normal operational messages.
    pub const INFO: i32 = 1;
    /// Something looks wrong.
    pub const WARN: i32 = 2;
    /// An operation failed.
    pub const ERROR: i32 = 3;
}

/// Panic code returned by trampolines when component code panics.
pub const CODE_PANIC: i32 = -1;
/// Code returned when a one-shot callback is invoked a second time.
pub const CODE_REUSED: i32 = -2;
