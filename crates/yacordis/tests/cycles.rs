//! Counterpart of cordis TS issue #66 (a declared-provides loop pends
//! forever, silently). A group is reported only when every edge points
//! at a *missing* dependency; a pending fiber behind a healthy provider
//! is never part of a report.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use yacordis::prelude::*;

#[derive(Default, Clone)]
struct Capture(Arc<Mutex<Vec<String>>>);

impl Capture {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Logger for Capture {
    fn log(&self, level: LogLevel, target: &str, message: &str) {
        self.0
            .lock()
            .unwrap()
            .push(format!("{level} {target} {message}"));
    }
}

struct Declared {
    name: &'static str,
    inject: Inject,
    provides: Vec<&'static str>,
}

impl Component for Declared {
    fn name(&self) -> Option<&str> {
        Some(self.name)
    }
    fn inject(&self) -> Inject {
        self.inject.clone()
    }
    fn provides(&self) -> Vec<String> {
        self.provides.iter().map(|s| (*s).to_owned()).collect()
    }
    fn apply(&self, _ctx: &Context, _config: &Value) -> ComponentResult {
        Ok(())
    }
}

#[test]
fn declared_cycle_is_reported_once_and_clears_when_broken() {
    let ctx = Context::new();
    let capture = Capture::default();
    ctx.set_logger(Arc::new(capture.clone()));

    let a = ctx
        .component(
            Declared {
                name: "A",
                inject: Inject::required(&["b"]),
                provides: vec!["a"],
            },
            json!(null),
        )
        .expect("install A");
    let b = ctx
        .component(
            Declared {
                name: "B",
                inject: Inject::required(&["c"]),
                provides: vec!["b"],
            },
            json!(null),
        )
        .expect("install B");
    let c = ctx
        .component(
            Declared {
                name: "C",
                inject: Inject::required(&["a"]),
                provides: vec!["c"],
            },
            json!(null),
        )
        .expect("install C");
    assert_eq!(a.state(), FiberState::Pending);
    assert_eq!(b.state(), FiberState::Pending);
    assert_eq!(c.state(), FiberState::Pending);

    let mut cycles = ctx.dependency_cycles();
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    let group = cycles.pop().expect("one group");
    assert_eq!(group.len(), 3, "{group:?}");
    for prefix in ["A#", "B#", "C#"] {
        assert!(
            group.iter().any(|label| label.starts_with(prefix)),
            "{prefix} missing from {group:?}"
        );
    }

    // Reported exactly once (deduped by shape).
    let warnings = |capture: &Capture| {
        capture
            .lines()
            .iter()
            .filter(|line| line.contains("WARN") && line.contains("dependency cycle"))
            .count()
    };
    assert_eq!(warnings(&capture), 1);

    // B stays pending behind C's disappearance — missing is not a cycle.
    c.dispose();
    assert!(
        ctx.dependency_cycles().is_empty(),
        "{:?}",
        ctx.dependency_cycles()
    );
    assert_eq!(warnings(&capture), 1);

    // A stays pending on B's declared-but-never-fulfilled provide — still not a cycle.
    ctx.provide_raw("c", Arc::new(json!({})))
        .expect("provide c");
    assert!(b.wait_for(FiberState::Active, std::time::Duration::from_secs(5)));
    assert_eq!(a.state(), FiberState::Pending);
    assert!(ctx.dependency_cycles().is_empty());
    assert_eq!(warnings(&capture), 1);

    let d = ctx
        .component(
            Declared {
                name: "D",
                inject: Inject::required(&["a"]),
                provides: vec!["b"],
            },
            json!(null),
        )
        .expect("install D");
    assert_eq!(d.state(), FiberState::Pending);
    let cycles = ctx.dependency_cycles();
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert_eq!(cycles[0].len(), 2, "{:?}", cycles[0]);
    assert_eq!(warnings(&capture), 2, "a new cycle shape is reported again");
}

#[test]
fn self_claim_on_own_missing_dependency_is_reported() {
    let ctx = Context::new();
    let solo = ctx
        .component(
            Declared {
                name: "solo",
                inject: Inject::required(&["x"]),
                provides: vec!["x"],
            },
            json!(null),
        )
        .expect("install solo");
    assert_eq!(solo.state(), FiberState::Pending);

    let cycles = ctx.dependency_cycles();
    assert_eq!(
        cycles,
        vec![vec![format!("solo#{}", solo.id().expect("live"))]]
    );
}

#[test]
fn healthy_and_merely_missing_dependencies_are_not_cycles() {
    let ctx = Context::new();

    let provider = ctx
        .component(
            |ctx: &Context, _: &Value| -> ComponentResult {
                ctx.provide("b", Arc::new(json!({})))?;
                Ok(())
            },
            json!(null),
        )
        .expect("install provider");
    let consumer = ctx
        .component(
            Declared {
                name: "A",
                inject: Inject::required(&["b"]),
                provides: vec!["a"],
            },
            json!(null),
        )
        .expect("install consumer");
    assert_eq!(consumer.state(), FiberState::Active);
    assert!(ctx.dependency_cycles().is_empty());

    let waiting = ctx
        .component(
            Declared {
                name: "wait",
                inject: Inject::required(&["zz"]),
                provides: vec![],
            },
            json!(null),
        )
        .expect("install waiting");
    assert_eq!(waiting.state(), FiberState::Pending);
    assert!(ctx.dependency_cycles().is_empty());

    let third = ctx
        .component(
            Declared {
                name: "third",
                inject: Inject::required(&["a"]),
                provides: vec![],
            },
            json!(null),
        )
        .expect("install third");
    assert_eq!(
        third.state(),
        FiberState::Pending,
        "declared but never provided"
    );
    assert!(ctx.dependency_cycles().is_empty());
    provider.dispose();
}
