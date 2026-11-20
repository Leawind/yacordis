//! Lifecycle residue: a dropped runtime, a disposed fiber and a retired
//! cascade entry must not stay alive behind the caller's back.
//!
//! The lifecycle this pins down:
//!
//! - `Context::new()` + drop: the runtime released its root, so the core
//!   is collectable (a `Context` stored on the fiber would make
//!   `Core -> Fiber -> Context -> Core` an uncollectable cycle);
//! - `component_dyn` + `dispose`: the fiber is released, and the parent's
//!   cascade entry retires with it instead of accumulating one spent
//!   effect per install.

use std::sync::Arc;

use serde_json::{Value, json};
use yacordis::prelude::*;

struct Native;

impl Component for Native {
    fn name(&self) -> Option<&str> {
        Some("native")
    }

    fn apply(&self, _ctx: &Context, _config: &Value) -> ComponentResult {
        Ok(())
    }
}

#[test]
fn the_lifecycle_returns_to_its_baseline() {
    // Warm up: first-use allocations (lazy registries, thread-locals) are
    // not residue.
    for _ in 0..10 {
        let ctx = Context::new();
        let handle = ctx
            .component_dyn(Arc::new(Native), json!({}))
            .expect("warm-up install");
        handle.dispose();
    }

    let ctx = Context::new();
    let (cores, fibers) = yacordis::test_support::live_objects();

    for _ in 0..500 {
        let dropped = Context::new();
        std::hint::black_box(&dropped);
    }
    assert_eq!(
        yacordis::test_support::live_objects(),
        (cores, fibers),
        "a dropped runtime leaves no core or root fiber behind"
    );

    for _ in 0..500 {
        let handle = ctx
            .component_dyn(Arc::new(Native), json!({}))
            .expect("install");
        handle.dispose();
    }
    assert_eq!(
        yacordis::test_support::live_objects(),
        (cores, fibers),
        "a disposed fiber is released"
    );
    assert!(
        yacordis::test_support::effect_batches(&ctx) <= 1,
        "a disposed child retires its cascade entry on the parent"
    );
}
