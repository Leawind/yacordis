//! Panic containment at the remaining catch_unwind sites beyond the
//! component body (covered in `lifecycle.rs`).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yacordis::prelude::*;

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, msg: &str) {
    log.lock().unwrap().push(msg.to_owned());
}

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

/// Box coercion needs the target type known (see AGENTS.md pitfalls).
fn boom_disposer(log: Log) -> Disposer {
    Box::new(move || {
        push(&log, "boom:dispose");
        panic!("disposer boom");
    })
}

#[test]
fn panicking_listener_does_not_poison_dispatch() {
    let ctx = Context::new();
    let second_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));

    ctx.on("evt", |_, _| panic!("listener boom")).expect("on");
    ctx.on("evt", {
        let second_ran = second_ran.clone();
        move |_, _| {
            second_ran.store(true, std::sync::atomic::Ordering::SeqCst);
            None
        }
    })
    .expect("on");

    ctx.emit("evt", &json!(null));
    assert!(second_ran.load(std::sync::atomic::Ordering::SeqCst));

    // Under bail a panic counts as "no result"; dispatch continues.
    let result = Arc::new(std::sync::atomic::AtomicBool::new(false));
    ctx.on("bail-evt", |_, _| panic!("bail boom")).expect("on");
    ctx.on("bail-evt", {
        let result = result.clone();
        move |_, _| {
            result.store(true, std::sync::atomic::Ordering::SeqCst);
            Some(json!("answer"))
        }
    })
    .expect("on");
    assert_eq!(ctx.bail("bail-evt", &json!(null)), Some(json!("answer")));
}

#[test]
fn panicking_disposer_does_not_block_remaining_disposers() {
    let ctx = Context::new();
    let log: Log = Arc::default();

    let handle = ctx
        .component(
            {
                let log = log.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    // Registered first → disposed last: proves the chain
                    // continued past the panicking disposer.
                    ctx.effect("eff/after", {
                        let log = log.clone();
                        move |_| {
                            Ok(Some(
                                Box::new(move || push(&log, "after:dispose")) as Disposer
                            ))
                        }
                    })?;
                    ctx.effect("eff/boom", {
                        let log = log.clone();
                        move |_| Ok(Some(boom_disposer(log.clone())))
                    })?;
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install");

    assert!(handle.dispose());
    assert_eq!(logged(&log), ["boom:dispose", "after:dispose"]);
    assert_eq!(handle.state(), FiberState::Disposed);
}

#[test]
fn panicking_middleware_aborts_update_with_error() {
    let ctx = Context::new();
    let handle = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.middleware("internal/update", |_ctx, _value, _next| {
                    panic!("middleware boom");
                })?;
                Ok(())
            },
            json!({ "v": 1 }),
        )
        .expect("install");
    assert!(handle.is_active());

    let err = handle
        .update(json!({ "v": 2 }))
        .expect_err("update must fail");
    assert!(
        err.to_string()
            .contains("middleware panicked: middleware boom"),
        "{err}"
    );
    assert_eq!(handle.config(), Some(json!({ "v": 1 })));
    assert_eq!(handle.state(), FiberState::Active);
}

#[test]
fn panicking_effect_setup_fails_the_fiber_and_reverts_collected_effects() {
    let ctx = Context::new();
    let log: Log = Arc::default();
    let recover = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let handle = {
        let recover = recover.clone();
        let log = log.clone();
        ctx.component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.effect("eff/before", {
                    let log = log.clone();
                    move |_| {
                        Ok(Some(
                            Box::new(move || push(&log, "before:dispose")) as Disposer
                        ))
                    }
                })?;
                if !recover.load(std::sync::atomic::Ordering::SeqCst) {
                    // The setup panic reverts already-collected disposers
                    // on the way out, then fails the fiber.
                    ctx.effect(
                        "eff/boom",
                        |_| -> Result<EffectOutcome, Box<dyn std::error::Error + Send + Sync>> {
                            panic!("setup boom");
                        },
                    )?;
                }
                Ok(())
            },
            json!(null),
        )
        .expect("install")
    };

    assert_eq!(handle.state(), FiberState::Failed);
    let error = handle.error().expect("failure recorded");
    assert!(error.contains("component panicked: setup boom"), "{error}");
    assert_eq!(logged(&log), ["before:dispose"]);

    // update() re-runs the body without the panic branch; the fresh
    // `eff/before` stays live (no second dispose).
    recover.store(true, std::sync::atomic::Ordering::SeqCst);
    handle.update(json!(null)).expect("update recovers");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(
        logged(&log)
            .iter()
            .filter(|e| *e == "before:dispose")
            .count(),
        1,
        "only the failed round's effect may have been disposed: {:?}",
        logged(&log)
    );
}
