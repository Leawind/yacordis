//! Service intercepts (§2.7): config-layered overrides along the fiber
//! chain — a layer set on a fiber is visible to that fiber and its
//! **descendants** only (upstream PR #90/#206 semantics), nearest
//! ancestor wins, shallow object merge, non-object layers replace,
//! typed services untouched. Entries are effects: unloading the
//! registering fiber removes its layer (I2 cleanup), and intercepts
//! never disturb epochs or notifications.

use std::sync::Arc;

use yacordis::prelude::*;
use serde_json::{Value, json};

/// Provides a base JSON config object.
#[component(name = "base", provides = ["cfg"])]
fn base_component(ctx: &Context, _config: &Value) -> ComponentResult {
    ctx.provide(
        "cfg",
        Arc::new(json!({ "host": "localhost", "port": 80, "tls": false })),
    )?;
    Ok(())
}

fn view(ctx: &Context) -> Value {
    ctx.get::<Value>("cfg")
        .map(|v| v.as_ref().clone())
        .expect("cfg visible")
}

#[test]
fn intercept_layer_is_visible_to_self_and_descendants() {
    let ctx = Context::new();
    let base = ctx
        .component(BaseComponent, json!(null))
        .expect("install base");
    assert_eq!(
        view(&ctx),
        json!({ "host": "localhost", "port": 80, "tls": false })
    );

    // The interceptor fiber's layer: visible to itself and its
    // descendants, invisible to the root view (siblings are not
    // ancestors — upstream PR #90/#206 semantics).
    let interceptor = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.intercept("cfg", json!({ "port": 8080, "tls": true }))?;
                assert_eq!(
                    view(ctx),
                    json!({ "host": "localhost", "port": 8080, "tls": true }),
                    "own layer visible from own context"
                );
                let child = ctx
                    .component(
                        |ctx: &Context, _: &Value| -> ComponentResult {
                            assert_eq!(view(ctx)["tls"], json!(true), "descendant sees the layer");
                            Ok(())
                        },
                        json!(null),
                    )
                    .expect("child");
                assert!(child.is_active());
                Ok(())
            },
            json!(null),
        )
        .expect("install interceptor");
    assert_eq!(
        view(&ctx),
        json!({ "host": "localhost", "port": 80, "tls": false }),
        "root view unaffected by a sibling's layer"
    );

    // Unloading the interceptor drops the layer: a fresh descendant of
    // the root sees the base again (I2: entries die with their fiber).
    interceptor.dispose();
    let after = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                assert_eq!(
                    view(ctx),
                    json!({ "host": "localhost", "port": 80, "tls": false })
                );
                Ok(())
            },
            json!(null),
        )
        .expect("post-dispose observer");
    assert!(after.is_active());
    let _ = base;
}

#[test]
fn nearest_ancestor_layer_wins_and_layers_compose() {
    let ctx = Context::new();
    ctx.component(BaseComponent, json!(null))
        .expect("install base");

    // Parent fiber sets one layer.
    let parent = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.intercept("cfg", json!({ "port": 8080 }))?;
                // A child fiber under it sets a nearer layer; the child's
                // view sees both, its own winning.
                let child = ctx
                    .component(
                        |ctx: &Context, _: &Value| -> ComponentResult {
                            ctx.intercept("cfg", json!({ "tls": true }))?;
                            assert_eq!(
                                view(ctx),
                                json!({ "host": "localhost", "port": 8080, "tls": true }),
                                "child sees parent layer + own layer"
                            );
                            // A grandchild overrides further: nearest wins.
                            let grandchild = ctx
                                .component(
                                    |ctx: &Context, _: &Value| -> ComponentResult {
                                        ctx.intercept("cfg", json!({ "port": 9090 }))?;
                                        assert_eq!(view(ctx)["port"], json!(9090), "nearest wins");
                                        Ok(())
                                    },
                                    json!(null),
                                )
                                .expect("grandchild");
                            assert!(grandchild.is_active());
                            Ok(())
                        },
                        json!(null),
                    )
                    .expect("child");
                assert!(child.is_active());
                Ok(())
            },
            json!(null),
        )
        .expect("parent");
    parent.dispose();
    assert_eq!(
        view(&ctx),
        json!({ "host": "localhost", "port": 80, "tls": false })
    );
}

#[test]
fn non_object_layer_replaces_entirely() {
    let ctx = Context::new();
    ctx.component(BaseComponent, json!(null))
        .expect("install base");
    ctx.component(
        |ctx: &Context, _: &Value| -> ComponentResult {
            ctx.intercept("cfg", json!("plain string"))?;
            assert_eq!(view(ctx), json!("plain string"));
            Ok(())
        },
        json!(null),
    )
    .expect("install interceptor");
    assert_eq!(
        view(&ctx),
        json!({ "host": "localhost", "port": 80, "tls": false })
    );
}

#[test]
fn typed_services_ignore_intercepts() {
    let ctx = Context::new();
    let provider = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("counter", Arc::new(31u32))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    ctx.component(
        |ctx: &Context, _: &Value| -> ComponentResult {
            ctx.intercept("counter", json!(99))?;
            Ok(())
        },
        json!(null),
    )
    .expect("install interceptor");
    assert_eq!(
        ctx.get::<u32>("counter").map(|v| *v),
        Some(31),
        "typed services are returned untouched"
    );
    let _ = provider;
}

#[test]
fn intercepts_do_not_disturb_lifecycle() {
    let ctx = Context::new();
    let handle = ctx
        .component(BaseComponent, json!(null))
        .expect("install base");
    ctx.component(
        |ctx: &Context, _: &Value| -> ComponentResult {
            ctx.intercept("cfg", json!({ "port": 8080 }))?;
            Ok(())
        },
        json!(null),
    )
    .expect("install interceptor");
    assert!(handle.is_active(), "intercepts never reload the provider");
    assert!(
        ctx.dependency_cycles().is_empty(),
        "intercepts are view-layer only"
    );
}
