//! Fiber ids must be process-global, not per runtime.
//!
//! The loader's plugin-facing handle table is process-global and audits
//! context handles by fiber id when a plugin fiber is disposed. If two
//! runtimes in one process both minted ids from 1, one runtime's dispose
//! audit would collect the other runtime's live context handles, and the
//! victim's next plugin call would fail with `run_blocking failed` (the
//! regression seen as a flaky e2e under parallel test threads).

use cordis::prelude::*;
use serde_json::{Value, json};

#[test]
fn fiber_ids_are_unique_across_runtimes() {
    let first = Context::new();
    let second = Context::new();
    let noop = |_ctx: &Context, _config: &Value| -> PluginResult { Ok(()) };

    let a = first.plugin(noop, json!(null)).expect("install first");
    let b = second
        .plugin(
            |_ctx: &Context, _config: &Value| -> PluginResult { Ok(()) },
            json!(null),
        )
        .expect("install second");

    assert_ne!(
        a.id(),
        b.id(),
        "two runtimes minted the same fiber id; the loader's global \
         handle audit would collect the other runtime's live contexts"
    );
}
