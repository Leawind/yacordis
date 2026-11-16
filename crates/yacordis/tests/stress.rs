//! Concurrency stress: the fiber scheduler's lost-wakeup regression test.
//!
//! Eight threads drive install/consume/dispose cycles against one shared
//! runtime, each in its own isolation group (so per-thread assertions
//! stay deterministic while every lock, queue and registry in the core
//! is under cross-thread contention). A lost wakeup, a deadlock or a
//! corrupt notify cascade fails the test via the watchdog.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};
use yacordis::prelude::*;

const THREADS: usize = 8;
const CYCLES: usize = 40;
const WATCHDOG: Duration = Duration::from_secs(120);

#[test]
fn concurrent_install_dispose_cycles_across_isolation_groups() {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));

    // Shared listener + event traffic, so the hook table is also under contention.
    let pings = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ping_counter = Arc::clone(&pings);
    ctx.on("stress/ping", move |_, _| {
        ping_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        None
    })
    .expect("on");

    let (done_tx, done_rx) = mpsc::channel::<()>();
    for thread in 0..THREADS {
        let ctx = ctx.clone();
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            // Per-thread isolation of the service name keeps assertions
            // deterministic while shared locks stay under contention.
            let group = ctx.isolate("database");
            for cycle in 0..CYCLES {
                let provider = group
                    .component_dyn(
                        Arc::new(move |ctx: &Context, _: &Value| -> ComponentResult {
                            ctx.provide(
                                "database",
                                Arc::new(json!({ "thread": thread, "cycle": cycle })),
                            )?;
                            Ok(())
                        }) as Arc<dyn Component>,
                        json!(null),
                    )
                    .expect("install provider");

                let consumer = group
                    .inject(Inject::required(&["database"]), |ctx: &Context| {
                        assert!(
                            ctx.get::<Value>("database").is_some(),
                            "required dependency must be visible inside the group"
                        );
                        Ok(())
                    })
                    .expect("install consumer");
                assert!(
                    consumer.wait_for(FiberState::Active, std::time::Duration::from_secs(10)),
                    "consumer must activate, cycle {cycle}"
                );

                ctx.emit("stress/ping", &json!({ "thread": thread }));

                provider.dispose();
                // Cascaded deactivation is eventually-consistent under
                // concurrent lifecycle activity; assert the settled state.
                let mut settled = false;
                for _ in 0..20_000 {
                    if consumer.state() == FiberState::Pending {
                        settled = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
                assert!(settled, "consumer must settle to Pending, cycle {cycle}");
                consumer.dispose();
            }
            done_tx.send(()).expect("report done");
        });
    }
    drop(done_tx);

    for _ in 0..THREADS {
        done_rx
            .recv_timeout(WATCHDOG)
            .expect("threads deadlocked or lost wakeups");
    }

    assert!(
        pings.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "listener under contention must have fired"
    );
    ctx.quiesce();
    assert!(
        ctx.provided_services().is_empty(),
        "{:?}",
        ctx.provided_services()
    );
    assert!(!ctx.has("database"));
}
