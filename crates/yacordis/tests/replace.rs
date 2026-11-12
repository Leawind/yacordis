//! In-place component replacement (v4 `registry.replace()`, #266): fibers
//! keep their identity — id, parent, config — and re-run the new body
//! through the normal unload/reload cascade, so dependents observe the
//! service change and a `Failed` fiber recovers.

use std::sync::{Arc, Mutex};

use yacordis::prelude::*;
use serde_json::{Value, json};

/// A provider component: body closure specialized per value, identified by
/// its own `Arc` (distinct closures = distinct component identities).
fn provider(value: &'static str) -> Arc<dyn Component> {
    Arc::new(move |ctx: &Context, _: &Value| -> ComponentResult {
        ctx.provide("svc", Arc::new(json!(value)))?;
        Ok(())
    })
}

fn read_svc(ctx: &Context) -> Option<Value> {
    ctx.get_raw("svc")
        .and_then(|any| any.downcast_ref::<Value>().cloned())
}

#[test]
fn replace_preserves_fiber_identity_and_updates_dependents() {
    let ctx = Context::new();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();

    let p1 = provider("v1");
    let provider_handle = ctx
        .component_dyn(p1.clone(), json!(null))
        .expect("install p1");
    let old_id = provider_handle.id();

    let dependent = ctx
        .inject(Inject::required(&["svc"]), {
            let seen = seen.clone();
            move |ctx: &Context| {
                seen.lock()
                    .unwrap()
                    .push(read_svc(ctx).expect("svc visible"));
                Ok(())
            }
        })
        .expect("install dependent");
    assert_eq!(*seen.lock().unwrap(), [json!("v1")]);

    let p2 = provider("v2");
    let handles = ctx.replace_component(p1, p2).expect("replace");
    assert_eq!(handles.len(), 1);
    // Identity: the replacement handle is the same fiber.
    assert_eq!(handles[0].id(), old_id, "fiber id must survive replace");
    assert_eq!(provider_handle.id(), old_id);

    // The dependent re-ran with the new service value.
    assert_eq!(*seen.lock().unwrap(), [json!("v1"), json!("v2")]);
    assert_eq!(dependent.state(), FiberState::Active);
    assert_eq!(read_svc(&ctx), Some(json!("v2")));

    // The old identity is gone: installing it again starts fresh.
    let fresh = ctx
        .component_dyn(provider("v1"), json!(null))
        .expect("reinstall old identity");
    assert_ne!(fresh.id(), old_id);
}

#[test]
fn replace_recovers_a_failed_fiber() {
    let ctx = Context::new();

    let broken: Arc<dyn Component> =
        Arc::new(|_: &Context, _: &Value| -> ComponentResult { Err(ComponentError::new("boom")) });
    let handle = ctx
        .component_dyn(broken.clone(), json!(null))
        .expect("install");
    assert_eq!(handle.state(), FiberState::Failed);

    let handles = ctx
        .replace_component(broken, provider("fixed"))
        .expect("replace");
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].state(), FiberState::Active);
    assert_eq!(read_svc(&ctx), Some(json!("fixed")));
}

#[test]
fn replace_keeps_config_and_runs_every_fiber_of_the_component() {
    let ctx = Context::new();
    let configs: Arc<Mutex<Vec<Value>>> = Arc::default();

    // Two installs of the same Arc share one runtime; replace must move
    // both fibers.
    let component_v1: Arc<dyn Component> =
        Arc::new(|_: &Context, _: &Value| -> ComponentResult { Ok(()) });
    let h1 = ctx
        .component_dyn(component_v1.clone(), json!({ "n": 1 }))
        .expect("install #1");
    let h2 = ctx
        .component_dyn(component_v1.clone(), json!({ "n": 2 }))
        .expect("install #2");

    let configs_for_body = configs.clone();
    let component_v2: Arc<dyn Component> =
        Arc::new(move |_: &Context, config: &Value| -> ComponentResult {
            configs_for_body.lock().unwrap().push(config.clone());
            Ok(())
        });
    let handles = ctx
        .replace_component(component_v1, component_v2)
        .expect("replace");
    assert_eq!(handles.len(), 2);
    assert_eq!(handles[0].id(), h1.id());
    assert_eq!(handles[1].id(), h2.id());
    // Both bodies ran, each with its own preserved config.
    let mut seen = configs.lock().unwrap().clone();
    seen.sort_by_key(|value| value["n"].as_i64().unwrap());
    assert_eq!(seen, [json!({ "n": 1 }), json!({ "n": 2 })]);
}

#[test]
fn replace_of_uninstalled_component_errors() {
    let ctx = Context::new();
    let err = ctx
        .replace_component(provider("v1"), provider("v2"))
        .expect_err("not installed");
    assert!(
        err.to_string().contains("not installed"),
        "unexpected error: {err}"
    );
}

#[test]
fn replace_swaps_declared_inject() {
    let ctx = Context::new();
    let activated: Arc<Mutex<Vec<&'static str>>> = Arc::default();

    // v1 has no requirements and activates immediately.
    let v1: Arc<dyn Component> = Arc::new(|_: &Context, _: &Value| -> ComponentResult { Ok(()) });
    let handle = ctx
        .component_dyn(v1.clone(), json!(null))
        .expect("install v1");
    assert_eq!(handle.state(), FiberState::Active);

    // v2 requires a missing service: the same fiber drops to Pending.
    let activated_for_body = activated.clone();
    let v2: Arc<dyn Component> = Arc::new(move |_: &Context, _: &Value| -> ComponentResult {
        activated_for_body.lock().unwrap().push("v2 body");
        Ok(())
    });
    let inject = Inject::required(&["missing"]);
    let v2 = Arc::new(InjectClosure2 {
        inject,
        component: v2,
    });
    ctx.replace_component(v1, v2).expect("replace");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(activated.lock().unwrap().is_empty());

    // Satisfying the new requirement activates the fiber.
    ctx.provide("missing", Arc::new(json!(true)))
        .expect("provide");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(*activated.lock().unwrap(), ["v2 body"]);
}

/// Adapter pairing an inject declaration with an inner component closure.
struct InjectClosure2 {
    inject: Inject,
    component: Arc<dyn Component>,
}

impl Component for InjectClosure2 {
    fn inject(&self) -> Inject {
        self.inject.clone()
    }
    fn name(&self) -> Option<&str> {
        self.component.name()
    }
    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
        self.component.apply(ctx, config)
    }
}
