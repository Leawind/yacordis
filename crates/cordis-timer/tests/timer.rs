//! Timer service tests: fire-once, intervals, scope-release cancellation.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use cordis::prelude::*;
use cordis_timer::{SleepOutcome, TimerContextExt, TimerPlugin};
use serde_json::{Value, json};

fn install_timer() -> (Context, cordis::FiberHandle) {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));
    let handle = ctx.plugin(TimerPlugin, json!(null)).expect("install timer");
    (ctx, handle)
}

/// Poll `check` until it returns true (or the budget expires) so timing
/// assertions stay robust on slow machines.
fn wait_until(budget: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < budget {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    check()
}

#[test]
fn timeout_fires_once() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel();

    let worker = ctx
        .plugin(
            move |ctx: &Context, _: &Value| -> PluginResult {
                let tx = tx.clone();
                ctx.set_timeout(Duration::from_millis(30), move || {
                    tx.send("fired").unwrap();
                })
                .map_err(PluginError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok("fired"));
    worker.dispose().expect("dispose");
}

#[test]
fn unloading_the_fiber_cancels_pending_timeouts() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .plugin(
            move |ctx: &Context, _: &Value| -> PluginResult {
                let tx = tx.clone();
                ctx.set_timeout(Duration::from_millis(500), move || {
                    tx.send("must not fire").unwrap();
                })
                .map_err(PluginError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    worker.dispose().expect("dispose before firing");
    // Give the wheel ample time to (wrongly) fire if cancellation broke.
    assert!(!wait_until(Duration::from_millis(900), || rx
        .try_recv()
        .is_ok()));
}

#[test]
fn manual_cancel_works() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .plugin(
            move |ctx: &Context, _: &Value| -> PluginResult {
                let tx = tx.clone();
                let handle = ctx
                    .set_timeout(Duration::from_millis(400), move || {
                        tx.send("fired").unwrap();
                    })
                    .map_err(PluginError::new)?;
                handle.cancel();
                assert!(!handle.is_pending());
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    worker.dispose().expect("dispose");
    assert!(!wait_until(Duration::from_millis(700), || rx
        .try_recv()
        .is_ok()));
}

#[test]
fn interval_fires_repeatedly_until_cancelled() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel::<u32>();

    let worker = ctx
        .plugin(
            move |ctx: &Context, _: &Value| -> PluginResult {
                let tx = tx.clone();
                let counter = Arc::new(std::sync::atomic::AtomicU32::new(0));
                let counter_for_callback = Arc::clone(&counter);
                let handle = ctx
                    .set_interval(Duration::from_millis(20), move || {
                        let n =
                            counter_for_callback.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tx.send(n).unwrap();
                    })
                    .map_err(PluginError::new)?;
                // Cancel from inside the callback closure context: stop
                // after three ticks by cancelling on the third send.
                let counter2 = Arc::clone(&counter);
                let stopper = handle.clone();
                ctx.set_timeout(Duration::from_millis(75), move || {
                    if counter2.load(std::sync::atomic::Ordering::SeqCst) >= 3 {
                        stopper.cancel();
                    }
                })
                .map_err(PluginError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    // At least three ticks arrive.
    let mut ticks = 0;
    while ticks < 3 {
        rx.recv_timeout(Duration::from_secs(2)).expect("tick");
        ticks += 1;
    }
    // After cancellation, no further tick within a generous window.
    assert!(!wait_until(Duration::from_millis(300), || rx
        .try_recv()
        .is_ok()));
    worker.dispose().expect("dispose");
}

#[test]
fn sleep_elapses_and_unload_wakes_it_early() {
    let (ctx, timer_fiber) = install_timer();

    let start = Instant::now();
    let outcome = ctx.sleep(Duration::from_millis(40)).expect("sleep");
    assert_eq!(outcome, SleepOutcome::Elapsed);
    assert!(start.elapsed() >= Duration::from_millis(35));

    // A worker thread (spawned by a plugin body) sleeps on the fiber's
    // behalf; unloading the fiber wakes it with `Cancelled`.
    let (tx, rx) = mpsc::channel::<SleepOutcome>();
    let sleeper = ctx
        .plugin(
            move |ctx: &Context, _: &Value| -> PluginResult {
                let ctx = ctx.clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let outcome = ctx.sleep(Duration::from_secs(30)).expect("sleep");
                    tx.send(outcome).expect("report outcome");
                });
                Ok(())
            },
            json!(null),
        )
        .expect("install sleeper");

    // Let the worker enter its sleep, then unload the fiber.
    std::thread::sleep(Duration::from_millis(80));
    let start = Instant::now();
    sleeper.dispose().expect("dispose sleeper");
    let outcome = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("sleep woke up");
    assert_eq!(
        outcome,
        SleepOutcome::Cancelled,
        "unload woke the sleeper early"
    );
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "wakeup must be prompt"
    );
    timer_fiber.dispose().expect("dispose timer");
}

#[test]
fn disposing_the_timer_plugin_shuts_the_wheel_down() {
    let (ctx, timer_fiber) = install_timer();
    assert!(ctx.has(cordis_timer::TIMER_SERVICE));
    timer_fiber.dispose().expect("dispose timer");
    assert!(!ctx.has(cordis_timer::TIMER_SERVICE));
    // New timers fail cleanly instead of silently doing nothing.
    assert!(ctx.set_timeout(Duration::from_millis(1), || {}).is_err());
}
