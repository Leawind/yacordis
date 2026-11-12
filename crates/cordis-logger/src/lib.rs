//! Leveled logging service for cordis hosts and components.
//!
//! Install [`LoggerComponent`] to provide the `logger` service; anything
//! that can resolve it gains leveled logging with pluggable sinks, and
//! component fibers get a natural scope (their component name) attached to
//! every record.
//!
//! ```no_run
//! use cordis::prelude::*;
//! use cordis_logger::{LoggerContextExt, LoggerComponent};
//! use serde_json::json;
//!
//! let ctx = Context::new();
//! ctx.component(LoggerComponent, json!(null)).unwrap();
//!
//! struct Worker;
//! impl Component for Worker {
//!     fn name(&self) -> Option<&str> { Some("worker") }
//!     fn apply(&self, ctx: &Context, _: &serde_json::Value) -> cordis::ComponentResult {
//!         ctx.log_info("started");
//!         Ok(())
//!     }
//! }
//! # let _ = ctx.component(Worker, json!(null)).unwrap();
//! ```

#![deny(missing_docs)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, Weak};
use std::time::SystemTime;

use cordis::{Component, ComponentError, ComponentResult, Context};
use serde_json::Value;

/// The name under which [`LoggerComponent`] provides [`LoggerService`].
pub const LOGGER_SERVICE: &str = "logger";

/// Severity of a record, ordered from most to least severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Failures.
    Error = 0,
    /// Something looks wrong.
    Warn = 1,
    /// Normal operational messages.
    Info = 2,
    /// Verbose diagnostics (filtered out by default).
    Debug = 3,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }
}

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
        eprintln!(
            "[{}] [{}] {}",
            record.level.as_str(),
            record.scope,
            record.message
        );
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
    pub fn set_level(&self, level: Option<LogLevel>) {
        *self.min_level.write().unwrap() = level;
    }

    /// Register a sink (on an `Arc`'d service); returns a handle that
    /// removes it when disposed.
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
    pub fn log(&self, level: LogLevel, scope: &str, message: &str) {
        let threshold = *self.min_level.read().unwrap();
        let min = threshold.unwrap_or(LogLevel::Info);
        if level > min {
            return;
        }
        let record = LogRecord {
            level,
            scope: scope.to_owned(),
            message: message.to_owned(),
            timestamp: SystemTime::now(),
        };
        let sinks = self.sinks.read().unwrap();
        for entry in sinks.iter() {
            entry.sink.write(&record);
        }
    }
}

/// Handle that removes a sink when disposed (idempotent).
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
