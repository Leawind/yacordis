//! Runtime installer for cordis plugins distributed as prebuilt cdylib
//! binaries.
//!
//! This is the Rust answer to cordis's loader: applications install
//! brand-new plugins — downloaded from the internet, dropped in as
//! files, or built from source on the spot — into a **running** context.
//!
//! Plugins are ordinary `cdylib` crates built against
//! [`cordis-plugin-api`](cordis_plugin_api); the boundary is a C ABI
//! (JSON bytes + handles + callback trampolines), so a plugin binary
//! works across the host's rustc versions and dependency graphs.
//!
//! ```no_run
//! use cordis_loader::Loader;
//!
//! fn run() -> Result<(), Box<dyn std::error::Error>> {
//!     let ctx = cordis::Context::new();
//!
//!     // From a file (or URL with the `download` feature):
//!     let plugin = Loader::load_file("./greeter.so")?;
//!
//!     println!("installing {}", plugin.manifest.name.clone().unwrap_or_default());
//!     let handle = plugin.install(&ctx, serde_json::json!({ "greeting": "hello" }))?;
//!
//!     // Fully dynamic lifecycle, same as native plugins:
//!     handle.update(serde_json::json!({ "greeting": "hola" }))?;
//!     handle.dispose();
//!     Ok(())
//! }
//! ```
//!
//! The developer path — write a plugin, build, install without
//! restarting — is [`Loader::build_and_load`], which shells out to
//! `cargo` and loads the fresh cdylib.
//!
//! # Trust model
//!
//! Loading a cdylib executes native code in-process: the same trust
//! level as `require()` in the TypeScript original, and deliberately
//! **not** a sandbox. For isolation, prefer a process- or WASM-based
//! design; this crate optimizes for full-fidelity native plugins.

#![deny(missing_docs)]

mod adapter;
mod handles;
mod loader;
mod registry;
mod vtable;

pub use loader::{
    BuildOptions, LoadError, Loader, PluginLibrary, PluginManifest, lint_source,
};
pub use registry::{
    Constraint, Entry, RegistryError, RegistryIndex, Repository, Sha256Verifier, Verifier,
    sha256_hex, sha256 as sha256_digest, Version,
};

/// Re-export of the ABI crate so hosts and plugins can agree on types.
pub use cordis_plugin_api as abi;

/// Dispose-time audit hook (§4.1 I2, host half): collect every context
/// handle bound to `fiber_id`. Exposed for tests and host tooling.
#[doc(hidden)]
pub fn audit_fiber_handles(fiber_id: u64) -> usize {
    handles::HandleTable::global().audit_fiber(fiber_id)
}
