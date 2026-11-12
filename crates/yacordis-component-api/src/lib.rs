//! Stable C-ABI boundary between a cordis host and components compiled as
//! cdylib binaries.
//!
//! Prebuilt component binaries cannot be assumed to share the host's rustc
//! version or dependency set, so the boundary deliberately does **not**
//! pass Rust types across. Everything crosses as one of:
//!
//! - **JSON bytes** ([`RawBuffer`]) — configs, event payloads, service
//!   values, error messages (`serde_json::Value` on both sides);
//! - **u64 handles** — references to host-side objects (contexts,
//!   fibers, effects) owned by the host's handle table;
//! - **[`Callback`] function pointers** — component code the host invokes
//!   later (component bodies, listeners, disposers, effect setups).
//!
//! The [`Component`] trait plus [`Context`] mirror the shape of the native
//! `yacordis::Component` API, so a component source looks the same either way;
//! [`register_component!`] generates the exported symbols.
//!
//! # Authoring a component
//!
//! ```no_run
//! use yacordis_component_api::prelude::*;
//!
//! struct Greeter;
//!
//! impl Component for Greeter {
//!     fn name(&self) -> Option<&str> { Some("greeter") }
//!     fn inject(&self) -> Inject { Inject::required(&["database"]) }
//!     fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
//!         let greeting = config["greeting"].as_str().unwrap_or("hello").to_owned();
//!         ctx.on("greet", move |_, args| {
//!             Some(serde_json::json!({ "text": format!("{greeting}, {}!", args["who"]) }))
//!         }).map_err(ComponentError::new)?;
//!         Ok(())
//!     }
//! }
//!
//! register_component!(Greeter);
//! ```
//!
//! Build with `crate-type = ["cdylib"]`; the host loads the resulting
//! binary with `yacordis-loader`.
//!
//! # Memory protocol
//!
//! Buffers travel through the process's shared `malloc`/`free`, and the
//! rule is symmetrical on both sides:
//!
//! - the *callee* of a cross-boundary call frees the **input** buffers
//!   it was handed once it is done reading them;
//! - the *caller* frees the **output** buffers after reading them
//!   (through `HostVTable::free_buffer`, or the component's exported
//!   `yacordis_component_free_buffer` — both back the same allocator).
//!
//! # Trust model
//!
//! Loading a cdylib executes third-party native code in-process — the
//! same trust level as `require()` in the TypeScript original; the ABI
//! is a compatibility layer.
//!
//! # Thread safety
//!
//! The host may invoke (and drop) callbacks from any thread; component
//! closures must be `Send + Sync` in effect. The constructors enforce
//! this at the type level before erasure into [`Callback`].

#![deny(missing_docs)]

use serde_json::Value;

mod abi;
mod buffer;
mod callback;
mod component;
mod context;

pub use abi::log_level;
pub use abi::{
    ABI_VERSION, CODE_PANIC, CODE_REUSED, Callback, HostVTable, RawBuffer, RawCallbacks,
};
pub use buffer::{alloc_buffer, buffer_from_vec, buffer_into_vec, buffer_to_string, free_buffer};
pub use callback::Disposable;
pub use component::{Component, ComponentError, ComponentResult, Inject};
pub use context::{Context, EffectHandle, ListenerHandle, LogLevel, WorkOutcome, WorkToken};

/// Generate the exported symbols the loader expects.
///
/// Takes an expression that constructs the component instance; it is built
/// once on first use and lives for the whole process:
///
/// ```ignore
/// register_component!(Greeter::new());
/// ```
#[macro_export]
macro_rules! register_component {
    ($component:expr) => {
        static __CORDIS_COMPONENT: std::sync::OnceLock<Box<dyn $crate::Component>> =
            std::sync::OnceLock::new();

        fn __yacordis_component() -> &'static dyn $crate::Component {
            let boxed = __CORDIS_COMPONENT.get_or_init(|| Box::new($component));
            let reference: &'static dyn $crate::Component = &**boxed;
            reference
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn yacordis_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn yacordis_component_manifest() -> $crate::RawBuffer {
            $crate::__manifest(__yacordis_component())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn yacordis_component_apply() -> $crate::Callback {
            $crate::__apply_callback(__yacordis_component())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn yacordis_component_free_buffer(buffer: $crate::RawBuffer) {
            $crate::free_buffer(buffer);
        }
    };
}

/// Ergonomic imports for component authors.
pub mod prelude {
    pub use crate::callback::Disposable;
    pub use crate::component::{Component, ComponentError, ComponentResult, Inject};
    pub use crate::context::{
        Context, EffectHandle, ListenerHandle, LogLevel, WorkOutcome, WorkToken,
    };
    pub use crate::register_component;
    pub use serde_json::{Value, json};
}

/// Serialize a component's manifest for the boundary.
#[must_use]
pub fn __manifest(component: &dyn Component) -> RawBuffer {
    context::build_manifest(component.name(), &component.inject())
}

/// Build the apply callback for a component instance that lives for the
/// whole process (`&'static`).
#[must_use]
pub fn __apply_callback(component: &'static dyn Component) -> Callback {
    // Store the wide reference so the non-generic trampoline can
    // reconstruct `&dyn Component` (raw pointers to unsized types cannot be
    // cast back with `as`).
    let storage: &'static &'static dyn Component = Box::leak(Box::new(component));
    let user_data: *const &'static dyn Component = storage;
    Callback {
        user_data: user_data.cast::<std::os::raw::c_void>().cast_mut(),
        invoke: Some(apply_trampoline_static),
        drop_fn: None,
    }
}

extern "C" fn apply_trampoline_static(
    user_data: *mut std::os::raw::c_void,
    env: *const HostVTable,
    ctx: u64,
    args: RawBuffer,
    out_json: *mut RawBuffer,
    _out_callbacks: *mut RawCallbacks,
) -> i32 {
    // SAFETY: user_data is the leaked `&'static dyn Component` storage from
    // `__apply_callback`; env is the host vtable.
    let component = unsafe { &*user_data.cast::<&'static dyn Component>() };
    let config = serde_json::from_slice(&buffer::buffer_into_vec(args)).unwrap_or(Value::Null);
    buffer::free_buffer(args);
    let context = Context::from_raw(env, ctx);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        component.apply(&context, &config)
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(err)) => {
            // SAFETY: valid out-pointer from the host.
            unsafe {
                *out_json = buffer::buffer_from_vec(err.to_string().into_bytes());
            }
            3
        }
        Err(_) => CODE_PANIC,
    }
}
