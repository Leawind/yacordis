//! Service value schemas (the `check` predicate, Rust-shaped): a
//! registered schema gates every provide/set of that JSON service in
//! the isolation group — violations fail the call and leave the previous
//! value untouched. The registration is an effect: unloading the
//! registering fiber lifts the gate. Typed services bypass.

use std::sync::{Arc, Mutex};

use yacordis::prelude::*;
use serde_json::{Value, json};

fn counter_schema() -> Schema {
    Schema::object()
        .req("kind", Schema::enum_of([json!("tick"), json!("tock")]))
        .req("n", Schema::integer().at_least(0.0))
        .build()
}

#[test]
fn provide_schema_gates_provide_and_set() {
    let ctx = Context::new();
    let observed: Arc<Mutex<Vec<String>>> = Arc::default();

    let worker = ctx
        .component(
            {
                let observed = observed.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    let observed = observed.clone();
                    ctx.provide_schema("counter", counter_schema())
                        .expect("register schema");

                    // Valid provide passes and is visible.
                    ctx.provide("counter", Arc::new(json!({ "kind": "tick", "n": 0 })))?;

                    // Invalid set: rejected, previous value untouched.
                    let error = ctx
                        .set("counter", Arc::new(json!({ "kind": "tock", "n": -1 })))
                        .expect_err("negative n violates the schema");
                    observed.lock().unwrap().push(format!(
                        "set-rejected={} kept={}",
                        error.to_string().contains("at least 0"),
                        ctx.get::<Value>("counter")
                            .map(|v| v.as_ref().clone())
                            .unwrap()
                    ));

                    // A violating fresh provide fails the call.
                    ctx.provide_schema("fresh", counter_schema())
                        .expect("register fresh schema");
                    let error = ctx
                        .provide("fresh", Arc::new(json!({ "kind": "tick", "n": "NaN" })))
                        .expect_err("string n violates the schema");
                    observed.lock().unwrap().push(format!(
                        "provide-rejected={}",
                        error.to_string().contains("expected an integer")
                    ));
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install worker");

    assert!(worker.is_active(), "the body itself must not fail");
    let log = observed.lock().unwrap();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log[0].contains("set-rejected=true"), "{log:?}");
    assert!(
        log[0].contains(r#""kind":"tick","n":0"#) || log[0].contains(r#""n":0"#),
        "previous value survived: {log:?}"
    );
    assert!(log[1].contains("provide-rejected=true"), "{log:?}");
}

#[test]
fn schema_registration_dies_with_its_fiber() {
    let ctx = Context::new();
    let observed: Arc<Mutex<Vec<String>>> = Arc::default();

    // A scoped fiber raises the gate and probes it from inside.
    let scope = ctx
        .component(
            {
                let observed = observed.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    let observed = observed.clone();
                    ctx.provide_schema("temporary", counter_schema())
                        .expect("register schema");
                    // Violating provide while the gate is up: rejected.
                    let error = ctx
                        .provide("temporary", Arc::new(json!("not an object")))
                        .expect_err("gate active");
                    observed.lock().unwrap().push(format!(
                        "gated={}",
                        error.to_string().contains("expected an object")
                    ));
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install scope");
    scope.dispose();
    assert_eq!(
        observed.lock().unwrap().as_slice(),
        ["gated=true"],
        "the gate rejected while its fiber was alive"
    );

    // After disposal the gate is gone: a violating value provides fine.
    let after = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("temporary", Arc::new(json!("anything goes now")))?;
                Ok(())
            },
            json!(null),
        )
        .expect("provide after gate lifted");
    assert!(after.is_active());
}

#[test]
fn typed_services_bypass_service_schemas() {
    let ctx = Context::new();
    let worker = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide_schema("magic", counter_schema())?;
                ctx.provide("magic", Arc::new(42u32))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert!(worker.is_active(), "typed services bypass the schema");
    assert_eq!(ctx.get::<u32>("magic").map(|v| *v), Some(42));
}
