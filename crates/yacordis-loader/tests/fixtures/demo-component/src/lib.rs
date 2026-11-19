//! A demo cordis component: declared dependency, provided service, event
//! listener, and a disposer that appends to a log file — everything the
//! integration test drives through the C ABI.

use std::io::Write;

use yacordis_component_api::prelude::*;

struct DemoComponent;

impl Component for DemoComponent {
    fn name(&self) -> Option<&str> {
        Some("demo-greeter")
    }

    fn inject(&self) -> Inject {
        Inject::required(&["database"])
    }

    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
        let greeting = config["greeting"].as_str().unwrap_or("hello").to_owned();
        let log_path = config["log"].as_str().unwrap_or("").to_owned();

        if config["fail"].as_bool().unwrap_or(false) {
            return Err(ComponentError::new("demo component asked to fail"));
        }

        // A JSON service, visible to the host and to other components.
        ctx.provide("greeter", json!({ "greeting": greeting }))
            .map_err(ComponentError::new)?;

        // A listener that reads its config and a host-provided service.
        ctx.on("greet", move |ctx, args| {
            let db = ctx.get("database")?;
            let who = args["who"].as_str().unwrap_or("?");
            let uri = db["uri"].as_str().unwrap_or("?");
            Some(json!({ "text": format!("{greeting}, {who} (db: {uri})") }))
        })
        .map_err(ComponentError::new)?;

        // An effect whose disposer writes to a file: hard proof that
        // component-side cleanup runs when the host reverts the fiber.
        let log_for_effect = log_path.clone();
        ctx.effect("demo/log", move |_| {
            append(&log_for_effect, "setup\n");
            Ok(vec![Disposable::new(move || {
                append(&log_for_effect, "disposed\n");
            })])
        })
        .map_err(ComponentError::new)?;

        // Optional blocking-work demo: polls the cancellation token, and
        // the completion callback appends to the log from the worker
        // thread (a framework-initiated call).
        let work_delay = config["work_delay_ms"].as_u64().unwrap_or(0);
        if work_delay > 0 {
            let log_for_work = log_path.clone();
            let log_for_started = log_for_work.clone();
            ctx.run_blocking(
                "demo/work",
                move |token| {
                    // Marks that component code is in flight on the worker
                    // thread: the unload tests poll for this line to know
                    // the work trampoline has been entered.
                    append(&log_for_started, "started\n");
                    let mut waited = 0u64;
                    while waited < work_delay {
                        if token.is_cancelled() {
                            return Ok(json!({ "waited": waited, "cancelled": true }));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        waited += 10;
                    }
                    Ok(json!({ "waited": waited, "cancelled": false }))
                },
                move |_, outcome| {
                    let tag = match outcome {
                        WorkOutcome::Done(_) => "work-done",
                        WorkOutcome::Cancelled => "work-cancelled",
                        WorkOutcome::Failed(_) => "work-failed",
                        WorkOutcome::Panicked(_) => "work-panicked",
                    };
                    append(&log_for_work, &format!("{tag}\n"));
                },
            )
            .map_err(ComponentError::new)?;
        }

        // Child component install: exercises the component-code install
        // path (a nested adapter on the same lease, tracked for unload).
        if config["child"].as_bool().unwrap_or(false) {
            ctx.component(
                |ctx: &Context, _: &Value| -> ComponentResult {
                    ctx.provide("greeter/child", json!({ "from": "child" }))
                        .map_err(ComponentError::new)
                },
                &json!(null),
            )
            .map_err(ComponentError::new)?;
        }

        Ok(())
    }
}

fn append(path: &str, text: &str) {
    if path.is_empty() {
        return;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("demo component: open log file");
    file.write_all(text.as_bytes())
        .expect("demo component: write log");
}

register_component!(DemoComponent);
