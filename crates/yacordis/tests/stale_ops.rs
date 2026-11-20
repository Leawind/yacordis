//! Epoch / stale-op semantics: a queued Reload whose premise vanished
//! must never pull a fiber back to Active. That state — active without
//! its dependencies — is the observable stale-replay bug.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use yacordis::prelude::*;

#[test]
fn queued_reload_whose_premise_vanished_is_dropped() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(DiscardLogger));
    let body_runs = Arc::new(AtomicUsize::new(0));

    // The wave listener queues a Reload (install p2) then an Unload
    // (dispose p2) on the busy consumer.
    ctx.on("wave", {
        let ctx = ctx.clone();
        move |_, _| {
            let p2 = ctx
                .component(
                    |ctx: &Context, _: &Value| -> ComponentResult {
                        ctx.provide("a", Arc::new(json!({ "gen": 2 })))?;
                        Ok(())
                    },
                    json!(null),
                )
                .expect("install p2");
            p2.dispose();
            None
        }
    })
    .expect("register wave listener");

    let consumer = {
        let body_runs = body_runs.clone();
        ctx.inject(Inject::required(&["a"]), move |ctx: &Context| {
            body_runs.fetch_add(1, Ordering::SeqCst);
            // The disposer runs while the consumer's scheduler is busy,
            // forcing the wave listener's ops onto the deferred queue.
            ctx.effect("wave-trigger", {
                let ctx = ctx.clone();
                move |_| {
                    let ctx = ctx.clone();
                    Ok(Some(Box::new(move || {
                        ctx.emit("wave", &json!(null));
                    }) as Disposer))
                }
            })?;
            Ok(())
        })
    }
    .expect("install consumer");

    let provider = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("a", Arc::new(json!({ "gen": 1 })))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install provider");
    assert!(consumer.is_active());
    assert_eq!(body_runs.load(Ordering::SeqCst), 1);

    assert!(provider.dispose());
    assert_eq!(consumer.state(), FiberState::Pending);
    assert_eq!(
        body_runs.load(Ordering::SeqCst),
        1,
        "stale Reload must not re-run the body"
    );
    let trace = yacordis::test_support::lifecycle_trace_fiber(consumer.id().expect("live fiber"));
    assert!(
        trace.iter().any(|line| line.contains("STALE")),
        "the queued Reload must be dropped by the stale guard: {trace:?}"
    );

    let handle = ctx
        .effect("root-provider", |ctx: &Context| {
            ctx.provide("a", Arc::new(json!({ "gen": 3 })))?;
            Ok(())
        })
        .expect("root provide");
    assert!(consumer.wait_for(FiberState::Active, Duration::from_secs(5)));
    assert_eq!(body_runs.load(Ordering::SeqCst), 2);
    handle.dispose();
    assert_eq!(consumer.state(), FiberState::Pending);
}

const CHURN_ROUNDS: usize = 60;
const CONSUMER_THREADS: usize = 4;
const CONSUMER_ROUNDS: usize = 30;
const WATCHDOG: Duration = Duration::from_secs(120);

fn poll_until(mut check: impl FnMut() -> bool, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    check()
}

#[test]
fn fibers_never_settle_active_without_required_deps() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(DiscardLogger));

    let violations: Arc<Mutex<Vec<String>>> = Arc::default();
    let providers: Arc<Mutex<HashMap<u64, FiberHandle>>> = Arc::default();
    let consumers: Arc<Mutex<Vec<FiberHandle>>> = Arc::default();

    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

    // Churner: after each disposal holds a quiesced no-provider window in
    // which no consumer may be (or remain) Active.
    {
        let ctx = ctx.clone();
        let violations = violations.clone();
        let consumers = consumers.clone();
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            for round in 0..CHURN_ROUNDS {
                let providers = providers.clone();
                let provider = ctx
                    .component(
                        move |ctx: &Context, _: &Value| -> ComponentResult {
                            let id = ctx.fiber().id().unwrap_or(u64::MAX);
                            providers.lock().unwrap().insert(id, ctx.fiber());
                            ctx.provide("database", Arc::new(json!({ "provider": id })))?;
                            Ok(())
                        },
                        json!(null),
                    )
                    .expect("install provider");
                assert!(
                    provider.wait_for(FiberState::Active, Duration::from_secs(10)),
                    "provider must activate, round {round}"
                );
                std::thread::sleep(Duration::from_millis(2));

                provider.dispose();
                assert!(
                    poll_until(|| !ctx.has("database"), Duration::from_secs(5)),
                    "withdrawal must land, round {round}"
                );
                ctx.quiesce();
                let snapshot = consumers.lock().unwrap().clone();
                for consumer in &snapshot {
                    if consumer.state() == FiberState::Active {
                        violations.lock().unwrap().push(format!(
                            "fiber {} stayed Active while its required dependency was absent (stale replay?), round {round}",
                            consumer.name().unwrap_or_default()
                        ));
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            done_tx.send(()).expect("report done");
        });
    }

    for _ in 0..CONSUMER_THREADS {
        let ctx = ctx.clone();
        let violations = violations.clone();
        let consumers = consumers.clone();
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            for _ in 0..CONSUMER_ROUNDS {
                let consumer = ctx
                    .inject(Inject::required(&["database"]), |_ctx: &Context| {
                        // Mid-body absence is a legal race (the provider
                        // may vanish between revalidate and body end);
                        // the queued Unload corrects it. Only a *settled*
                        // Active-without-dependency is a bug.
                        Ok(())
                    })
                    .expect("install consumer");
                consumers.lock().unwrap().push(consumer.clone());
                if !consumer.wait_for(FiberState::Active, Duration::from_secs(10)) {
                    violations
                        .lock()
                        .unwrap()
                        .push("consumer never activated against a churning provider".to_owned());
                } else {
                    // Either the dependency stays present or the cascade demotes us.
                    let ok = poll_until(
                        || ctx.has("database") || consumer.state() == FiberState::Pending,
                        Duration::from_millis(500),
                    );
                    if !ok {
                        violations.lock().unwrap().push(format!(
                            "consumer {} settled Active without its dependency",
                            consumer.name().unwrap_or_default()
                        ));
                    }
                }
                consumer.dispose();
            }
            done_tx.send(()).expect("report done");
        });
    }
    drop(done_tx);

    for _ in 0..(1 + CONSUMER_THREADS) {
        done_rx
            .recv_timeout(WATCHDOG)
            .expect("threads deadlocked or lost wakeups");
    }

    assert!(
        violations.lock().unwrap().is_empty(),
        "{:?}",
        *violations.lock().unwrap()
    );
    ctx.quiesce();
    assert!(!ctx.has("database"));
    assert!(ctx.provided_services().is_empty());
}
