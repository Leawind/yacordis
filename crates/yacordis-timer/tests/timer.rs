//! Timer service tests: fire-once, intervals, scope-release cancellation.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use yacordis::prelude::*;
use yacordis_timer::{CancelHandle, SleepOutcome, TimerComponent, TimerContextExt};
use serde_json::{Value, json};

fn install_timer() -> (Context, yacordis::FiberHandle) {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    let handle = ctx
        .component(TimerComponent, json!(null))
        .expect("install timer");
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
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.set_timeout(Duration::from_millis(30), move || {
                    tx.send("fired").unwrap();
                })
                .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok("fired"));
    worker.dispose();
}

#[test]
fn unloading_the_fiber_cancels_pending_timeouts() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.set_timeout(Duration::from_millis(500), move || {
                    tx.send("must not fire").unwrap();
                })
                .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    worker.dispose();
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
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                let handle = ctx
                    .set_timeout(Duration::from_millis(400), move || {
                        tx.send("fired").unwrap();
                    })
                    .map_err(ComponentError::new)?;
                handle.cancel();
                assert!(!handle.is_pending());
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    worker.dispose();
    assert!(!wait_until(Duration::from_millis(700), || rx
        .try_recv()
        .is_ok()));
}

#[test]
fn interval_fires_repeatedly_until_cancelled() {
    let (ctx, _timer_fiber) = install_timer();
    let (tx, rx) = mpsc::channel::<u32>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                let counter_for_callback = Arc::new(std::sync::atomic::AtomicU32::new(0));
                // Cancel on the third tick, from inside the callback. A
                // deadline-based check (setTimeout + counter >= 3) races
                // the wheel's tick drift — Windows timer coalescing can
                // push the third tick past the deadline, the condition
                // never fires, and the interval runs forever. Cancelling
                // inside the tick that reaches the count is
                // deterministic: the wheel re-arms before running but
                // re-checks `cancelled` before invoking, so the next
                // tick is dropped.
                let slot: Arc<Mutex<Option<CancelHandle>>> = Arc::default();
                let slot_for_callback = Arc::clone(&slot);
                let handle = ctx
                    .set_interval(Duration::from_millis(20), move || {
                        let n =
                            counter_for_callback.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tx.send(n).unwrap();
                        if n + 1 >= 3 {
                            if let Some(stopper) = slot_for_callback.lock().unwrap().as_ref() {
                                stopper.cancel();
                            }
                        }
                    })
                    .map_err(ComponentError::new)?;
                *slot.lock().unwrap() = Some(handle);
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
    worker.dispose();
}

#[test]
fn sleep_elapses_and_unload_wakes_it_early() {
    let (ctx, timer_fiber) = install_timer();

    let start = Instant::now();
    let outcome = ctx.sleep(Duration::from_millis(40)).expect("sleep");
    assert_eq!(outcome, SleepOutcome::Elapsed);
    assert!(start.elapsed() >= Duration::from_millis(35));

    // A worker thread (spawned by a component body) sleeps on the fiber's
    // behalf; unloading the fiber wakes it with `Cancelled`.
    let (tx, rx) = mpsc::channel::<SleepOutcome>();
    let sleeper = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
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
    sleeper.dispose();
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
    timer_fiber.dispose();
}

#[test]
fn disposing_the_timer_component_shuts_the_wheel_down() {
    let (ctx, timer_fiber) = install_timer();
    assert!(ctx.has(yacordis_timer::TIMER_SERVICE));
    timer_fiber.dispose();
    assert!(!ctx.has(yacordis_timer::TIMER_SERVICE));
    // New timers fail cleanly instead of silently doing nothing.
    assert!(ctx.set_timeout(Duration::from_millis(1), || {}).is_err());
}
