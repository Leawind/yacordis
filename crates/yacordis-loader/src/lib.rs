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
//! # Unloading
//!
//! Components unload for real. [`ComponentLibrary::unload`] unmaps the
//! binary once the host has proven a quiet state — every fiber disposed,
//! no component code in flight — and on Windows that releases the file
//! lock. When the proof fails, the unload is refused with the violation
//! named (an [`UnloadError`]) and the library stays mapped: a contract
//! violation degrades at worst to a memory leak, never to a crash.
//!
//! A host that rebuilds an artifact before unloading it loads a
//! materialized copy ([`Loader::load_materialized`]) instead of mapping the
//! artifact in place: the mapped file is then a content-addressed copy, so
//! the producer's path stays writable while the previous copy is still
//! mapped, and a rebuild cannot truncate the bytes under an existing
//! mapping on Unix. In-place loading remains right for prebuilt binaries
//! that never change under the process. When even the copy should not
//! outlive its mapping, the ephemeral variants
//! ([`Loader::load_materialized_ephemeral`],
//! [`Loader::load_bytes_ephemeral`]) map a private copy and remove it —
//! at load on POSIX, at unload on Windows — leaving nothing behind on a
//! failed load either.
//!
//! The proof is the component author's responsibility to make possible.
//! The contract's invariants, in code terms:
//!
//! - **I1 (single scheduling domain)** — only run code when the framework
//!   calls you (`apply`, effect setups, listeners, work callbacks,
//!   disposers). Never `std::thread::spawn` or run a scheduler of your
//!   own: a thread that outlives the framework's calls is in flight
//!   forever, unload either refuses or — if the thread never calls back
//!   into the framework, where the host could see it — cannot prove
//!   anything and unmapping becomes undefined behavior. Framework work
//!   (`ctx.run_blocking`, `ctx.spawn_future`) is drained by dispose, so it
//!   never blocks an unload.
//! - **I2 (resource ownership)** — acquire everything through the
//!   `Context` (services, effects, listeners, child components) and free
//!   what you allocate through the buffer protocol; nothing escapes the
//!   component body scope. On dispose the host audits its own half (all
//!   context handles reclaimed); your half is what makes the effect
//!   rollback actually clean.
//! - **I3 (no cross-install state)** — no mutable state that survives
//!   across installs: behavior is a pure function of (config, service
//!   view, received events). No interior-mutable `static`s carrying
//!   behavior. A reloaded component is a fresh instance, which is what
//!   makes unload → rebuild → reload in place produce correct semantics.
//!
//! Prebuilt distribution cannot re-run the source lints (`Loader::build`
//! with `lint: true` enforces them at build time), so unloading a binary
//! you did not build from linted sources is trusting its publisher — the
//! runtime refusal above is the auditable remainder.

#![deny(missing_docs)]

mod adapter;
mod digest;
mod handles;
mod lease;
mod loader;
mod registry;
mod vtable;

pub use digest::{sha256 as sha256_digest, sha256_hex};
pub use lease::UnloadError;
pub use loader::{
    BuildOptions, ComponentLibrary, ComponentManifest, LoadError, Loader, MaterializedArtifact,
    lint_source,
};
pub use registry::{
    Constraint, Entry, RegistryError, RegistryIndex, Repository, Sha256Verifier, Verifier, Version,
};

/// Re-export of the ABI crate so hosts and components can agree on types.
pub use yacordis_component_api as abi;

/// Dispose-time audit hook (I2, resource ownership — host half): collect every context
/// handle bound to `fiber_id`. Exposed for tests and host tooling.
#[doc(hidden)]
pub fn audit_fiber_handles(fiber_id: u64) -> usize {
    handles::HandleTable::global().audit_fiber(fiber_id)
}
