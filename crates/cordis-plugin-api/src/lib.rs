//! Stable C-ABI boundary between a cordis host and plugins compiled as
//! cdylib binaries.
//!
//! Prebuilt plugin binaries cannot be assumed to share the host's rustc
//! version or dependency set, so the boundary deliberately does **not**
//! pass Rust types across. Everything crosses as one of:
//!
//! - **JSON bytes** ([`RawBuffer`]) — configs, event payloads, service
//!   values, error messages (`serde_json::Value` on both sides);
//! - **u64 handles** — references to host-side objects (contexts,
//!   fibers, effects) owned by the host's handle table;
//! - **[`Callback`] function pointers** — plugin code the host invokes
//!   later (plugin bodies, listeners, disposers, effect setups).
//!
//! The [`Plugin`] trait plus [`Context`] mirror the shape of the native
//! `cordis::Plugin` API, so a plugin source looks the same either way;
//! [`register_plugin!`] generates the exported symbols.
//!
//! # Authoring a plugin
//!
//! ```no_run
//! use cordis_plugin_api::prelude::*;
//!
//! struct Greeter;
//!
//! impl Plugin for Greeter {
//!     fn name(&self) -> Option<&str> { Some("greeter") }
//!     fn inject(&self) -> Inject { Inject::required(&["database"]) }
//!     fn apply(&self, ctx: &Context, config: &Value) -> PluginResult {
//!         let greeting = config["greeting"].as_str().unwrap_or("hello").to_owned();
//!         ctx.on("greet", move |_, args| {
//!             Some(serde_json::json!({ "text": format!("{greeting}, {}!", args["who"]) }))
//!         }).map_err(PluginError::new)?;
//!         Ok(())
//!     }
//! }
//!
//! register_plugin!(Greeter);
//! ```
//!
//! Build with `crate-type = ["cdylib"]`; the host loads the resulting
//! binary with `cordis-loader`.
//!
//! # Memory protocol
//!
//! Buffers travel through the process's shared `malloc`/`free`, and the
//! rule is symmetrical on both sides:
//!
//! - the *callee* of a cross-boundary call frees the **input** buffers
//!   it was handed once it is done reading them;
//! - the *caller* frees the **output** buffers after reading them
//!   (through `HostVTable::free_buffer`, or the plugin's exported
//!   `cordis_plugin_free_buffer` — both back the same allocator).
//!
//! # Trust model
//!
//! Loading a cdylib executes third-party native code in-process — the
//! same trust level as `require()` in the TypeScript original. The ABI
//! provides compatibility, not sandboxing.
//!
//! # Thread safety
//!
//! The host may invoke (and drop) callbacks from any thread; plugin
//! closures must be `Send + Sync` in effect. The constructors enforce
//! this at the type level before erasure into [`Callback`].

#![deny(missing_docs)]

use serde_json::Value;

mod abi;
mod buffer;
mod callback;
mod context;
mod plugin;

pub use abi::log_level;
pub use abi::{
    ABI_VERSION, CODE_PANIC, CODE_REUSED, Callback, HostVTable, RawBuffer, RawCallbacks,
};
pub use buffer::{alloc_buffer, buffer_from_vec, buffer_into_vec, buffer_to_string, free_buffer};
pub use callback::Disposable;
pub use context::{Context, EffectHandle, ListenerHandle, LogLevel, WorkOutcome, WorkToken};
pub use plugin::{Inject, Plugin, PluginError, PluginResult};

/// Generate the exported symbols the loader expects.
///
/// Takes an expression that constructs the plugin instance; it is built
/// once on first use and lives for the whole process:
///
/// ```ignore
/// register_plugin!(Greeter::new());
/// ```
#[macro_export]
macro_rules! register_plugin {
    ($plugin:expr) => {
        static __CORDIS_PLUGIN: std::sync::OnceLock<Box<dyn $crate::Plugin>> =
            std::sync::OnceLock::new();

        fn __cordis_plugin() -> &'static dyn $crate::Plugin {
            let boxed = __CORDIS_PLUGIN.get_or_init(|| Box::new($plugin));
            let reference: &'static dyn $crate::Plugin = &**boxed;
            reference
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn cordis_abi_version() -> u32 {
            $crate::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn cordis_plugin_manifest() -> $crate::RawBuffer {
            $crate::__manifest(__cordis_plugin())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn cordis_plugin_apply() -> $crate::Callback {
            $crate::__apply_callback(__cordis_plugin())
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn cordis_plugin_free_buffer(buffer: $crate::RawBuffer) {
            $crate::free_buffer(buffer);
        }
    };
}

/// Ergonomic imports for plugin authors.
pub mod prelude {
    pub use crate::callback::Disposable;
    pub use crate::context::{Context, EffectHandle, ListenerHandle, LogLevel, WorkOutcome, WorkToken};
    pub use crate::plugin::{Inject, Plugin, PluginError, PluginResult};
    pub use crate::register_plugin;
    pub use serde_json::{Value, json};
}

/// Serialize a plugin's manifest for the boundary.
#[must_use]
pub fn __manifest(plugin: &dyn Plugin) -> RawBuffer {
    context::build_manifest(plugin.name(), &plugin.inject())
}

/// Build the apply callback for a plugin instance that lives for the
/// whole process (`&'static`).
#[must_use]
pub fn __apply_callback(plugin: &'static dyn Plugin) -> Callback {
    // Store the wide reference so the non-generic trampoline can
    // reconstruct `&dyn Plugin` (raw pointers to unsized types cannot be
    // cast back with `as`).
    let storage: &'static &'static dyn Plugin = Box::leak(Box::new(plugin));
    let user_data: *const &'static dyn Plugin = storage;
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
    // SAFETY: user_data is the leaked `&'static dyn Plugin` storage from
    // `__apply_callback`; env is the host vtable.
    let plugin = unsafe { &*user_data.cast::<&'static dyn Plugin>() };
    let config = serde_json::from_slice(&buffer::buffer_into_vec(args)).unwrap_or(Value::Null);
    buffer::free_buffer(args);
    let context = Context::from_raw(env, ctx);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        plugin.apply(&context, &config)
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
