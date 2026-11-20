use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yacordis::prelude::*;

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

    let global_db = ctx
        .component_dyn(provider("global"), json!(null))
        .expect("global");
    assert!(global_db.is_active());
    assert_eq!(
        ctx.get::<Value>("database").expect("global db")["uri"],
        json!("global")
    );

    let isolated = ctx.isolate("database");
    let local_db = isolated
        .component_dyn(provider("local"), json!(null))
        .expect("local");
    assert!(local_db.is_active());

    assert_eq!(
        ctx.get::<Value>("database").expect("global")["uri"],
        json!("global")
    );
    assert_eq!(
        isolated.get::<Value>("database").expect("local")["uri"],
        json!("local")
    );

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

    let db_a = subtree_a
        .component_dyn(provider("a"), json!(null))
        .expect("db a");
    assert!(consumer_a.is_active());
    assert_eq!(
        consumer_b.state(),
        FiberState::Pending,
        "B must not see A's service"
    );

    db_a.dispose();
    assert_eq!(consumer_a.state(), FiberState::Pending);

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
    // Handle-level stand-in for the loader (no dylib): the context handed
    // to components is the isolated one.
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    let isolated = ctx.isolate("database");

    let component_ctx = isolated.clone();
    let consumer = isolated
        .inject(Inject::required(&["database"]), move |_: &Context| {
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

#[test]
fn service_events_stay_in_their_isolation_group() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));

    let seen_from_shared: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen_for_listener = seen_from_shared.clone();
    ctx.on("internal/service", move |_, args| {
        seen_for_listener
            .lock()
            .unwrap()
            .push(args[0].as_str().unwrap_or("?").to_owned());
        None
    })
    .expect("shared observer");

    // An observer inside an isolated group.
    let isolated = ctx.isolate("database");
    let seen_from_group: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen_for_observer = seen_from_group.clone();
    isolated
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let seen = seen_for_observer.clone();
                ctx.on("internal/service", move |_, args| {
                    seen.lock()
                        .unwrap()
                        .push(args[0].as_str().unwrap_or("?").to_owned());
                    None
                })?;
                Ok(())
            },
            json!(null),
        )
        .expect("group observer");

    // A provide inside the isolated group: the shared observer must not
    // see it, the group's own observer must.
    let local_db = isolated
        .component_dyn(provider("local"), json!(null))
        .expect("local db");
    assert!(local_db.is_active());
    assert!(
        !seen_from_shared
            .lock()
            .unwrap()
            .contains(&"database".to_owned()),
        "isolated service events do not cross the group boundary"
    );
    assert!(
        seen_from_group.lock().unwrap().contains(&"database".to_owned()),
        "the group's own observer sees the event"
    );

    // A shared-group service reaches both observers (the group's map only
    // isolates the name it was derived for).
    seen_from_shared.lock().unwrap().clear();
    seen_from_group.lock().unwrap().clear();
    ctx.component_dyn(
        Arc::new(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("other", Arc::new(json!({})))?;
                Ok(())
            },
        ),
        json!(null),
    )
    .expect("shared provider");
    assert!(
        seen_from_shared.lock().unwrap().contains(&"other".to_owned()),
        "shared observers see shared-group events"
    );
    assert!(
        seen_from_group.lock().unwrap().contains(&"other".to_owned()),
        "a group observer with no isolation for that name sees shared events"
    );
}
