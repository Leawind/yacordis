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
//! // reactivates it. Both without touching the process.
//! db.dispose();
//! assert_eq!(app.state(), FiberState::Pending);
//! # Ok(())
//! # }
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
mod work;

pub use context::{Context, FiberHandle};
pub use cordis_macros::plugin;
pub use effect::{Disposer, EffectHandle, EffectOutcome};
pub use error::{CordisError, PluginError, PluginResult};
pub use events::Event;
pub use fiber::FiberState;
pub use logger::{DiscardLogger, LogLevel, Logger, StderrLogger};
pub use plugin::{Inject, Plugin};
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
    pub use crate::serde_json;
    pub use crate::work::{CancellationToken, WorkHandle, WorkOutcome};
}
