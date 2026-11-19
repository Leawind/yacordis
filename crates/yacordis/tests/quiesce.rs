//! The host-side quiescence primitives: process-global `quiesce` and the
//! `executing_lifecycle_op` guard that keeps it from waiting for itself.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yacordis::prelude::*;

#[test]
fn component_bodies_run_inside_a_lifecycle_op() {
    let ctx = Context::new();
    let observed = Arc::new(Mutex::new(None));
    let observed_for_body = observed.clone();
    let handle = ctx
        .component(
            move |_: &Context, _: &Value| -> ComponentResult {
                *observed_for_body.lock().unwrap() = Some(yacordis::executing_lifecycle_op());
                Ok(())
            },
            json!(null),
        )
        .expect("install");
    assert!(handle.is_active());
    assert_eq!(
        observed.lock().unwrap().take(),
        Some(true),
        "apply runs inside a lifecycle op"
    );
    assert!(
        !yacordis::executing_lifecycle_op(),
        "the install call itself is not an op"
    );
}

#[test]
fn quiesce_settles_concurrent_lifecycle_churn() {
    let ctx = Arc::new(Context::new());
    let provider = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("q", Arc::new(json!(1)))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install provider");

    // Concurrent churn: consumers install and dispose against the provider.
    let ctx_for_churn = ctx.clone();
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let ctx = ctx_for_churn.clone();
                scope.spawn(move || {
                    for _ in 0..25 {
                        let consumer = ctx
                            .inject(Inject::required(&["q"]), |_: &Context| Ok(()))
                            .expect("install consumer");
                        assert!(consumer.is_active());
                        consumer.dispose();
                    }
                })
            })
            .collect();
        threads
            .into_iter()
            .for_each(|thread| thread.join().expect("churn"));
    });
    provider.dispose();

    yacordis::quiesce();
    assert_eq!(yacordis::test_support::pending_ops(), 0);
}
