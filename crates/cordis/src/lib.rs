//! Cordis in Rust: spatiotemporal composability for plugins.
//!
//! This crate re-implements the core of
//! [Cordis](https://github.com/cordiverse/cordis) — the reference
//! implementation of the programming paradigm described in
//! [arXiv:2608.25512](https://arxiv.org/abs/2608.25512), *A Programming
//! Paradigm for Spatiotemporal Composability* — as a synchronous,
//! natively typed Rust library.
//!
//! Two orthogonal capabilities, one intermediary:
//!
//! - **Temporal composability** (revertible effects): every context
//!   transformation records its own inverse. Disposing a plugin reverts
//!   its side effects in reverse order, completely.
//! - **Spatial composability** (reactive coeffects): plugins declare the
//!   services they require; the runtime activates them when the services
//!   appear and deactivates them when the services disappear.
//!
//! # A tour
//!
//! ```no_run
//! use cordis::{Context, FiberState, Inject, Plugin, PluginResult};
//! use serde_json::json;
//! use std::sync::Arc;
//!
//! struct Database { uri: String }
//!
//! // A service plugin: provide something on your scoped context.
//! struct DatabasePlugin;
//! impl Plugin for DatabasePlugin {
//!     fn name(&self) -> Option<&str> { Some("database") }
//!     fn apply(&self, ctx: &cordis::Context, config: &serde_json::Value) -> PluginResult {
//!         let uri = config["uri"].as_str().unwrap_or_default().to_owned();
//!         ctx.provide("database", Arc::new(Database { uri }))?;
//!         Ok(())
//!     }
//! }
//!
//! // A consumer plugin: declare the coeffect, use the service.
//! struct AppPlugin;
//! impl Plugin for AppPlugin {
//!     fn inject(&self) -> Inject { Inject::required(&["database"]) }
//!     fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> PluginResult {
//!         let db = ctx.get::<Database>("database").expect("required dependency");
//!         println!("connected to {}", db.uri);
//!         Ok(())
//!     }
//! }
//!
//! # fn main() -> Result<(), cordis::CordisError> {
//! let ctx = Context::new();
//!
//! // Install at runtime; uninstall just as easily.
//! let db = ctx.plugin(DatabasePlugin, json!({ "uri": "postgres://localhost" }))?;
//! assert_eq!(db.state(), FiberState::Active);
//!
//! // `AppPlugin` was installed before its dependency? Install it now —
//! // activation is reactive.
//! let app = ctx.plugin(AppPlugin, json!(null))?;
//! assert_eq!(app.state(), FiberState::Active);
//!
//! // Disposing the provider deactivates the dependent; reinstalling
//! // reactivates it. Both in place.
//! db.dispose();
//! assert_eq!(app.state(), FiberState::Pending);
//! # Ok(())
//! # }
//! ```
//!
//! # The concurrency substrate (§ design.md 2.6)
//!
//! Blocking or async work never blocks the scheduling domain: submit it
//! through the framework, and unload drains it before rolling back.
//!
//! ```
//! use cordis::prelude::*;
//! use serde_json::json;
//!
//! # fn demo() -> Result<(), cordis::CordisError> {
//! let ctx = Context::new();
//! let worker = ctx.plugin(|ctx: &Context, _: &serde_json::Value| -> PluginResult {
//!     let (tx, rx) = std::sync::mpsc::channel();
//!     ctx.run_blocking("compute", |_| Ok(json!(21)), move |ctx, outcome| {
//!         if let WorkOutcome::Done(value) = outcome {
//!             ctx.emit("answer", &json!({ "value": value }));
//!             let _ = tx.send(());
//!         }
//!     })?;
//!     rx.recv().map_err(|e| PluginError::new(e.to_string()))?;
//!     Ok(())
//! }, json!(null))?;
//! assert!(worker.is_active());
//! worker.dispose();
//! # Ok(())
//! # }
//! # demo().unwrap();
//! ```
//!
//! # Intercepts and in-place replacement
//!
//! ```
//! use cordis::prelude::*;
//! use serde_json::{json, Value};
//! use std::sync::Arc;
//!
//! # fn demo() -> Result<(), cordis::CordisError> {
//! let ctx = Context::new();
//! let plugin: Arc<dyn Plugin> = Arc::new(
//!     |ctx: &Context, _: &Value| -> PluginResult {
//!         ctx.provide("cfg", Arc::new(json!({ "host": "localhost", "port": 80 })))?;
//!         Ok(())
//!     },
//! );
//! let provider = ctx.plugin_dyn(plugin.clone(), json!(null))?;
//!
//! // A JSON service can be layered with config overrides; the nearest
//! // fiber wins, entries die with their fiber.
//! let viewer = ctx.plugin(|ctx: &Context, _: &Value| -> PluginResult {
//!     ctx.intercept("cfg", json!({ "port": 8080 }))?;
//!     let cfg = ctx.get::<Value>("cfg").unwrap();
//!     assert_eq!(cfg["port"], json!(8080));
//!     Ok(())
//! }, json!(null))?;
//! assert!(viewer.is_active());
//!
//! // Swap the provider in place: identity (id/config) survives, the
//! // body re-runs, dependents observe the new service values.
//! let _handles = ctx.replace_plugin(plugin, Arc::new(|_: &Context, _: &Value| Ok(())))?;
//! assert!(provider.is_active(), "the fiber keeps its identity");
//! # Ok(())
//! # }
//! # demo().unwrap();
//! ```
//!
//! See `docs/design.md` in the repository for the concept mapping from
//! the paper / cordis v4 to this crate, and for the list of deliberate
//! divergences.

#![deny(missing_docs)]

mod context;
mod diagnose;
mod effect;
mod error;
mod events;
mod fiber;
mod logger;
mod plugin;
mod reflect;
mod registry;
mod schema;
mod work;

pub use context::{Context, FiberHandle};
pub use cordis_macros::plugin;
pub use effect::{Disposer, EffectHandle, EffectOutcome};
pub use error::{CordisError, PluginError, PluginResult};
pub use events::Event;
pub use fiber::FiberState;
pub use logger::{DiscardLogger, LogLevel, Logger, StderrLogger};
pub use plugin::{Inject, Plugin};
pub use schema::{Diagnostic, Schema, SchemaBuilder};
pub use serde_json;
pub use work::{CancellationToken, WorkHandle, WorkOutcome};

/// Internal diagnostics (used by the stress tests).
#[doc(hidden)]
pub mod test_support {
    pub fn pending_ops() -> i64 {
        crate::fiber::pending_ops()
    }

    pub fn lifecycle_trace_fiber(id: u64) -> Vec<String> {
        crate::fiber::lifecycle_trace::dump_fiber(id)
    }
}

/// Ergonomic imports for typical hosts and plugins.
pub mod prelude {
    pub use crate::context::{Context, FiberHandle};
    pub use crate::effect::{Disposer, EffectHandle, EffectOutcome};
    pub use crate::error::{CordisError, PluginError, PluginResult};
    pub use crate::events::Event;
    pub use crate::fiber::FiberState;
    pub use crate::logger::{DiscardLogger, LogLevel, Logger, StderrLogger};
    pub use crate::plugin;
    pub use crate::plugin::{Inject, Plugin};
    pub use crate::schema::{Diagnostic, Schema, SchemaBuilder};
    pub use crate::serde_json;
    pub use crate::work::{CancellationToken, WorkHandle, WorkOutcome};
}
