//! internal/update interception dies with the fiber that registered it
//! (cordis TS #56: update hooks leaked past fiber disposal and kept
//! rewriting later updates). Positive control first: while the fiber is
//! alive its middleware is applied; after disposal, later updates pass
//! through untouched.

use std::sync::{Arc, Mutex};

use yacordis::prelude::*;
use serde_json::{Value, json};

#[test]
fn update_middleware_dies_with_its_fiber() {
    let ctx = Context::new();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::default();

    let interceptor = ctx
        .component(
            {
                let calls = calls.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    ctx.middleware("internal/update", {
                        let calls = calls.clone();
                        move |_, _config, next| {
                            calls.lock().unwrap().push("interceptor");
                            next(json!({ "marked": true }))
                        }
                    })?;
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install interceptor");

    let target = ctx
        .component(|_: &Context, _: &Value| Ok(()), json!(null))
        .expect("install target");

    // Positive control: a live interceptor rewrites the update config.
    target
        .update(json!({ "n": 1 }))
        .expect("update while alive");
    assert_eq!(
        target.config().unwrap(),
        json!({ "marked": true }),
        "live middleware must rewrite the config"
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["interceptor"]);

    interceptor.dispose();

    // After disposal the middleware is gone: config passes through and
    // the dead fiber's closure is never invoked again.
    target
        .update(json!({ "n": 2 }))
        .expect("update after dispose");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["interceptor"],
        "disposed fiber's middleware must not observe later updates"
    );
    assert_eq!(target.config().unwrap(), json!({ "n": 2 }));
}

/// Prepend ordering (cordis TS #296, `DisposableList.unshift`): a
/// prepended update middleware runs before previously registered ones,
/// most recently prepended first; appended entries keep registration
/// order. Removing one prepended entry leaves the rest of the chain
/// intact.
#[test]
fn prepended_update_middleware_runs_first() {
    let ctx = Context::new();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::default();
    let dispose_b: Arc<Mutex<Option<EffectHandle>>> = Arc::default();

    let _interceptor = ctx
        .component(
            {
                let calls = calls.clone();
                let dispose_b = dispose_b.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    ctx.middleware("internal/update", {
                        let calls = calls.clone();
                        move |_, config: Value, next| {
                            calls.lock().unwrap().push("a");
                            next(config)
                        }
                    })?;
                    let handle = ctx.middleware_prepend("internal/update", {
                        let calls = calls.clone();
                        move |_, config: Value, next| {
                            calls.lock().unwrap().push("b");
                            next(config)
                        }
                    })?;
                    *dispose_b.lock().unwrap() = Some(handle);
                    ctx.middleware("internal/update", {
                        let calls = calls.clone();
                        move |_, config: Value, next| {
                            calls.lock().unwrap().push("c");
                            next(config)
                        }
                    })?;
                    ctx.middleware_prepend("internal/update", {
                        let calls = calls.clone();
                        move |_, config: Value, next| {
                            calls.lock().unwrap().push("d");
                            next(config)
                        }
                    })?;
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install interceptor");

    let target = ctx
        .component(|_: &Context, _: &Value| Ok(()), json!(null))
        .expect("install target");
    target.update(json!({ "n": 1 })).expect("first update");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["d", "b", "a", "c"],
        "prepended entries run first, most recent prepend first"
    );

    dispose_b
        .lock()
        .unwrap()
        .take()
        .expect("b's handle")
        .dispose();
    calls.lock().unwrap().clear();
    target.update(json!({ "n": 2 })).expect("second update");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["d", "a", "c"],
        "removing one prepended entry leaves the rest of the chain intact"
    );
}
