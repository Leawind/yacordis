//! Components: the unit of dynamic composition.
//!
//! A component is installed with [`Context::component`](crate::Context::component),
//! receiving a scoped [`Context`](crate::Context) and its config.
//! Everything the body registers — listeners, services, child components,
//! raw effects — is reverted when the component is disposed. The same
//! `Component` shape is used for native (statically linked) components and for
//! dynamically loaded cdylib components (the loader adapts the latter into
//! this trait).

use serde_json::Value;

use crate::context::Context;
use crate::error::ComponentResult;

/// Declared coeffects: which services this component requires to activate.
///
/// The component body does not run until every `required` name is provided
/// by an active fiber; `optional` names never block activation and are
/// resolved dynamically through `ctx.get`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inject {
    /// (name, required) in declaration order; required order defines the
    /// epoch.
    pub(crate) entries: Vec<(String, bool)>,
}

impl Inject {
    /// No dependencies.
    pub fn none() -> Self {
        Self::default()
    }

    /// All of `names` are required (they gate activation).
    pub fn required(names: &[&str]) -> Self {
        Self {
            entries: names.iter().map(|n| ((*n).to_owned(), true)).collect(),
        }
    }

    /// All of `names` are optional (they never gate activation).
    pub fn optional(names: &[&str]) -> Self {
        Self {
            entries: names.iter().map(|n| ((*n).to_owned(), false)).collect(),
        }
    }

    /// Mixed declaration: `Inject::new().req("database").opt("assets")`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a required dependency.
    pub fn req(mut self, name: impl Into<String>) -> Self {
        self.entries.push((name.into(), true));
        self
    }

    /// Add an optional dependency.
    pub fn opt(mut self, name: impl Into<String>) -> Self {
        self.entries.push((name.into(), false));
        self
    }

    /// Whether nothing is declared.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterate `(name, required)` pairs in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, bool)> {
        self.entries.iter().map(|(n, r)| (n.as_str(), *r))
    }

    pub(crate) fn required_names(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .filter(|(_, r)| *r)
            .map(|(n, _)| n.as_str())
    }
}

/// The unified component form. Both closures and dynamically loaded components
/// are normalized into `Arc<dyn Component>`.
pub trait Component: Send + Sync + 'static {
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

    /// Services this component declares it will provide once active.
    ///
    /// Purely declarative — the body is what actually provides. The
    /// runtime uses the declaration for dependency-cycle diagnostics:
    /// pending fibers whose required injects and declared provides close
    /// a loop can never activate, and are reported (log + introspection).
    /// Declare honestly: a declaration the
    /// body never fulfills can produce a false cycle report.
    fn provides(&self) -> Vec<String> {
        Vec::new()
    }

    /// Config schema. When declared, every install and update validates
    /// and resolves the config through it before the body runs; resolving
    /// fills in declared defaults. A rejected config leaves the fiber
    /// `Failed` with no side effects, and `update` refuses it with an
    /// error while keeping the previous config. See [`crate::Schema`].
    fn schema(&self) -> Option<crate::schema::Schema> {
        None
    }

    /// The component body. Called with a context scoped to this fiber and
    /// the component config. Registrations made on the context during the
    /// call are tracked automatically and reverted on dispose.
    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult;
}

impl<F> Component for F
where
    F: Fn(&Context, &Value) -> ComponentResult + Send + Sync + 'static,
{
    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
        self(ctx, config)
    }
}

/// A component that only depends on services:
/// `ctx.inject(&["database"], |ctx| { … })`.
pub(crate) struct InjectClosure<F> {
    pub(crate) inject: Inject,
    pub(crate) f: F,
}

impl<F> Component for InjectClosure<F>
where
    F: Fn(&Context) -> ComponentResult + Send + Sync + 'static,
{
    fn inject(&self) -> Inject {
        self.inject.clone()
    }

    fn apply(&self, ctx: &Context, _config: &Value) -> ComponentResult {
        (self.f)(ctx)
    }
}
