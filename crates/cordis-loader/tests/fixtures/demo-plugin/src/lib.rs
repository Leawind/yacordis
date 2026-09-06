//! A demo cordis plugin: declared dependency, provided service, event
//! listener, and a disposer that appends to a log file — everything the
//! integration test drives through the C ABI.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

use cordis_plugin_api::prelude::*;

static DISPOSES: AtomicUsize = AtomicUsize::new(0);

struct DemoPlugin;

impl Plugin for DemoPlugin {
    fn name(&self) -> Option<&str> {
        Some("demo-greeter")
    }

    fn inject(&self) -> Inject {
        Inject::required(&["database"])
    }

    fn apply(&self, ctx: &Context, config: &Value) -> PluginResult {
        let greeting = config["greeting"].as_str().unwrap_or("hello").to_owned();
        let log_path = config["log"].as_str().unwrap_or("").to_owned();

        // A JSON service, visible to the host and to other plugins.
        ctx.provide("greeter", json!({ "greeting": greeting }))
            .map_err(PluginError::new)?;

        // A listener that reads its config and a host-provided service.
        ctx.on("greet", move |ctx, args| {
            let db = ctx.get("database")?;
            let who = args["who"].as_str().unwrap_or("?");
            let uri = db["uri"].as_str().unwrap_or("?");
            Some(json!({ "text": format!("{greeting}, {who} (db: {uri})") }))
        })
        .map_err(PluginError::new)?;

        // An effect whose disposer writes to a file: hard proof that
        // plugin-side cleanup runs when the host reverts the fiber.
        ctx.effect("demo/log", move |_| {
            append(&log_path, "setup\n");
            Ok(vec![Disposable::new(move || {
                DISPOSES.fetch_add(1, Ordering::SeqCst);
                append(&log_path, "disposed\n");
            })])
        })
        .map_err(PluginError::new)?;

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
        .expect("demo plugin: open log file");
    file.write_all(text.as_bytes()).expect("demo plugin: write log");
}

register_plugin!(DemoPlugin);
