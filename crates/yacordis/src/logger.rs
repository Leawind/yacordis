//! Leveled logging for hosts and components.
//!
//! Install [`LoggerComponent`] to provide the `logger` service; anything
//! that can resolve it gains leveled logging with pluggable sinks, and
//! component fibers get a natural scope (their component name) attached
//! to every record. Core itself only needs the [`Logger`] sink for
//! framework diagnostics such as failed component loads and disposer
//! errors.
//!
//! ```no_run
//! use yacordis::prelude::*;
//! use serde_json::json;
//!
//! let ctx = Context::new();
//! ctx.component(LoggerComponent, json!(null)).unwrap();
//!
//! struct Worker;
//! impl Component for Worker {
//!     fn name(&self) -> Option<&str> { Some("worker") }
//!     fn apply(&self, ctx: &Context, _: &serde_json::Value) -> yacordis::ComponentResult {
//!         ctx.log_info("started");
//!         Ok(())
//!     }
//! }
//! # let _ = ctx.component(Worker, json!(null)).unwrap();
//! ```

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::SystemTime;

use serde_json::Value;

use crate::component::Component;
use crate::context::Context;
use crate::error::{ComponentError, ComponentResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// Severity of a diagnostic message.
pub enum LogLevel {
    /// Verbose diagnostics.
    Debug,
    /// Normal operational messages.
    Info,
    /// Something looks wrong but execution continues.
    Warn,
    /// An operation failed.
    Error,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        };
        f.write_str(s)
    }
}

/// Where cordis writes its internal diagnostics.
pub trait Logger: Send + Sync + 'static {
    /// Write one diagnostic line.
    fn log(&self, level: LogLevel, target: &str, message: &str);
}

/// Default logger: writes to stderr, drops `Debug`.
#[derive(Debug, Default)]
pub struct StderrLogger;

impl Logger for StderrLogger {
    fn log(&self, level: LogLevel, target: &str, message: &str) {
        if level < LogLevel::Info {
            return;
        }
        eprintln!("[{level}] [{target}] {message}");
    }
}

/// A logger that discards everything (useful in tests).
#[derive(Debug, Default)]
pub struct DiscardLogger;

impl Logger for DiscardLogger {
    fn log(&self, _: LogLevel, _: &str, _: &str) {}
}

/// Shared handle onto the runtime's diagnostic sink.
///
/// Clones share one lock, so `set` on any clone is observed by all of
/// them — the sink is a property of the runtime, not of the handle.
#[derive(Clone)]
pub(crate) struct LoggerHandle(Arc<RwLock<Arc<dyn Logger>>>);

impl LoggerHandle {
    pub fn new(inner: Arc<dyn Logger>) -> Self {
        Self(Arc::new(RwLock::new(inner)))
    }

    pub fn set(&self, inner: Arc<dyn Logger>) {
        *self.0.write().unwrap() = inner;
    }

    pub fn log(&self, level: LogLevel, target: &str, message: &str) {
        let logger = self.0.read().unwrap().clone();
        logger.log(level, target, message);
    }
}

/// The name under which [`LoggerComponent`] provides [`LoggerService`].
pub const LOGGER_SERVICE: &str = "logger";

/// One log record handed to every sink.
#[derive(Debug, Clone)]
pub struct LogRecord {
    /// Severity.
    pub level: LogLevel,
    /// Emitting scope (usually the component's name).
    pub scope: String,
    /// The message.
    pub message: String,
    /// When the record was created.
    pub timestamp: SystemTime,
}

/// Where log records go. Sinks receive every record the service accepts
/// (level filtering already applied).
pub trait LogSink: Send + Sync + 'static {
    /// Write one record.
    fn write(&self, record: &LogRecord);
}

/// A sink writing human-readable lines to stderr.
#[derive(Debug, Default)]
pub struct StderrSink;

impl LogSink for StderrSink {
    fn write(&self, record: &LogRecord) {
        eprintln!("[{}] [{}] {}", record.level, record.scope, record.message);
    }
}

/// A sink collecting records into a shared in-memory buffer (handy in
/// tests and for console UIs).
#[derive(Debug, Default)]
pub struct MemorySink {
    records: RwLock<Vec<LogRecord>>,
}

impl LogSink for MemorySink {
    fn write(&self, record: &LogRecord) {
        self.records.write().unwrap().push(record.clone());
    }
}

impl MemorySink {
    /// A new empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of the records collected so far.
    #[must_use]
    pub fn records(&self) -> Vec<LogRecord> {
        self.records.read().unwrap().clone()
    }
}

struct SinkEntry {
    id: u64,
    sink: Arc<dyn LogSink>,
}

/// The logging service: level threshold + ordered sinks.
#[derive(Default)]
pub struct LoggerService {
    min_level: RwLock<Option<LogLevel>>,
    sinks: RwLock<Vec<SinkEntry>>,
    sn: AtomicU64,
}

impl LoggerService {
    /// A new service with no sinks and the default level (`Info`).
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Set the minimum level; `None` restores the default (`Info`).
    ///
    /// The change is a plain mutation of the shared service and is not
    /// reverted by any fiber unload — hosts use this directly; components
    /// use [`LoggerContextExt::set_scoped_level`], which restores the
    /// previous threshold when the calling fiber unloads.
    pub fn set_level(&self, level: Option<LogLevel>) {
        *self.min_level.write().unwrap() = level;
    }

    /// The current minimum level; `None` means the default (`Info`).
    #[must_use]
    pub fn level(&self) -> Option<LogLevel> {
        *self.min_level.read().unwrap()
    }

    /// Register a sink (on an `Arc`'d service); returns a handle that
    /// removes it when disposed.
    ///
    /// Registration is not tracked by any fiber scope: the sink stays
    /// registered until the handle is disposed or the service dies.
    /// Components use [`LoggerContextExt::add_scoped_sink`], which removes
    /// the sink automatically when the calling fiber unloads.
    pub fn add_sink(self: &Arc<Self>, sink: impl LogSink) -> SinkHandle {
        self.add_arc_sink(Arc::new(sink))
    }

    /// Register an already-shared sink.
    pub fn add_arc_sink(self: &Arc<Self>, sink: Arc<dyn LogSink>) -> SinkHandle {
        let id = self.sn.fetch_add(1, Ordering::Relaxed);
        self.sinks.write().unwrap().push(SinkEntry { id, sink });
        SinkHandle {
            service: Arc::downgrade(self),
            id,
        }
    }

    /// Emit a record if `level` passes the threshold.
    ///
    /// Sinks are snapshotted, then called without the registry lock:
    /// a sink that re-enters the service (`set_level`, `add_sink`) must
    /// not deadlock against the emission that is calling it.
    pub fn log(&self, level: LogLevel, scope: &str, message: &str) {
        let min = self.min_level.read().unwrap().unwrap_or(LogLevel::Info);
        if level < min {
            return;
        }
        let record = LogRecord {
            level,
            scope: scope.to_owned(),
            message: message.to_owned(),
            timestamp: SystemTime::now(),
        };
        let sinks: Vec<Arc<dyn LogSink>> = self
            .sinks
            .read()
            .unwrap()
            .iter()
            .map(|entry| entry.sink.clone())
            .collect();
        for sink in sinks {
            sink.write(&record);
        }
    }
}

/// Handle that removes a sink when disposed (idempotent).
#[derive(Clone)]
pub struct SinkHandle {
    service: Weak<LoggerService>,
    id: u64,
}

impl SinkHandle {
    /// Remove the sink. No-op when the service is already gone.
    pub fn remove(&self) {
        if let Some(service) = self.service.upgrade() {
            service
                .sinks
                .write()
                .unwrap()
                .retain(|entry| entry.id != self.id);
        }
    }
}

/// Logging helpers for any context that resolves the `logger` service.
pub trait LoggerContextExt {
    /// The logging service, when [`LoggerComponent`] is installed and active.
    fn logger(&self) -> Option<Arc<LoggerService>>;

    /// Emit a record scoped to the current fiber.
    fn log(&self, level: LogLevel, message: &str);

    /// Emit at `Error` level.
    fn log_error(&self, message: &str) {
        self.log(LogLevel::Error, message);
    }

    /// Emit at `Warn` level.
    fn log_warn(&self, message: &str) {
        self.log(LogLevel::Warn, message);
    }

    /// Emit at `Info` level.
    fn log_info(&self, message: &str) {
        self.log(LogLevel::Info, message);
    }

    /// Emit at `Debug` level (filtered by the default threshold).
    fn log_debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    /// Set the level threshold scoped to the current fiber: the previous
    /// threshold is restored when the fiber unloads (an effect, so the
    /// change composes with nesting and reverts in reverse order).
    fn set_scoped_level(&self, level: Option<LogLevel>) -> Result<(), String>;

    /// Register a sink scoped to the current fiber: removed automatically
    /// when the fiber unloads. The returned handle removes it early.
    fn add_scoped_sink(&self, sink: impl LogSink) -> Result<SinkHandle, String>;

    /// Register an already-shared sink scoped to the current fiber:
    /// removed automatically when the fiber unloads.
    fn add_scoped_arc_sink(&self, sink: Arc<dyn LogSink>) -> Result<SinkHandle, String>;
}

impl LoggerContextExt for Context {
    fn logger(&self) -> Option<Arc<LoggerService>> {
        self.get::<LoggerService>(LOGGER_SERVICE)
    }

    fn log(&self, level: LogLevel, message: &str) {
        let Some(service) = self.logger() else {
            return;
        };
        let scope = self.fiber().name().unwrap_or_else(|| "root".to_owned());
        service.log(level, &scope, message);
    }

    fn set_scoped_level(&self, level: Option<LogLevel>) -> Result<(), String> {
        let service = self
            .logger()
            .ok_or_else(|| "logger service not available".to_owned())?;
        let previous = service.level();
        // The restore effect is registered before the change is applied,
        // so a failing registration leaves the threshold untouched.
        let restore = service.clone();
        self.effect("logger/level", move |_| {
            let disposer: crate::Disposer = Box::new(move || restore.set_level(previous));
            Ok(Some(disposer))
        })
        .map_err(|e| e.to_string())?;
        service.set_level(level);
        Ok(())
    }

    fn add_scoped_sink(&self, sink: impl LogSink) -> Result<SinkHandle, String> {
        self.add_scoped_arc_sink(Arc::new(sink))
    }

    fn add_scoped_arc_sink(&self, sink: Arc<dyn LogSink>) -> Result<SinkHandle, String> {
        let service = self
            .logger()
            .ok_or_else(|| "logger service not available".to_owned())?;
        let handle = service.add_arc_sink(sink);
        // The removal is an effect of the calling fiber: unload reverts
        // the registration even when the handle itself was dropped.
        let remove = handle.clone();
        if let Err(error) = self.effect("logger/sink", move |_| {
            let remove = remove.clone();
            let disposer: crate::Disposer = Box::new(move || remove.remove());
            Ok(Some(disposer))
        }) {
            handle.remove();
            return Err(error.to_string());
        }
        Ok(handle)
    }
}

/// Provides the `logger` service with a default [`StderrSink`].
pub struct LoggerComponent;

impl Component for LoggerComponent {
    fn name(&self) -> Option<&str> {
        Some("logger")
    }

    fn apply(&self, ctx: &Context, _config: &Value) -> ComponentResult {
        let service = LoggerService::new();
        ctx.provide(LOGGER_SERVICE, service.clone())
            .map_err(ComponentError::new)?;
        service.add_sink(StderrSink);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Counts the records it receives.
    struct Counting(AtomicUsize);

    impl Logger for Counting {
        fn log(&self, _: LogLevel, _: &str, _: &str) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn clones_observe_a_sink_installed_through_another_handle() {
        let handle = LoggerHandle::new(Arc::new(Counting(AtomicUsize::new(0))));
        let clone = handle.clone();
        let replacement = Arc::new(Counting(AtomicUsize::new(0)));
        clone.set(replacement.clone());
        handle.log(LogLevel::Error, "target", "message");
        assert_eq!(
            replacement.0.load(Ordering::SeqCst),
            1,
            "clones share one sink: a set through one is seen by all"
        );
    }
}
