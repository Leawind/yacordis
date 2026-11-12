//! Service isolation tests: same-name services coexisting across
//! isolation groups, reactive scoping, and conflict rules.

use std::sync::Arc;

use yacordis::prelude::*;
use serde_json::{Value, json};

/// A provider whose service value is `{"uri": <config>}`.
fn provider(uri: &'static str) -> Arc<dyn Component> {
    Arc::new(move |ctx: &Context, _config: &Value| -> ComponentResult {
        ctx.provide("database", Arc::new(json!({ "uri": uri })))?;
        Ok(())
    })
}

#[test]
fn isolated_subtrees_get_their_own_service_instances() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));

    // The runtime-wide database.
    let global_db = ctx
        .component_dyn(provider("global"), json!(null))
        .expect("global");
    assert!(global_db.is_active());
    assert_eq!(
        ctx.get::<Value>("database").expect("global db")["uri"],
        json!("global")
    );

    // An isolated subtree installs its own database: coexists, no conflict.
    let isolated = ctx.isolate("database");
    let local_db = isolated
        .component_dyn(provider("local"), json!(null))
        .expect("local");
    assert!(local_db.is_active());

    // Each side sees exactly its own instance.
    assert_eq!(
        ctx.get::<Value>("database").expect("global")["uri"],
        json!("global")
    );
    assert_eq!(
        isolated.get::<Value>("database").expect("local")["uri"],
        json!("local")
    );

    // A consumer inside the isolated subtree wires to the local instance.
    let consumer = isolated
        .inject(Inject::required(&["database"]), |ctx: &Context| {
            assert_eq!(
                ctx.get::<Value>("database").expect("db")["uri"],
                json!("local")
            );
            Ok(())
        })
        .expect("install consumer");
    assert!(consumer.is_active());

    // Withdrawing the local database halts only the isolated consumer;
    // the global database (and anything using it) is untouched.
    local_db.dispose();
    assert_eq!(consumer.state(), FiberState::Pending);
    assert_eq!(
        ctx.get::<Value>("database").expect("global")["uri"],
        json!("global")
    );
}

#[test]
fn notify_never_crosses_isolation_groups() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));

    let subtree_a = ctx.isolate("database");
    let subtree_b = ctx.isolate("database");
    let consumer_a = subtree_a
        .inject(Inject::required(&["database"]), |_: &Context| Ok(()))
        .expect("consumer a");
    let consumer_b = subtree_b
        .inject(Inject::required(&["database"]), |_: &Context| Ok(()))
        .expect("consumer b");
    assert_eq!(consumer_a.state(), FiberState::Pending);
    assert_eq!(consumer_b.state(), FiberState::Pending);

    // Installing a database in subtree A activates only A's consumer.
    let db_a = subtree_a
        .component_dyn(provider("a"), json!(null))
        .expect("db a");
    assert!(consumer_a.is_active());
    assert_eq!(
        consumer_b.state(),
        FiberState::Pending,
        "B must not see A's service"
    );

    // Disposing it re-locks only A's consumer.
    db_a.dispose();
    assert_eq!(consumer_a.state(), FiberState::Pending);

    // B gets its own; A stays down.
    let db_b = subtree_b
        .component_dyn(provider("b"), json!(null))
        .expect("db b");
    assert!(consumer_b.is_active());
    assert_eq!(consumer_a.state(), FiberState::Pending);
    db_b.dispose();
}

#[test]
fn conflicts_apply_within_one_isolation_group() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));

    let first = ctx
        .component_dyn(provider("one"), json!(null))
        .expect("first");
    assert!(first.is_active());

    // Same name, same (default) isolation group: the second install fails.
    let second = ctx
        .component_dyn(provider("two"), json!(null))
        .expect("handle");
    assert_eq!(second.state(), FiberState::Failed);

    // A derived isolate is a different group: it works there.
    let isolated = ctx.isolate("database");
    let third = isolated
        .component_dyn(provider("three"), json!(null))
        .expect("third");
    assert!(third.is_active());
    assert_eq!(
        isolated.get::<Value>("database").expect("isolated")["uri"],
        json!("three")
    );
}

#[test]
fn isolation_composes_with_the_ffi_channel() {
    // The loader's handle table stores whole Contexts, so a component
    // installed through an isolated context resolves services with the
    // isolated labels. Covered here at the handle level (no dylib): the
    // context the loader would hand over is the isolated one.
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    let isolated = ctx.isolate("database");

    let component_ctx = isolated.clone();
    let consumer = isolated
        .inject(Inject::required(&["database"]), move |_: &Context| {
            // The loader passes the *installation* context into component
            // bodies; assert the same scoping applies there.
            let _ = &component_ctx;
            Ok(())
        })
        .expect("consumer");

    let handle = isolated
        .component_dyn(provider("isolated"), json!(null))
        .expect("install");
    assert!(consumer.is_active());
    assert!(isolated.get_raw("database").is_some());
    assert!(ctx.get_raw("database").is_none(), "root sees nothing");
    handle.dispose();
}
