//! `#[plugin]` attribute macro: declarations flow into the runtime
//! (gating via inject, cycle diagnostics via provides), apply delegates
//! to the plain function, and the generated adapter keeps fiber
//! semantics identical to hand-written plugins.

use std::sync::Arc;

use cordis::prelude::*;
use serde_json::{Value, json};

#[plugin(
    name = "database",
    inject(required = ["config"], optional = ["logger"]),
    provides = ["db"]
)]
fn database_plugin(ctx: &Context, config: &Value) -> PluginResult {
    ctx.provide("db", Arc::new(json!({ "connected": true })))?;
    Ok(())
}

#[test]
fn plugin_macro_declarations_gate_and_provide() {
    let ctx = Context::new();

    // Missing required `config`: the plugin pends, body unexecuted.
    let handle = ctx.plugin(DatabasePlugin, json!(null)).expect("install");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(!ctx.has("db"));

    // Satisfy the requirement: reactive activation, service provided.
    ctx.provide("config", Arc::new(json!({ "uri": "pg://x" })))
        .expect("provide config");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(
        ctx.get::<Value>("db").map(|v| v.as_ref().clone()),
        Some(json!({ "connected": true }))
    );

    // Declarations are introspectable (name + provides) without running
    // the body again: the cycle diagnostics see them.
    let summaries = ctx.installed_plugins();
    let summary = summaries
        .iter()
        .find(|s| s.name.as_deref() == Some("database"))
        .expect("plugin summary");
    assert!(!summary.fibers.is_empty());
}

#[test]
fn plugin_macro_defaults_name_and_generates_pascal_struct() {
    let ctx = Context::new();

    /// A plugin declared with no attributes: the struct name is the
    /// PascalCase of the function name; the plugin name defaults to the
    /// function name.
    #[plugin]
    fn echo_tube(ctx: &Context, _config: &Value) -> PluginResult {
        ctx.provide("echo", Arc::new(json!("on")))?;
        Ok(())
    }

    let handle = ctx.plugin(EchoTube, json!(null)).expect("install");
    assert!(handle.is_active());
    let summaries = ctx.installed_plugins();
    assert!(
        summaries.iter().any(|s| s.name.as_deref() == Some("echo_tube")),
        "default name is the function name: {:?}",
        summaries.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
    );
}

#[test]
fn plugin_macro_provides_feed_cycle_diagnostics() {
    let ctx = Context::new();

    // Two plugins whose declared provides close a loop among pending
    // required injects: the diagnostic must fire.
    #[plugin(inject(required = ["a"]), provides = ["b"])]
    fn needs_a(ctx: &Context, _config: &Value) -> PluginResult {
        let _ = ctx;
        Ok(())
    }
    #[plugin(inject(required = ["b"]), provides = ["a"])]
    fn needs_b(ctx: &Context, _config: &Value) -> PluginResult {
        let _ = ctx;
        Ok(())
    }

    ctx.plugin(NeedsA, json!(null)).expect("install a");
    ctx.plugin(NeedsB, json!(null)).expect("install b");

    let cycles = ctx.dependency_cycles();
    assert_eq!(cycles.len(), 1, "one stuck group: {cycles:?}");
    assert_eq!(cycles[0].len(), 2, "both members reported");
}
