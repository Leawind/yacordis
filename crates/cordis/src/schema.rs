//! Config schemas: the Standard-Schema-equivalent validation layer.
//!
//! A [`Schema`] describes the config a component accepts, validates a
//! [`Value`](serde_json::Value) against it, and — like v4's
//! `resolveConfig` — resolves the config by filling declared defaults.
//! Validation runs before the component body on install and update; a
//! rejected config leaves the fiber with no side effects (`Failed` /
//! update error).
//!
//! Unknown object fields are reported by default (typo protection);
//! declare [`SchemaBuilder::allow_unknown`] to opt out.

use std::sync::Arc;

use serde_json::Value;

/// One validation failure: where (a dot path into the config) and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Dot path to the offending value; `""` means the config root.
    pub path: String,
    /// What is wrong, in human terms.
    pub message: String,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "at `{}`: {}", self.path, self.message)
        }
    }
}

#[derive(Clone, Default)]
struct StringChecks {
    min_len: Option<usize>,
    max_len: Option<usize>,
}

#[derive(Clone, Default)]
struct NumberChecks {
    min: Option<f64>,
    max: Option<f64>,
}

#[derive(Clone)]
enum Node {
    Any,
    Bool,
    String(StringChecks),
    Number(NumberChecks),
    Integer(NumberChecks),
    Enum(Vec<Value>),
    Array {
        item: Schema,
        min: Option<usize>,
        max: Option<usize>,
    },
    Object {
        fields: Vec<Field>,
        allow_unknown: bool,
    },
}

#[derive(Clone)]
struct Field {
    name: String,
    schema: Schema,
    required: bool,
    default: Option<Value>,
}

/// A reusable config schema. Cheap to clone; build with
/// [`Schema::object`] and friends.
#[derive(Clone)]
pub struct Schema {
    node: Arc<Node>,
}

impl Schema {
    /// Accepts any config (the default for components without a schema).
    pub fn any() -> Self {
        Self::of(Node::Any)
    }

    /// Requires a boolean.
    pub fn bool_() -> Self {
        Self::of(Node::Bool)
    }

    /// Requires a string, optionally length-bounded (in characters).
    pub fn string() -> Schema {
        Self::of(Node::String(StringChecks::default()))
    }

    /// Requires a number (integers pass), optionally range-bounded.
    pub fn number() -> Schema {
        Self::of(Node::Number(NumberChecks::default()))
    }

    /// Requires an integer-valued number, optionally range-bounded.
    pub fn integer() -> Schema {
        Self::of(Node::Integer(NumberChecks::default()))
    }

    /// Requires one of the given literal values.
    pub fn enum_of(values: impl IntoIterator<Item = Value>) -> Schema {
        Self::of(Node::Enum(values.into_iter().collect()))
    }

    /// Requires an array whose items match `item`.
    pub fn array(item: Schema) -> Schema {
        Self::of(Node::Array {
            item,
            min: None,
            max: None,
        })
    }

    /// Requires an object; add fields with [`SchemaBuilder::req`] and
    /// friends.
    pub fn object() -> SchemaBuilder {
        SchemaBuilder {
            fields: Vec::new(),
            allow_unknown: false,
        }
    }

    fn of(node: Node) -> Self {
        Self {
            node: Arc::new(node),
        }
    }

    /// Identity of the underlying schema node, for ownership checks
    /// when unregistrations race re-registrations.
    pub(crate) fn node_ptr(&self) -> usize {
        Arc::as_ptr(&self.node) as *const () as usize
    }

    /// Minimum item count (arrays) or length (strings).
    #[must_use]
    pub fn min(mut self, bound: usize) -> Self {
        match Arc::make_mut(&mut self.node) {
            Node::Array { min, .. } => *min = Some(bound),
            Node::String(checks) => checks.min_len = Some(bound),
            _ => {}
        }
        self
    }

    /// Maximum item count (arrays) or length (strings).
    #[must_use]
    pub fn max(mut self, bound: usize) -> Self {
        match Arc::make_mut(&mut self.node) {
            Node::Array { max, .. } => *max = Some(bound),
            Node::String(checks) => checks.max_len = Some(bound),
            _ => {}
        }
        self
    }

    /// Inclusive lower bound (numbers / integers).
    #[must_use]
    pub fn at_least(mut self, bound: f64) -> Self {
        if let Node::Number(checks) | Node::Integer(checks) = Arc::make_mut(&mut self.node) {
            checks.min = Some(bound);
        }
        self
    }

    /// Inclusive upper bound (numbers / integers).
    #[must_use]
    pub fn at_most(mut self, bound: f64) -> Self {
        if let Node::Number(checks) | Node::Integer(checks) = Arc::make_mut(&mut self.node) {
            checks.max = Some(bound);
        }
        self
    }

    /// Validate `config` against this schema.
    pub fn validate(&self, config: &Value) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        check(self, config, "", &mut out);
        out
    }

    /// Validate and resolve: defaults for absent optional fields are
    /// filled in, and the resolved value is returned.
    pub fn resolve(&self, config: &Value) -> Result<Value, Vec<Diagnostic>> {
        let diagnostics = self.validate(config);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        let mut resolved = config.clone();
        apply_defaults(self, &mut resolved, "");
        Ok(resolved)
    }
}

/// Builder for an object schema.
pub struct SchemaBuilder {
    fields: Vec<Field>,
    allow_unknown: bool,
}

impl SchemaBuilder {
    /// Declare a required field: absent configs are rejected.
    #[must_use]
    pub fn req(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.fields.push(Field {
            name: name.into(),
            schema,
            required: true,
            default: None,
        });
        self
    }

    /// Declare an optional field: absent configs pass, no default.
    #[must_use]
    pub fn opt(mut self, name: impl Into<String>, schema: Schema) -> Self {
        self.fields.push(Field {
            name: name.into(),
            schema,
            required: false,
            default: None,
        });
        self
    }

    /// Declare an optional field with a default filled in by
    /// [`Schema::resolve`].
    #[must_use]
    pub fn opt_default(mut self, name: impl Into<String>, schema: Schema, default: Value) -> Self {
        self.fields.push(Field {
            name: name.into(),
            schema,
            required: false,
            default: Some(default),
        });
        self
    }

    /// Stop reporting unknown fields (typo protection off).
    #[must_use]
    pub fn allow_unknown(mut self) -> Self {
        self.allow_unknown = true;
        self
    }

    /// Finish the object schema.
    pub fn build(self) -> Schema {
        Schema::of(Node::Object {
            fields: self.fields,
            allow_unknown: self.allow_unknown,
        })
    }
}

fn check(schema: &Schema, value: &Value, path: &str, out: &mut Vec<Diagnostic>) {
    let report = |out: &mut Vec<Diagnostic>, message: String| {
        out.push(Diagnostic {
            path: path.to_owned(),
            message,
        });
    };
    match (schema.node.as_ref(), value) {
        (Node::Any, _) => {}
        (Node::Bool, Value::Bool(_)) => {}
        (Node::Bool, other) => report(
            out,
            format!("expected a boolean, found {kind}", kind = kind(other)),
        ),
        (Node::String(checks), Value::String(text)) => {
            let len = text.chars().count();
            if let Some(min) = checks.min_len {
                if len < min {
                    report(
                        out,
                        format!("expected at least {min} characters, found {len}"),
                    );
                }
            }
            if let Some(max) = checks.max_len {
                if len > max {
                    report(
                        out,
                        format!("expected at most {max} characters, found {len}"),
                    );
                }
            }
        }
        (Node::String(_), other) => report(
            out,
            format!("expected a string, found {kind}", kind = kind(other)),
        ),
        (Node::Number(checks), Value::Number(n)) => check_range(checks, n.as_f64(), path, out),
        (Node::Number(_), other) => report(
            out,
            format!("expected a number, found {kind}", kind = kind(other)),
        ),
        (Node::Integer(checks), Value::Number(n)) => {
            if n.as_f64().map(|f| f.fract() != 0.0).unwrap_or(true) {
                report(out, format!("expected an integer, found {n}"));
            } else {
                check_range(checks, n.as_f64(), path, out);
            }
        }
        (Node::Integer(_), other) => report(
            out,
            format!("expected an integer, found {kind}", kind = kind(other)),
        ),
        (Node::Enum(values), other) => {
            if !values.iter().any(|v| v == other) {
                report(
                    out,
                    format!("expected one of [{}], found {other}", listed(values)),
                );
            }
        }
        (Node::Array { item, min, max }, Value::Array(items)) => {
            if let Some(min) = min {
                if items.len() < *min {
                    report(
                        out,
                        format!("expected at least {min} items, found {}", items.len()),
                    );
                }
            }
            if let Some(max) = max {
                if items.len() > *max {
                    report(
                        out,
                        format!("expected at most {max} items, found {}", items.len()),
                    );
                }
            }
            for (index, element) in items.iter().enumerate() {
                check(item, element, &join(path, &index.to_string()), out);
            }
        }
        (Node::Array { .. }, other) => report(
            out,
            format!("expected an array, found {kind}", kind = kind(other)),
        ),
        (
            Node::Object {
                fields,
                allow_unknown,
            },
            Value::Object(map),
        ) => {
            for field in fields {
                match map.get(&field.name) {
                    Some(Value::Null) | None => {
                        if field.required {
                            report(out, format!("missing required field `{}`", field.name));
                        }
                    }
                    Some(present) => {
                        check(&field.schema, present, &join(path, &field.name), out);
                    }
                }
            }
            if !allow_unknown {
                for key in map.keys() {
                    if !fields.iter().any(|field| &field.name == key) {
                        report(
                            out,
                            format!(
                                "unknown field `{key}`; expected one of [{}]",
                                listed(
                                    &fields
                                        .iter()
                                        .map(|f| Value::String(f.name.clone()))
                                        .collect::<Vec<_>>()
                                )
                            ),
                        );
                    }
                }
            }
        }
        (Node::Object { .. }, other) => report(
            out,
            format!("expected an object, found {kind}", kind = kind(other)),
        ),
    }
}

fn check_range(checks: &NumberChecks, value: Option<f64>, path: &str, out: &mut Vec<Diagnostic>) {
    if let Some(value) = value {
        if let Some(min) = checks.min {
            if value < min {
                out.push(Diagnostic {
                    path: path.to_owned(),
                    message: format!("expected at least {min}, found {value}"),
                });
            }
        }
        if let Some(max) = checks.max {
            if value > max {
                out.push(Diagnostic {
                    path: path.to_owned(),
                    message: format!("expected at most {max}, found {value}"),
                });
            }
        }
    }
}

/// Fill declared defaults for absent optional object fields, in place.
fn apply_defaults(schema: &Schema, value: &mut Value, _path: &str) {
    if let (Node::Object { fields, .. }, Value::Object(map)) = (schema.node.as_ref(), value) {
        for field in fields {
            if let Some(default) = &field.default {
                match map.get(&field.name) {
                    None | Some(Value::Null) => {
                        map.insert(field.name.clone(), default.clone());
                    }
                    Some(_) => {}
                }
            }
        }
    }
}

fn join(path: &str, leaf: &str) -> String {
    if path.is_empty() {
        leaf.to_owned()
    } else {
        format!("{path}.{leaf}")
    }
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

fn listed(values: &[Value]) -> String {
    values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}
