//! Core error types.

use std::fmt;

/// The error type produced by cordis core operations.
#[derive(Debug)]
#[allow(missing_docs)]
pub enum CordisError {
    /// The operation requires an active (installed) fiber, but the current
    /// context's fiber is disposed or being unloaded.
    InactiveContext {
        /// What the caller was trying to do, for the error message.
        action: &'static str,
        /// The affected fiber's name, when known.
        fiber: Option<String>,
    },
    /// A service with this name has already been provided by another fiber.
    ServiceConflict { name: String, owner: String },
    /// Setting a service value requires a prior `provide` on the same fiber.
    NotProvided { name: String },
    /// Setting a service value is only allowed from the fiber that provided it.
    ForeignService { name: String, owner: String },
    /// The value passed to `plugin()` is not a usable plugin.
    InvalidPlugin(&'static str),
    /// A plugin's `apply` failed; the error is stored on the fiber until
    /// `update()` clears it.
    PluginError(PluginError),
}

impl fmt::Display for CordisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CordisError::InactiveContext { action, fiber } => {
                write!(f, "cannot {action} on inactive context")?;
                if let Some(name) = fiber {
                    write!(f, " (fiber `{name}` is disposed)")?;
                }
                Ok(())
            }
            CordisError::ServiceConflict { name, owner } => {
                write!(f, "service `{name}` has been registered at <{owner}>")
            }
            CordisError::NotProvided { name } => {
                write!(f, "cannot set property `{name}` without provide")
            }
            CordisError::ForeignService { name, owner } => {
                write!(
                    f,
                    "cannot set property `{name}` provided by <{owner}> from another fiber"
                )
            }
            CordisError::InvalidPlugin(why) => write!(f, "invalid plugin: {why}"),
            CordisError::PluginError(e) => write!(f, "plugin error: {}", e.0),
        }
    }
}

impl std::error::Error for CordisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CordisError::PluginError(e) => Some(&*e.0),
            _ => None,
        }
    }
}

/// Errors thrown out of plugin code (`apply`, effect setup, …).
///
/// Cordis never lets a plugin panic take down the host: panics crossing
/// public boundaries are caught and converted into this type, which puts
/// the fiber into [`FiberState::Failed`](crate::FiberState::Failed).
#[derive(Debug)]
pub struct PluginError(pub Box<dyn std::error::Error + Send + Sync>);

impl PluginError {
    /// Wrap any error into a [`PluginError`].
    pub fn new(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(e.into())
    }
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Note: deliberately does **not** implement `std::error::Error`, so that
/// the blanket `From<E: Error>` keeps `?` working inside plugin bodies
/// without conflicting with the reflexive `From<T> for T`.
impl<E: std::error::Error + Send + Sync + 'static> From<E> for PluginError {
    fn from(e: E) -> Self {
        PluginError(Box::new(e))
    }
}

/// The result type of a plugin body.
pub type PluginResult = Result<(), PluginError>;
