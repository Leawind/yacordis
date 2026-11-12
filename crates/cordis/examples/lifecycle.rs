//! End-to-end lifecycle demo: install components at runtime, react to
//! services appearing/disappearing, hot-update config, and uninstall
//! everything cleanly — the temporal + spatial composability story.
//!
//! Run with `cargo run -p cordis --example lifecycle`.

use std::sync::Arc;

use cordis::prelude::*;
use serde_json::{Value, json};

struct Database {
    uri: String,
    pool: u32,
}

/// A service provider: reads config, provides a typed service.
struct DatabaseComponent;
impl Component for DatabaseComponent {
    fn name(&self) -> Option<&str> {
        Some("database")
    }
    fn apply(&self, ctx: &Context, config: &Value) -> ComponentResult {
        let uri = config["uri"].as_str().unwrap_or("memory://").to_owned();
        let pool = config["pool"].as_u64().unwrap_or(4) as u32;
        ctx.provide("database", Arc::new(Database { uri, pool }))?;
        Ok(())
    }
}

/// A consumer: declares a reactive coeffect on "database".
struct AppComponent;
impl Component for AppComponent {
    fn name(&self) -> Option<&str> {
        Some("app")
    }
    fn inject(&self) -> Inject {
        Inject::required(&["database"]).opt("cache")
    }
    fn apply(&self, ctx: &Context, _config: &Value) -> ComponentResult {
        let db = ctx
            .get::<Database>("database")
            .expect("required dependency");
        let pool = db.pool;
        ctx.effect("app/heartbeat", move |_| {
            println!("      [app] heartbeat started (pool={pool})");
            Ok(Some(
                Box::new(|| println!("      [app] heartbeat stopped")) as Disposer
            ))
        })?;
        ctx.on("request", move |ctx, req| {
            let db = ctx.get::<Database>("database")?;
            Some(json!({ "echo": req, "uri": db.uri }))
        })?;
        println!("      [app] started, listening for requests");
        Ok(())
    }
}

fn main() -> Result<(), CordisError> {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    println!("1. install app before its dependency: stays pending");
    let app = ctx.component(AppComponent, json!(null))?;
    println!("   app.state = {:?}", app.state());

    println!("2. install database: app activates reactively");
    let db = ctx.component(
        DatabaseComponent,
        json!({ "uri": "postgres://localhost", "pool": 8 }),
    )?;
    println!(
        "   app.state = {:?}, db.state = {:?}",
        app.state(),
        db.state()
    );

    println!("3. events flow through the provided service");
    let response = ctx.bail("request", &json!({ "method": "GET", "path": "/users" }));
    println!("   response = {:?}", response);

    println!("4. hot-update the database config (restart, same fiber)");
    db.update(json!({ "uri": "postgres://prod", "pool": 32 }))?;
    println!("   app still {:?}; new uri visible", app.state());

    println!("5. uninstall database: app deactivates, effects revert");
    db.dispose();
    println!("   app.state = {:?}", app.state());

    println!("6. reinstall: app comes back in place");
    let db2 = ctx.component(DatabaseComponent, json!({ "uri": "postgres://localhost" }))?;
    println!("   app.state = {:?} again", app.state());
    drop(db2);
    Ok(())
}
