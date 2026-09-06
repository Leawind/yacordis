//! Minimal pluggable logging.
//!
//! The full logger (levels, scopes, exporters) is planned as a separate
//! crate; core only needs a sink for framework diagnostics such as failed
//! plugin loads and disposer errors.

use std::fmt;
use std::sync::{Arc, RwLock};

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

pub(crate) struct LoggerHandle(RwLock<Arc<dyn Logger>>);

impl Clone for LoggerHandle {
    fn clone(&self) -> Self {
        Self(RwLock::new(self.0.read().unwrap().clone()))
    }
}

impl LoggerHandle {
    pub fn new(inner: Arc<dyn Logger>) -> Self {
        Self(RwLock::new(inner))
    }

    pub fn set(&self, inner: Arc<dyn Logger>) {
        *self.0.write().unwrap() = inner;
    }

    pub fn log(&self, level: LogLevel, target: &str, message: &str) {
        let logger = self.0.read().unwrap().clone();
        logger.log(level, target, message);
    }

    /// Log an error, flattening nested error sources into lines.
    #[allow(dead_code)] // exercised by tests and future modules
    pub fn log_error(&self, target: &str, error: &(dyn std::error::Error + 'static)) {
        let mut message = error.to_string();
        let mut source = error.source();
        while let Some(e) = source {
            message.push_str("\n  caused by: ");
            message.push_str(&e.to_string());
            source = e.source();
        }
        self.log(LogLevel::Error, target, &message);
    }
}
