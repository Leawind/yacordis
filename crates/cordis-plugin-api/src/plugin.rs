//! The plugin contract, mirroring the native `cordis::Plugin` API.

use serde_json::Value;

use crate::context::Context;

/// Declared service dependencies (reactive coeffects).
///
/// The plugin body does not run until every `required` name is provided
/// by an active fiber; `optional` names never block activation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inject {
    required: Vec<String>,
    optional: Vec<String>,
}

impl Inject {
    /// No dependencies.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// All of `names` are required (they gate activation).
    #[must_use]
    pub fn required(names: &[&str]) -> Self {
        Self {
            required: names.iter().map(ToString::to_string).collect(),
            optional: Vec::new(),
        }
    }

    /// All of `names` are optional (they never gate activation).
    #[must_use]
    pub fn optional(names: &[&str]) -> Self {
        Self {
            required: Vec::new(),
            optional: names.iter().map(ToString::to_string).collect(),
        }
    }

    /// Mixed declaration: `Inject::new().req("database").opt("assets")`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a required dependency.
    #[must_use]
    pub fn req(mut self, name: impl Into<String>) -> Self {
        self.required.push(name.into());
        self
    }

    /// Add an optional dependency.
    #[must_use]
    pub fn opt(mut self, name: impl Into<String>) -> Self {
        self.optional.push(name.into());
        self
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.optional.is_empty()
    }

    pub(crate) fn required_names(&self) -> impl Iterator<Item = &str> {
        self.required.iter().map(String::as_str)
    }

    pub(crate) fn optional_names(&self) -> impl Iterator<Item = &str> {
        self.optional.iter().map(String::as_str)
    }
}

/// Errors thrown out of plugin code. Deliberately does **not**
/// implement `std::error::Error` so the blanket `From<E: Error>` keeps
/// `?` working in plugin bodies.
#[derive(Debug)]
pub struct PluginError(pub Box<dyn std::error::Error + Send + Sync>);

impl PluginError {
    /// Wrap any error.
    pub fn new(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self(error.into())
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<E: std::error::Error + Send + Sync + 'static> From<E> for PluginError {
    fn from(error: E) -> Self {
        Self(Box::new(error))
    }
}

/// The result type of a plugin body.
pub type PluginResult = Result<(), PluginError>;

/// The plugin contract. The same shape as the native `cordis::Plugin`;
/// the loader adapts instances into the host's erased form.
pub trait Plugin: Send + Sync + 'static {
    /// Human-readable name used in logs and error messages.
    fn name(&self) -> Option<&str> {
        None
    }

    /// Declared service dependencies. Required dependencies gate
    /// activation: the body stays unexecuted until all of them are
    /// provided by active fibers.
    fn inject(&self) -> Inject {
        Inject::none()
    }

    /// The plugin body. Registrations made on the context during the
    /// call are tracked automatically and reverted on dispose.
    fn apply(&self, ctx: &Context, config: &Value) -> PluginResult;
}

impl<F> Plugin for F
where
    F: Fn(&Context, &Value) -> PluginResult + Send + Sync + 'static,
{
    fn apply(&self, ctx: &Context, config: &Value) -> PluginResult {
        self(ctx, config)
    }
}
