use std::sync::Arc;

use serde_json::{Value, json};
use yacordis::prelude::*;

#[component(
    name = "database",
    inject(required = ["config"], optional = ["logger"]),
    provides = ["db"]
)]
fn database_component(ctx: &Context, _config: &Value) -> ComponentResult {
    ctx.provide("db", Arc::new(json!({ "connected": true })))?;
    Ok(())
}

#[test]
fn component_macro_declarations_gate_and_provide() {
    let ctx = Context::new();

    let handle = ctx
        .component(DatabaseComponent, json!(null))
        .expect("install");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(!ctx.has("db"));

    ctx.provide("config", Arc::new(json!({ "uri": "pg://x" })))
        .expect("provide config");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(
        ctx.get::<Value>("db").map(|v| v.as_ref().clone()),
        Some(json!({ "connected": true }))
    );

    let summaries = ctx.installed_components();
    let summary = summaries
        .iter()
        .find(|s| s.name.as_deref() == Some("database"))
        .expect("component summary");
    assert!(!summary.fibers.is_empty());
}

#[test]
fn component_macro_defaults_name_and_generates_pascal_struct() {
    let ctx = Context::new();

    #[component]
    fn echo_tube(ctx: &Context, _config: &Value) -> ComponentResult {
        ctx.provide("echo", Arc::new(json!("on")))?;
        Ok(())
    }

    let handle = ctx.component(EchoTube, json!(null)).expect("install");
    assert!(handle.is_active());
    let summaries = ctx.installed_components();
    assert!(
        summaries
            .iter()
            .any(|s| s.name.as_deref() == Some("echo_tube")),
        "default name is the function name: {:?}",
        summaries.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn component_macro_provides_feed_cycle_diagnostics() {
    let ctx = Context::new();

    #[component(inject(required = ["a"]), provides = ["b"])]
    fn needs_a(ctx: &Context, _config: &Value) -> ComponentResult {
        let _ = ctx;
        Ok(())
    }
    #[component(inject(required = ["b"]), provides = ["a"])]
    fn needs_b(ctx: &Context, _config: &Value) -> ComponentResult {
        let _ = ctx;
        Ok(())
    }

    ctx.component(NeedsA, json!(null)).expect("install a");
    ctx.component(NeedsB, json!(null)).expect("install b");

    let cycles = ctx.dependency_cycles();
    assert_eq!(cycles.len(), 1, "one stuck group: {cycles:?}");
    assert_eq!(cycles[0].len(), 2, "both members reported");
}

/// Locks the macro → `Component::schema` wiring (§6 item 4).
#[test]
fn component_macro_schema_gates_install_and_update() {
    let ctx = Context::new();

    #[component(
        name = "validated",
        schema = Schema::object()
            .req("uri", Schema::string())
            .opt_default("port", Schema::integer().at_least(1.0).at_most(65535.0), json!(5432))
            .build()
    )]
    fn validated_component(ctx: &Context, config: &Value) -> ComponentResult {
        ctx.provide("db", Arc::new(config.clone()))?;
        Ok(())
    }

    let handle = ctx
        .component(ValidatedComponent, json!({ "uri": 42 }))
        .expect("install still returns a handle");
    assert_eq!(handle.state(), FiberState::Failed);
    assert!(
        handle.error().unwrap().contains("expected a string"),
        "{}",
        handle.error().unwrap()
    );
    assert!(!ctx.has("db"), "zero side effects: the body never ran");

    let handle = ctx
        .component(ValidatedComponent, json!({ "uri": "pg://x" }))
        .expect("install ok");
    assert!(handle.is_active());
    assert_eq!(
        ctx.get::<Value>("db").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://x", "port": 5432 }))
    );

    handle
        .update(json!({ "uri": "pg://y", "port": 0 }))
        .expect_err("update rejected by the declared schema");
    assert_eq!(
        ctx.get::<Value>("db").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://x", "port": 5432 }))
    );

    handle
        .update(json!({ "uri": "pg://z" }))
        .expect("valid update");
    assert_eq!(
        ctx.get::<Value>("db").map(|v| v.as_ref().clone()),
        Some(json!({ "uri": "pg://z", "port": 5432 }))
    );
}
