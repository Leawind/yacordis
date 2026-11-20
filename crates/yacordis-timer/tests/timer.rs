use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yacordis::prelude::*;
use yacordis_timer::{CancelHandle, SleepOutcome, TimerComponent, TimerContextExt};

fn install_timer() -> (Context, yacordis::FiberHandle) {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    let handle = ctx
        .component(TimerComponent, json!(null))
        .expect("install timer");
    (ctx, handle)
}

/// Poll `check` until it returns true so timing assertions stay robust on slow machines.
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
                // A deadline-based check races wheel tick drift (Windows timer coalescing);
                // cancelling inside the tick that reaches the count is deterministic.
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

    let mut ticks = 0;
    while ticks < 3 {
        rx.recv_timeout(Duration::from_secs(2)).expect("tick");
        ticks += 1;
    }
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
    assert!(ctx.set_timeout(Duration::from_millis(1), || {}).is_err());
}

#[test]
fn effect_failure_leaves_nothing_scheduled() {
    let (ctx, timer_fiber) = install_timer();

    // A disposer runs while its own fiber is unloading: the timer service
    // is reachable, but registering an effect there fails. Scheduling must
    // not have happened either, so nothing fires afterwards.
    let (tx, rx) = mpsc::channel::<&'static str>();
    let victim = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let ctx = ctx.clone();
                let tx = tx.clone();
                let ctx_for_disposer = ctx.clone();
                ctx.effect("victim", move |_| {
                    let disposer: Disposer = Box::new(move || {
                        let _ =
                            ctx_for_disposer.set_timeout(Duration::from_millis(50), move || {
                                let _ = tx.send("orphan fired");
                            });
                    });
                    Ok(Some(disposer))
                })
                .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install victim");

    victim.dispose();
    assert!(!wait_until(Duration::from_millis(400), || rx
        .try_recv()
        .is_ok()));
    timer_fiber.dispose();
}

#[test]
fn unload_waits_for_an_inflight_callback_before_rollback() {
    let (ctx, timer_fiber) = install_timer();

    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let rollback_ran = Arc::new(AtomicBool::new(false));

    let started_for_body = started.clone();
    let started_for_callback = started.clone();
    let finished_for_callback = finished.clone();
    let rollback_for_body = rollback_ran.clone();

    let victim = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                // Registered first: its disposer runs after the timer's
                // during the reverse-order rollback.
                let flag = rollback_for_body.clone();
                ctx.effect("rollback-marker", move |_| {
                    let disposer: Disposer = Box::new(move || {
                        flag.store(true, Ordering::SeqCst);
                    });
                    Ok(Some(disposer))
                })
                .map_err(ComponentError::new)?;
                let started_cb = started_for_callback.clone();
                let finished_cb = finished_for_callback.clone();
                let started_body = started_for_body.clone();
                ctx.set_interval(Duration::from_millis(20), move || {
                    if !started_cb.swap(true, Ordering::SeqCst) {
                        // First tick: stay in flight for a bit.
                        std::thread::sleep(Duration::from_millis(150));
                        finished_cb.store(true, Ordering::SeqCst);
                    }
                    let _ = started_body.load(Ordering::SeqCst);
                })
                .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install victim");

    // Precondition: the callback is executing right now.
    assert!(
        wait_until(Duration::from_secs(2), || started.load(Ordering::SeqCst)),
        "callback should be in flight"
    );

    let dispose_started = Instant::now();
    victim.dispose();
    let dispose_elapsed = dispose_started.elapsed();

    assert!(
        finished.load(Ordering::SeqCst),
        "dispose must wait for the in-flight callback to return"
    );
    assert!(
        rollback_ran.load(Ordering::SeqCst),
        "rollback ran after the callback finished"
    );
    assert!(
        dispose_elapsed >= Duration::from_millis(100),
        "dispose blocked until the callback finished ({dispose_elapsed:?})"
    );
    timer_fiber.dispose();
}

#[test]
fn a_panicking_callback_neither_kills_the_wheel_nor_wedges_unload() {
    let (ctx, timer_fiber) = install_timer();
    let (entered_tx, entered_rx) = mpsc::channel::<()>();

    let victim = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let entered = entered_tx.clone();
                ctx.set_timeout(Duration::from_millis(10), move || {
                    let _ = entered.send(());
                    panic!("timer callback boom");
                })
                .map_err(ComponentError::new)?;
                Ok(())
            },
            json!(null),
        )
        .expect("install victim");

    assert_eq!(
        entered_rx.recv_timeout(Duration::from_secs(2)),
        Ok(()),
        "the panicking callback should have run"
    );
    // Let the unwind finish before unloading: the regression was that a
    // panicked callback left both its mutex poisoned and the in-flight
    // flag set.
    std::thread::sleep(Duration::from_millis(50));

    // The wheel thread survived: timers registered afterwards still fire.
    let (tx, rx) = mpsc::channel::<()>();
    ctx.set_timeout(Duration::from_millis(20), move || {
        let _ = tx.send(());
    })
    .expect("schedule after the panic");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(()),
        "the wheel thread died with the panicking callback"
    );

    // Unloading the fiber that owned the panic must not hang on a stuck
    // in-flight flag.
    let (done_tx, done_rx) = mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let _ = done_tx.send(victim.dispose());
    });
    assert_eq!(
        done_rx.recv_timeout(Duration::from_secs(5)),
        Ok(true),
        "dispose hung on a panicked callback"
    );
    timer_fiber.dispose();
}
