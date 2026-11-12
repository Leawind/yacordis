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
    /// The value passed to `component()` is not a usable component.
    InvalidComponent(&'static str),
    /// A component's `apply` failed; the error is stored on the fiber until
    /// `update()` clears it.
    ComponentError(ComponentError),
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
            CordisError::InvalidComponent(why) => write!(f, "invalid component: {why}"),
            CordisError::ComponentError(e) => write!(f, "component error: {}", e.0),
        }
    }
}

impl std::error::Error for CordisError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CordisError::ComponentError(e) => Some(&*e.0),
            _ => None,
        }
    }
}

/// Errors thrown out of component code (`apply`, effect setup, …).
///
/// Cordis never lets a component panic take down the host: panics crossing
/// public boundaries are caught and converted into this type, which puts
/// the fiber into [`FiberState::Failed`](crate::FiberState::Failed).
#[derive(Debug)]
pub struct ComponentError(pub Box<dyn std::error::Error + Send + Sync>);

impl ComponentError {
    /// Wrap any error into a [`ComponentError`].
    pub fn new(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(e.into())
    }
}

impl fmt::Display for ComponentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Note: deliberately does **not** implement `std::error::Error`, so that
/// the blanket `From<E: Error>` keeps `?` working inside component bodies
/// alongside the reflexive `From<T> for T`.
impl<E: std::error::Error + Send + Sync + 'static> From<E> for ComponentError {
    fn from(e: E) -> Self {
        ComponentError(Box::new(e))
    }
}

/// The result type of a component body.
pub type ComponentResult = Result<(), ComponentError>;
