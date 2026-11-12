//! Runtime installer for cordis components distributed as prebuilt cdylib
//! binaries.
//!
//! This is the Rust answer to cordis's loader: applications install
//! brand-new components — downloaded from the internet, dropped in as
//! files, or built from source on the spot — into a **running** context.
//!
//! Components are ordinary `cdylib` crates built against
//! [`yacordis-component-api`](yacordis_component_api); the boundary is a C ABI
//! (JSON bytes + handles + callback trampolines), so a component binary
//! works across the host's rustc versions and dependency graphs.
//!
//! ```no_run
//! use yacordis_loader::Loader;
//!
//! fn run() -> Result<(), Box<dyn std::error::Error>> {
//!     let ctx = yacordis::Context::new();
//!
//!     // From a file (or URL with the `download` feature):
//!     let component = Loader::load_file("./greeter.so")?;
//!
//!     println!("installing {}", component.manifest.name.clone().unwrap_or_default());
//!     let handle = component.install(&ctx, serde_json::json!({ "greeting": "hello" }))?;
//!
//!     // Fully dynamic lifecycle, same as native components:
//!     handle.update(serde_json::json!({ "greeting": "hola" }))?;
//!     handle.dispose();
//!     Ok(())
//! }
//! ```
//!
//! The developer path — write a component, build, install without
//! restarting — is [`Loader::build_and_load`], which shells out to
//! `cargo` and loads the fresh cdylib.
//!
//! # Trust model
//!
//! Loading a cdylib executes native code in-process: the same trust
//! level as `require()` in the TypeScript original, and deliberately
//! **not** a sandbox. For isolation, prefer a process- or WASM-based
//! design; this crate optimizes for full-fidelity native components.

#![deny(missing_docs)]

mod adapter;
mod handles;
mod loader;
mod registry;
mod vtable;

pub use loader::{
    BuildOptions, ComponentLibrary, ComponentManifest, LoadError, Loader, lint_source,
};
pub use registry::{
    Constraint, Entry, RegistryError, RegistryIndex, Repository, Sha256Verifier, Verifier, Version,
    sha256 as sha256_digest, sha256_hex,
};

/// Re-export of the ABI crate so hosts and components can agree on types.
pub use yacordis_component_api as abi;

/// Dispose-time audit hook (§4.1 I2, host half): collect every context
/// handle bound to `fiber_id`. Exposed for tests and host tooling.
#[doc(hidden)]
pub fn audit_fiber_handles(fiber_id: u64) -> usize {
    handles::HandleTable::global().audit_fiber(fiber_id)
}
