//! Config schemas: validation + default resolution (the
//! `resolveConfig` equivalent). Locked behaviors: install rejects an
//! invalid config with a `Failed` fiber and no side effects, update
//! refuses an invalid config while keeping the previous one, resolved
//! defaults are visible to the body and persist across restarts.

use std::sync::Arc;

use yacordis::prelude::*;
use serde_json::{Value, json};

fn server_schema() -> Schema {
    Schema::object()
        .req("uri", Schema::string().min(4))
        .opt_default(
            "port",
            Schema::integer().at_least(1.0).at_most(65535.0),
            json!(8080),
        )
        .opt("tags", Schema::array(Schema::string()).min(1))
        .build()
}

/// Provides the resolved config it saw, so tests can observe what the
/// body actually received.
fn server_component(ctx: &Context, config: &Value) -> ComponentResult {
    ctx.provide("seen", Arc::new(config.clone()))?;
    Ok(())
}

#[test]
fn schema_rejects_invalid_config_at_install_without_side_effects() {
    let ctx = Context::new();
    let handle = ctx
        .component(WithSchema(server_schema()), json!({ "uri": "" }))
        .expect("install");
    // Blank uri fails min(4); the fiber is Failed and provided nothing.
    assert_eq!(handle.state(), FiberState::Failed);
    let error = handle.error().expect("diagnostic error");
    assert!(error.contains("at least 4 characters"), "{error}");
    assert!(!ctx.has("seen"));
}

#[test]
fn schema_resolves_defaults_for_the_body_and_across_restarts() {
    let ctx = Context::new();
    let handle = ctx
        .component(WithSchema(server_schema()), json!({ "uri": "pg://host" }))
        .expect("install");
    assert!(handle.is_active());
    // The body saw the default port filled in.
    assert_eq!(
        ctx.get::<Value>("seen").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://host", "port": 8080 }))
    );

    // Restarts persist the resolved config (not the raw one).
    handle.restart().expect("restart");
    assert_eq!(
        ctx.get::<Value>("seen").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://host", "port": 8080 }))
    );
}

#[test]
fn schema_rejects_invalid_update_and_keeps_previous_config() {
    let ctx = Context::new();
    let handle = ctx
        .component(WithSchema(server_schema()), json!({ "uri": "pg://host" }))
        .expect("install");
    assert!(handle.is_active());

    // Invalid update: unknown field + out-of-range port.
    let err = handle
        .update(json!({ "uri": "pg://other", "port": 0, "typo": true }))
        .expect_err("invalid update");
    assert!(err.to_string().contains("port"), "{err}");
    assert!(err.to_string().contains("typo"), "{err}");

    // The fiber kept running on the previous (resolved) config.
    assert!(handle.is_active());
    assert_eq!(
        ctx.get::<Value>("seen").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://host", "port": 8080 }))
    );

    // A valid update goes through.
    handle
        .update(json!({ "uri": "pg://next", "port": 9090 }))
        .expect("valid update");
    assert_eq!(
        ctx.get::<Value>("seen").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://next", "port": 9090 }))
    );
}

#[test]
fn schema_reports_unknown_fields_and_type_mismatches() {
    let schema = Schema::object()
        .req("mode", Schema::enum_of([json!("fast"), json!("safe")]))
        .opt("retries", Schema::integer().at_least(0.0))
        .build();
    let diagnostics = schema.validate(&json!({ "mode": "turbo", "retries": -1, "extra": 1 }));
    let rendered: Vec<String> = diagnostics.iter().map(|d| d.to_string()).collect();
    assert!(
        rendered
            .iter()
            .any(|d| d.contains("mode") && d.contains("one of")),
        "{rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|d| d.contains("retries") && d.contains("at least 0")),
        "{rendered:?}"
    );
    assert!(
        rendered.iter().any(|d| d.contains("unknown field `extra`")),
        "{rendered:?}"
    );

    // allow_unknown silences typo reports but keeps real violations.
    let lenient = Schema::object()
        .req("mode", Schema::enum_of([json!("fast"), json!("safe")]))
        .allow_unknown()
        .build();
    assert!(
        lenient
            .validate(&json!({ "mode": "fast", "extra": 1 }))
            .is_empty()
    );

    // Root-level type mismatch (config root is not an object).
    let diagnostics = lenient.validate(&json!("not an object"));
    assert!(diagnostics[0].message.contains("expected an object"));
}

#[test]
fn components_without_schema_are_unaffected() {
    let ctx = Context::new();
    let handle = ctx
        .component(|_: &Context, _: &Value| Ok(()), json!({ "anything": true }))
        .expect("install");
    assert!(handle.is_active());
}

/// Wrapper pairing a schema with a plain body function (closures have
/// no schema support of their own).
struct WithSchema(Schema);

impl yacordis::Component for WithSchema {
    fn name(&self) -> Option<&str> {
        Some("schema-server")
    }
    fn schema(&self) -> Option<Schema> {
        Some(self.0.clone())
    }
    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
        server_component(ctx, config)
    }
}
