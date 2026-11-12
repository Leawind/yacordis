//! Concurrency stress for the work substrate: the drain-before-rollback
//! protocol under cross-thread contention.
//!
//! Eight threads run install → spawn-works → dispose cycles, each fiber
//! starting blocking units (which ignore their token) plus futures
//! (which park until aborted). If any drain leaks a unit past rollback,
//! a completion fires after the fiber is gone, or a lock is held across
//! user code (deadlock), the watchdog fails the run.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use cordis::prelude::*;
use serde_json::{Value, json};

const THREADS: usize = 8;
const CYCLES: usize = 25;
const WATCHDOG: Duration = Duration::from_secs(120);

#[test]
fn concurrent_work_spawn_drain_cycles() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    // Late deliveries: completions that must never fire (delivered only
    // while the fiber is alive). Any nonzero count after the runs means
    // a drain leaked past rollback.
    let late = Arc::new(AtomicU64::new(0));
    let completed = Arc::new(AtomicU64::new(0));

    let (done_tx, done_rx) = mpsc::channel::<()>();
    for thread in 0..THREADS {
        let ctx = ctx.clone();
        let late = late.clone();
        let completed = completed.clone();
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            // Each thread isolates its probe service, mirroring stress.rs:
            // per-thread assertions stay deterministic while the shared
            // registry and work substrate stay under contention.
            let group = ctx.isolate("probe");
            for cycle in 0..CYCLES {
                let late = late.clone();
                let completed = completed.clone();
                let worker = group
                    .component(
                        move |ctx: &Context, _: &Value| -> ComponentResult {
                            let (late, completed) = (late.clone(), completed.clone());
                            // Liveness marker: provide-disposers run AFTER
                            // the drain phase, so completions delivered
                            // during a correct drain still see it; a leak
                            // past rollback does not.
                            ctx.provide(
                                "probe",
                                Arc::new(json!({ "thread": thread, "cycle": cycle })),
                            )?;
                            // A stubborn unit: ignores its token; dispose
                            // must block until it returns.
                            ctx.run_blocking(
                                "stubborn",
                                move |_| {
                                    std::thread::sleep(Duration::from_millis(5));
                                    Ok(json!({ "thread": thread }))
                                },
                                {
                                    let (late, completed) = (late.clone(), completed.clone());
                                    move |ctx, _| {
                                        completed.fetch_add(1, Ordering::Relaxed);
                                        if ctx.get::<Value>("probe").is_none() {
                                            late.fetch_add(1, Ordering::Relaxed);
                                        }
                                    }
                                },
                            )
                            .map_err(ComponentError::new)?;
                            // A parked future: aborted at the await point
                            // by the drain phase.
                            ctx.spawn_future(
                                "parked",
                                std::future::pending::<Result<Value, ComponentError>>(),
                                move |ctx, _| {
                                    completed.fetch_add(1, Ordering::Relaxed);
                                    if ctx.get::<Value>("probe").is_none() {
                                        late.fetch_add(1, Ordering::Relaxed);
                                    }
                                },
                            )
                            .map_err(ComponentError::new)?;
                            Ok(())
                        },
                        json!(null),
                    )
                    .expect("install worker");

                worker.dispose();
            }
            done_tx.send(()).expect("report done");
        });
    }
    drop(done_tx);

    for _ in 0..THREADS {
        done_rx
            .recv_timeout(WATCHDOG)
            .expect("work substrate deadlocked under contention");
    }

    // Every unit completed exactly once (delivery, not drop: fibers
    // stayed alive through their own dispose's drain phase).
    let total = (THREADS * CYCLES * 2) as u64;
    let done = completed.load(Ordering::Relaxed);
    assert_eq!(
        done, total,
        "each unit delivers exactly one completion ({done}/{total})"
    );
    // And none of them leaked past a rollback boundary.
    assert_eq!(
        late.load(Ordering::Relaxed),
        0,
        "completions must observe a live fiber"
    );
}
