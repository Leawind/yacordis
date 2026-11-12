//! Listener removal is honored inside an active dispatch (cordis TS
//! #242): a listener removed mid-dispatch — by an earlier listener in the
//! same cycle — must not run in that cycle, and `bail` continues past it
//! to later listeners instead of short-circuiting on it.

use std::sync::{Arc, Mutex};

use yacordis::prelude::*;
use serde_json::{Value, json};

fn record(log: &Arc<Mutex<Vec<&'static str>>>, tag: &'static str) {
    log.lock().unwrap().push(tag);
}

#[test]
fn emit_skips_listener_removed_mid_dispatch() {
    let ctx = Context::new();
    let fired: Arc<Mutex<Vec<&'static str>>> = Arc::default();

    let b_handle = ctx
        .on("evt", {
            let fired = fired.clone();
            move |_, _: &Value| {
                record(&fired, "b");
                None
            }
        })
        .expect("register b");

    ctx.on_prepend("evt", {
        let fired = fired.clone();
        let b_handle = b_handle.clone();
        move |_, _: &Value| {
            record(&fired, "a");
            b_handle.dispose();
            None
        }
    })
    .expect("register a (prepended so it runs before b)");

    ctx.on("evt", {
        let fired = fired.clone();
        move |_, _: &Value| {
            record(&fired, "c");
            None
        }
    })
    .expect("register c");

    ctx.emit("evt", &json!(null));
    assert_eq!(*fired.lock().unwrap(), ["a", "c"]);

    // The removal persists: later dispatches stay clean.
    ctx.emit("evt", &json!(null));
    assert_eq!(*fired.lock().unwrap(), ["a", "c", "a", "c"]);
}

#[test]
fn bail_continues_past_listener_removed_mid_dispatch() {
    let ctx = Context::new();

    let b_handle = ctx
        .on("evt", |_, _: &Value| Some(json!("b")))
        .expect("register b");
    ctx.on_prepend("evt", {
        let b_handle = b_handle.clone();
        move |_, _: &Value| {
            b_handle.dispose();
            None
        }
    })
    .expect("register a (prepended so it runs before b)");
    ctx.on("evt", |_, _: &Value| Some(json!("c")))
        .expect("register c");

    let ret = ctx.bail("evt", &json!(null));
    assert_eq!(ret, Some(json!("c")));
}

#[test]
fn prepend_listener_removed_mid_dispatch_is_skipped() {
    let ctx = Context::new();
    let fired: Arc<Mutex<Vec<&'static str>>> = Arc::default();

    // `prepend` puts the probe at the head: it runs first and removes
    // the regular listener registered before it in source order.
    let b_handle = ctx
        .on("evt", {
            let fired = fired.clone();
            move |_, _: &Value| {
                record(&fired, "b");
                None
            }
        })
        .expect("register b");
    ctx.on_prepend("evt", {
        let b_handle = b_handle.clone();
        move |_, _: &Value| {
            b_handle.dispose();
            None
        }
    })
    .expect("prepend probe");

    ctx.emit("evt", &json!(null));
    assert!(
        fired.lock().unwrap().is_empty(),
        "removed listener must not run in the same dispatch"
    );
}
