//! Cordis in Rust: spatiotemporal composability for components.
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
//!   transformation records its own inverse. Disposing a component reverts
//!   its side effects in reverse order, completely.
//! - **Spatial composability** (reactive coeffects): components declare the
//!   services they require; the runtime activates them when the services
//!   appear and deactivates them when the services disappear.
//!
//! # A tour
//!
//! ```no_run
//! use yacordis::{Context, FiberState, Inject, Component, ComponentResult};
//! use serde_json::json;
//! use std::sync::Arc;
//!
//! struct Database { uri: String }
//!
//! // A service component: provide something on your scoped context.
//! struct DatabaseComponent;
//! impl Component for DatabaseComponent {
//!     fn name(&self) -> Option<&str> { Some("database") }
//!     fn apply(&self, ctx: &yacordis::Context, config: &serde_json::Value) -> ComponentResult {
//!         let uri = config["uri"].as_str().unwrap_or_default().to_owned();
//!         ctx.provide("database", Arc::new(Database { uri }))?;
//!         Ok(())
//!     }
//! }
//!
//! // A consumer component: declare the coeffect, use the service.
//! struct AppComponent;
//! impl Component for AppComponent {
//!     fn inject(&self) -> Inject { Inject::required(&["database"]) }
//!     fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> ComponentResult {
//!         let db = ctx.get::<Database>("database").expect("required dependency");
//!         println!("connected to {}", db.uri);
//!         Ok(())
//!     }
//! }
//!
//! # fn main() -> Result<(), yacordis::CordisError> {
//! let ctx = Context::new();
//!
//! // Install at runtime; uninstall just as easily.
//! let db = ctx.component(DatabaseComponent, json!({ "uri": "postgres://localhost" }))?;
//! assert_eq!(db.state(), FiberState::Active);
//!
//! // `AppComponent` was installed before its dependency? Install it now —
//! // activation is reactive.
//! let app = ctx.component(AppComponent, json!(null))?;
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
//! use yacordis::prelude::*;
//! use serde_json::json;
//!
//! # fn demo() -> Result<(), yacordis::CordisError> {
//! let ctx = Context::new();
//! let worker = ctx.component(|ctx: &Context, _: &serde_json::Value| -> ComponentResult {
//!     let (tx, rx) = std::sync::mpsc::channel();
//!     ctx.run_blocking("compute", |_| Ok(json!(21)), move |ctx, outcome| {
//!         if let WorkOutcome::Done(value) = outcome {
//!             ctx.emit("answer", &json!({ "value": value }));
//!             let _ = tx.send(());
//!         }
//!     })?;
//!     rx.recv().map_err(|e| ComponentError::new(e.to_string()))?;
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
//! use yacordis::prelude::*;
//! use serde_json::{json, Value};
//! use std::sync::Arc;
//!
//! # fn demo() -> Result<(), yacordis::CordisError> {
//! let ctx = Context::new();
//! let component: Arc<dyn Component> = Arc::new(
//!     |ctx: &Context, _: &Value| -> ComponentResult {
//!         ctx.provide("cfg", Arc::new(json!({ "host": "localhost", "port": 80 })))?;
//!         Ok(())
//!     },
//! );
//! let provider = ctx.component_dyn(component.clone(), json!(null))?;
//!
//! // A JSON service can be layered with config overrides; the nearest
//! // fiber wins, entries die with their fiber.
//! let viewer = ctx.component(|ctx: &Context, _: &Value| -> ComponentResult {
//!     ctx.intercept("cfg", json!({ "port": 8080 }))?;
//!     let cfg = ctx.get::<Value>("cfg").unwrap();
//!     assert_eq!(cfg["port"], json!(8080));
//!     Ok(())
//! }, json!(null))?;
//! assert!(viewer.is_active());
//!
//! // Swap the provider in place: identity (id/config) survives, the
//! // body re-runs, dependents observe the new service values.
//! let _handles = ctx.replace_component(component, Arc::new(|_: &Context, _: &Value| Ok(())))?;
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

mod component;
mod context;
mod diagnose;
mod effect;
mod error;
mod events;
mod fiber;
mod logger;
mod reflect;
mod registry;
mod schema;
mod work;

pub use component::{Component, Inject};
pub use context::{Context, FiberHandle};
pub use effect::{Disposer, EffectHandle, EffectOutcome};
pub use error::{ComponentError, ComponentResult, CordisError};
pub use events::Event;
pub use fiber::FiberState;
pub use logger::{DiscardLogger, LogLevel, Logger, StderrLogger};
pub use schema::{Diagnostic, Schema, SchemaBuilder};
pub use serde_json;
pub use work::{CancellationToken, WorkHandle, WorkOutcome};
pub use yacordis_macros::component;

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

/// Ergonomic imports for typical hosts and components.
pub mod prelude {
    pub use crate::component;
    pub use crate::component::{Component, Inject};
    pub use crate::context::{Context, FiberHandle};
    pub use crate::effect::{Disposer, EffectHandle, EffectOutcome};
    pub use crate::error::{ComponentError, ComponentResult, CordisError};
    pub use crate::events::Event;
    pub use crate::fiber::FiberState;
    pub use crate::logger::{DiscardLogger, LogLevel, Logger, StderrLogger};
    pub use crate::schema::{Diagnostic, Schema, SchemaBuilder};
    pub use crate::serde_json;
    pub use crate::work::{CancellationToken, WorkHandle, WorkOutcome};
}
