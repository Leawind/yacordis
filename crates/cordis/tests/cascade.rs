//! Deep dependency-chain unwinding (the depth ≥ 2 counterpart of
//! cordis TS issue #110: "provider unloads before its consumers").
//!
//! When the root provider of an A ← B ← C chain is disposed, every
//! consumer must fully halt *inside* the provider's withdrawal, and no
//! consumer may observe its dependency after the providing fiber released
//! it. Reinstalling the root revives the whole chain.
//!
//! House rule: never assert inside disposers/listeners (their panics are
//! swallowed by design) — record observations, assert on the test thread.

use std::sync::{Arc, Mutex};

use cordis::prelude::*;
use serde_json::{Value, json};

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, msg: &str) {
    log.lock().unwrap().push(msg.to_owned());
}

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

fn count(log: &Log, msg: &str) -> usize {
    logged(log).iter().filter(|e| *e == msg).count()
}

#[test]
fn multi_level_dependency_chain_unwinds_consumers_first() {
    let ctx = Context::new();
    let log: Log = Arc::default();
    // Observations recorded inside disposers, in encounter order.
    let saw: Arc<Mutex<Vec<String>>> = Arc::default();

    // A: provides "a", then registers a plain effect. Registration order
    // [provide, effect] means teardown runs the effect disposer first and
    // the withdrawal (which drives the whole downstream cascade) last.
    let mk_provider_a = |log: Log, tag: &'static str| {
        move |ctx: &Context, _: &Value| -> ComponentResult {
            ctx.provide("a", Arc::new(json!({ "from": tag })))?;
            ctx.effect("a/eff", {
                let log = log.clone();
                move |_| {
                    push(&log, &format!("{tag}:setup"));
                    let log = log.clone();
                    Ok(Some(Box::new(move || {
                        push(&log, &format!("{tag}:dispose"));
                    }) as Disposer))
                }
            })?;
            Ok(())
        }
    };

    // C: injects "b", logs its own setup/dispose.
    let consumer = {
        let log = log.clone();
        ctx.inject(Inject::required(&["b"]), move |ctx: &Context| {
            ctx.effect("c/eff", {
                let log = log.clone();
                move |_| {
                    push(&log, "c:setup");
                    let log = log.clone();
                    Ok(Some(Box::new(move || push(&log, "c:dispose")) as Disposer))
                }
            })?;
            Ok(())
        })
    }
    .expect("install C");

    // B: injects "a", provides "b". Registration order
    // [b/after-cascade, provide, b/eff] gives teardown order
    // [b/eff, withdraw-b (cascades into C), b/after-cascade] — so the
    // recorded observations cover both ends of the invariant:
    // - inside b/eff: the upstream service "a" is already gone;
    // - inside b/after-cascade: the grandchild C is already fully halted.
    let middle = {
        let log = log.clone();
        let saw = saw.clone();
        let handle_c = consumer.clone();
        ctx.inject(Inject::required(&["a"]), move |ctx: &Context| {
            ctx.effect("b/after-cascade", {
                let saw = saw.clone();
                let handle_c = handle_c.clone();
                let log = log.clone();
                move |_| {
                    let saw = saw.clone();
                    let handle_c = handle_c.clone();
                    let log = log.clone();
                    Ok(Some(Box::new(move || {
                        saw.lock().unwrap().push(format!(
                            "after_cascade c_state={:?} c_dispose_logged={}",
                            handle_c.state(),
                            logged(&log).iter().any(|e| e == "c:dispose"),
                        ));
                    }) as Disposer))
                }
            })?;
            ctx.provide("b", Arc::new(json!({ "from": "b" })))?;
            ctx.effect("b/eff", {
                let saw = saw.clone();
                let ctx = ctx.clone();
                let log = log.clone();
                move |_| {
                    push(&log, "b:setup");
                    let saw = saw.clone();
                    let ctx = ctx.clone();
                    let log = log.clone();
                    Ok(Some(Box::new(move || {
                        push(&log, "b:dispose");
                        saw.lock().unwrap().push(format!(
                            "a_visible_to_b_disposer={}",
                            ctx.get::<Value>("a").is_some(),
                        ));
                    }) as Disposer))
                }
            })?;
            Ok(())
        })
    }
    .expect("install B");

    let root_provider = ctx
        .component(mk_provider_a(log.clone(), "a1"), json!(null))
        .expect("install A");
    assert!(root_provider.is_active());
    assert!(middle.is_active());
    assert!(consumer.is_active());
    assert_eq!(logged(&log), ["a1:setup", "b:setup", "c:setup"]);

    // Dispose the root: the cascade must nest consumer teardowns inside
    // the provider's withdrawal.
    assert!(root_provider.dispose());
    assert_eq!(logged(&log)[3..], ["a1:dispose", "b:dispose", "c:dispose"]);
    assert_eq!(root_provider.state(), FiberState::Disposed);
    assert_eq!(middle.state(), FiberState::Pending);
    assert_eq!(consumer.state(), FiberState::Pending);

    // Observations from inside the teardown, in encounter order.
    assert_eq!(
        *saw.lock().unwrap(),
        [
            "a_visible_to_b_disposer=false", // B's disposer: "a" already withdrawn
            "after_cascade c_state=Pending c_dispose_logged=true", // C fully halted first
        ],
        "{:?}",
        *saw.lock().unwrap()
    );

    // Reinstalling the root revives the entire chain (bodies re-run).
    let root_provider = ctx
        .component(mk_provider_a(log.clone(), "a2"), json!(null))
        .expect("reinstall A");
    assert!(root_provider.is_active());
    assert!(middle.is_active());
    assert!(consumer.is_active());
    assert_eq!(count(&log, "b:setup"), 2);
    assert_eq!(count(&log, "c:setup"), 2);
}
