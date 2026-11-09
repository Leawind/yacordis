//! internal/update interception dies with the fiber that registered it
//! (cordis TS #56: update hooks leaked past fiber disposal and kept
//! rewriting later updates). Positive control first: while the fiber is
//! alive its middleware is applied; after disposal, later updates pass
//! through untouched.

use std::sync::{Arc, Mutex};

use cordis::prelude::*;
use serde_json::{Value, json};

#[test]
fn update_middleware_dies_with_its_fiber() {
    let ctx = Context::new();
    let calls: Arc<Mutex<Vec<&'static str>>> = Arc::default();

    let interceptor = ctx
        .plugin(
            {
                let calls = calls.clone();
                move |ctx: &Context, _: &Value| -> PluginResult {
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
        .plugin(|_: &Context, _: &Value| Ok(()), json!(null))
        .expect("install target");

    // Positive control: a live interceptor rewrites the update config.
    target.update(json!({ "n": 1 })).expect("update while alive");
    assert_eq!(
        target.config().unwrap(),
        json!({ "marked": true }),
        "live middleware must rewrite the config"
    );
    assert_eq!(calls.lock().unwrap().as_slice(), ["interceptor"]);

    interceptor.dispose();

    // After disposal the middleware is gone: config passes through and
    // the dead fiber's closure is never invoked again.
    target.update(json!({ "n": 2 })).expect("update after dispose");
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["interceptor"],
        "disposed fiber's middleware must not observe later updates"
    );
    assert_eq!(target.config().unwrap(), json!({ "n": 2 }));
}
