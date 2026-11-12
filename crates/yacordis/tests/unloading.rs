//! Registrations attempted while a fiber is in its unloading window
//! (cordis TS issues #96/#91: effects and listeners registered during
//! unload must not leak into the next activation).
//!
//! The window is reached deterministically: a disposer runs while the
//! fiber is marked `unloading`, so registration attempts from inside it
//! must be rejected as `InactiveContext` — and rejected cleanly enough
//! that the fiber can reactivate afterwards with no leftover state.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use yacordis::prelude::*;
use serde_json::{Value, json};

#[test]
fn registration_during_unload_is_rejected_without_leaks() {
    let ctx = Context::new();
    // (attempted action, rejected-as-InactiveContext)
    let attempts: Arc<Mutex<Vec<(&'static str, bool)>>> = Arc::default();
    let fired: Arc<Mutex<Vec<String>>> = Arc::default();

    let mk_provider = |generation: u32| {
        move |ctx: &Context, _: &Value| -> ComponentResult {
            ctx.provide("db", Arc::new(json!({ "gen": generation })))?;
            Ok(())
        }
    };

    // The provider goes in first so the consumer activates on install.
    let provider = ctx
        .component(mk_provider(1), json!(null))
        .expect("install provider");

    let consumer = {
        let attempts = attempts.clone();
        let fired = fired.clone();
        ctx.inject(Inject::required(&["db"]), move |ctx: &Context| {
            // A regular listener: re-registered on every activation,
            // disposed on every unload.
            ctx.on("evt", {
                let fired = fired.clone();
                move |_, _| {
                    fired.lock().unwrap().push("evt".to_owned());
                    None
                }
            })?;

            // The probe: its disposer runs inside the unloading window
            // and attempts one of every registration.
            ctx.effect("probe", {
                let attempts = attempts.clone();
                let ctx = ctx.clone();
                move |_| {
                    let attempts = attempts.clone();
                    let ctx = ctx.clone();
                    Ok(Some(Box::new(move || {
                        let mut results = attempts.lock().unwrap();
                        let r = ctx.on("late", |_, _| None);
                        results
                            .push(("on", matches!(&r, Err(CordisError::InactiveContext { .. }))));
                        let r = ctx.effect("late-eff", |_| {
                            Ok::<Option<Disposer>, Box<dyn std::error::Error + Send + Sync>>(None)
                        });
                        results.push((
                            "effect",
                            matches!(&r, Err(CordisError::InactiveContext { .. })),
                        ));
                        let r = ctx.provide_raw("late", Arc::new(json!(1)));
                        results.push((
                            "provide",
                            matches!(&r, Err(CordisError::InactiveContext { .. })),
                        ));
                        let r = ctx.middleware("late-mw", |_ctx, v, next| next(v));
                        results.push((
                            "middleware",
                            matches!(&r, Err(CordisError::InactiveContext { .. })),
                        ));
                        let r = {
                            let ctx = ctx.clone();
                            ctx.component(
                                move |_: &Context, _: &Value| -> ComponentResult { Ok(()) },
                                json!(null),
                            )
                        };
                        results.push((
                            "component",
                            matches!(&r, Err(CordisError::InactiveContext { .. })),
                        ));
                    }) as Disposer))
                }
            })?;
            Ok(())
        })
    }
    .expect("install consumer");
    assert!(consumer.is_active());
    assert_eq!(consumer.state(), FiberState::Active);

    // Disposing the provider keep-alive-unloads the consumer: the probe
    // disposer runs while the consumer is marked unloading (but not yet
    // disposed).
    assert!(provider.dispose());
    assert_eq!(consumer.state(), FiberState::Pending);

    let results = attempts.lock().unwrap().clone();
    assert_eq!(results.len(), 5, "every registration attempt must have run");
    for (action, rejected_as_inactive) in &results {
        assert!(
            *rejected_as_inactive,
            "`{action}` attempted during unload must be rejected as InactiveContext"
        );
    }

    // Nothing leaked from the rejected attempts.
    assert!(!ctx.has("late"), "rejected provide must not be registered");

    // The fiber survives as Pending and works normally after
    // reactivation: exactly one `evt` listener (from the fresh body),
    // not two, and no `late` listener at all.
    let provider = ctx
        .component(mk_provider(2), json!(null))
        .expect("reinstall provider");
    assert!(consumer.wait_for(FiberState::Active, Duration::from_secs(5)));
    ctx.emit("evt", &json!(null));
    ctx.emit("late", &json!(null));
    assert_eq!(*fired.lock().unwrap(), ["evt"]);

    provider.dispose();
    consumer.dispose();
    assert!(
        ctx.provided_services().is_empty(),
        "{:?}",
        ctx.provided_services()
    );
}
