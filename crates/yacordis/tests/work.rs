//! Framework-mediated concurrency (§4.1 I1): work units are framework
//! plumbing — registered as effects, drained (cancel + join) before
//! rollback, completion delivered as framework-initiated calls. These
//! tests lock the drain-before-rollback protocol and the executor
//! lifecycle.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context as TaskContext, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use yacordis::prelude::*;
use serde_json::{Value, json};

fn outcome_tag(outcome: &WorkOutcome) -> &'static str {
    match outcome {
        WorkOutcome::Done(_) => "done",
        WorkOutcome::Cancelled => "cancelled",
        WorkOutcome::Failed(_) => "failed",
        WorkOutcome::Panicked(_) => "panicked",
    }
}

#[test]
fn blocking_work_completes_and_delivers() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<String>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.run_blocking(
                    "adder",
                    |_| Ok(json!(21)),
                    move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome).to_owned());
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    let tag = rx.recv_timeout(Duration::from_secs(2)).expect("completion");
    assert_eq!(tag, "done");
    worker.dispose();
}

#[test]
fn completion_callback_can_use_the_framework() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<Value>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.on("work/done", {
                    let tx = tx.clone();
                    move |_, args: &Value| {
                        let _ = tx.send(args.clone());
                        None
                    }
                })?;
                ctx.run_blocking(
                    "announce",
                    |_| Ok(json!("payload")),
                    move |ctx, outcome| {
                        if let WorkOutcome::Done(value) = outcome {
                            ctx.emit("work/done", &json!({ "result": value }));
                        }
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    let event = rx.recv_timeout(Duration::from_secs(2)).expect("event");
    assert_eq!(event, json!({ "result": "payload" }));
    worker.dispose();
}

#[test]
fn unload_joins_in_flight_work_before_rollback() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                // Ignores the token: unload must block until the unit
                // returns, then deliver Cancelled (work finished, fiber
                // cancelled). The sleep starts before `component()` returns
                // (install quiesces against the global op counter, which
                // parallel tests may delay further), so the lower bound
                // on dispose latency stays well under the sleep.
                let tx = tx.clone();
                ctx.run_blocking(
                    "stubborn",
                    |_| {
                        thread::sleep(Duration::from_millis(250));
                        Ok(json!(null))
                    },
                    move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome));
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    let start = Instant::now();
    worker.dispose();
    assert!(
        start.elapsed() >= Duration::from_millis(100),
        "dispose must join the in-flight unit (elapsed {:?})",
        start.elapsed()
    );
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("cancelled"));
}

#[test]
fn work_from_a_failing_body_is_drained() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.run_blocking(
                    "doomed",
                    |_| {
                        thread::sleep(Duration::from_millis(40));
                        Ok(json!(null))
                    },
                    move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome));
                    },
                )?;
                // The body fails after starting the unit: drain must run
                // before the eager rollback, with the fiber still alive.
                Err(ComponentError::new("body fails"))
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(worker.state(), FiberState::Failed);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("cancelled"));
}

#[test]
fn explicit_cancel_is_cooperative() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                let handle = ctx.run_blocking(
                    "cooperative",
                    |token| {
                        while !token.is_cancelled() {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Ok(json!(null))
                    },
                    move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome));
                    },
                )?;
                handle.cancel();
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("cancelled"));
    worker.dispose();
}

#[test]
fn future_completes_on_the_executor() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<Value>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.spawn_future(
                    "ready",
                    async { Ok(json!("from future")) },
                    move |_, outcome| {
                        if let WorkOutcome::Done(value) = outcome {
                            let _ = tx.send(value);
                        }
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    let value = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("future value");
    assert_eq!(value, json!("from future"));
    worker.dispose();
}

#[test]
fn future_progresses_via_waker() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<Value>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let (waker_tx, waker_rx) = mpsc::channel::<Waker>();
                let polls = Arc::new(AtomicUsize::new(0));
                let polls_for_future = polls.clone();

                // Yields once, then completes when woken.
                let fut = std::future::poll_fn(move |cx: &mut TaskContext<'_>| {
                    let n = polls_for_future.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        let _ = waker_tx.send(cx.waker().clone());
                        Poll::Pending
                    } else {
                        Poll::Ready(Ok(json!("woken")))
                    }
                });

                // Wake it from a foreign thread after a short delay.
                thread::spawn(move || {
                    let waker = waker_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("waker");
                    thread::sleep(Duration::from_millis(30));
                    waker.wake();
                });

                let tx = tx.clone();
                ctx.spawn_future("waker", fut, move |_, outcome| {
                    if let WorkOutcome::Done(value) = outcome {
                        let _ = tx.send(value);
                    }
                })?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    let value = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("woken value");
    assert_eq!(value, json!("woken"));
    worker.dispose();
}

#[test]
fn unload_aborts_a_pending_future() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.spawn_future(
                    "never",
                    std::future::pending::<Result<Value, ComponentError>>(),
                    move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome));
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    // Dispose must not hang on the never-completing future: the abort
    // drops it at the await point.
    worker.dispose();
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("cancelled"));
}

#[test]
fn on_done_disposing_its_own_fiber_does_not_deadlock() {
    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let tx = tx.clone();
                ctx.run_blocking(
                    "self-dispose",
                    |_| Ok(json!(null)),
                    move |ctx, _| {
                        let _ = tx.send("in-callback");
                        // Drains this fiber's own unit from inside its
                        // completion: the join must skip the unit
                        // executing on this thread instead of waiting
                        // for itself.
                        ctx.dispose_all();
                        let _ = tx.send("disposed");
                    },
                )?;
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("in-callback"));
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("disposed"));
    assert_eq!(worker.state(), FiberState::Disposed);
}

#[test]
fn double_wake_delivers_exactly_one_completion() {
    use std::sync::atomic::AtomicUsize;

    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();

    let worker = ctx
        .component(
            move |ctx: &Context, _: &Value| -> ComponentResult {
                let (waker_tx, waker_rx) = mpsc::channel::<Waker>();
                let polls = Arc::new(AtomicUsize::new(0));
                let polls_for_future = polls.clone();

                // Yields once; the second poll completes it.
                let fut = std::future::poll_fn(move |cx: &mut TaskContext<'_>| {
                    let n = polls_for_future.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        let _ = waker_tx.send(cx.waker().clone());
                        Poll::Pending
                    } else {
                        Poll::Ready(Ok(json!("ready")))
                    }
                });

                let tx = tx.clone();
                ctx.spawn_future("double-wake", fut, move |_, outcome| {
                    let _ = tx.send(outcome_tag(&outcome));
                })?;

                // Wake twice after completion is reachable: the second
                // enqueue polls a consumed future and must be a no-op.
                thread::spawn(move || {
                    let waker = waker_rx
                        .recv_timeout(Duration::from_secs(2))
                        .expect("waker");
                    thread::sleep(Duration::from_millis(20));
                    waker.wake_by_ref();
                    waker.wake();
                });
                Ok(())
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok("done"));
    // Allow any stray duplicate delivery to surface.
    thread::sleep(Duration::from_millis(100));
    assert!(
        rx.try_recv().is_err(),
        "no duplicate completion after a double wake"
    );
    worker.dispose();
}

#[test]
fn abort_before_first_poll_never_runs_the_future() {
    use std::sync::atomic::AtomicUsize;

    let ctx = Context::new();
    let (tx, rx) = mpsc::channel::<&'static str>();
    let polls = Arc::new(AtomicUsize::new(0));

    let worker = ctx
        .component(
            {
                let polls_body = polls.clone();
                move |ctx: &Context, _: &Value| -> ComponentResult {
                    let polls_for_future = polls_body.clone();
                    let tx = tx.clone();
                    let fut = std::future::poll_fn(move |_: &mut TaskContext<'_>| {
                        polls_for_future.fetch_add(1, Ordering::SeqCst);
                        Poll::Pending::<Result<Value, ComponentError>>
                    });
                    let handle = ctx.spawn_future("never-polled", fut, move |_, outcome| {
                        let _ = tx.send(outcome_tag(&outcome));
                    })?;
                    // Cancel before the executor can schedule the first poll.
                    handle.cancel();
                    Ok(())
                }
            },
            json!(null),
        )
        .expect("install worker");

    assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok("cancelled"));
    // Give a hypothetical buggy executor ample time to poll anyway.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "an aborted-before-first-poll future must never run"
    );
    worker.dispose();
}
