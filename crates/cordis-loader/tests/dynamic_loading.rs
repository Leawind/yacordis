//! End-to-end proof of runtime binary plugin installation: cargo-build a
//! real cdylib plugin from source, load it into a running host, and
//! drive its full lifecycle across the C ABI.

use std::path::Path;
use std::sync::Arc;

use cordis::prelude::*;
use cordis_loader::{BuildOptions, Loader};
use serde_json::{Value, json};

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-plugin")
}

fn build_options() -> BuildOptions {
    BuildOptions {
        offline: true,
        target_dir: Some(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/loader-integration"),
        ),
    }
}

#[test]
fn full_binary_plugin_lifecycle() {
    // 1. Build the plugin from source (the developer path) and load it.
    let plugin = Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    assert_eq!(plugin.manifest.name.as_deref(), Some("demo-greeter"));
    assert_eq!(plugin.manifest.required, vec!["database".to_owned()]);
    assert!(plugin.manifest.optional.is_empty());

    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    // 2. Host provides the dependency the plugin requires, then installs.
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide");

    // A native consumer of the FFI-provided service: it only activates
    // once the *binary plugin* provides `greeter`.
    let consumer = ctx
        .inject(Inject::required(&["greeter"]), |ctx: &Context| {
            assert!(ctx.get::<Value>("greeter").is_some(), "FFI service visible");
            Ok(())
        })
        .expect("install consumer");
    assert_eq!(consumer.state(), FiberState::Pending);

    // 3. Install the binary plugin: it activates and its service feeds
    //    the native consumer reactively.
    let log_path = build_options()
        .target_dir
        .unwrap()
        .join("demo-lifecycle.log");
    let _ = std::fs::remove_file(&log_path);
    let handle = plugin
        .install(
            &ctx,
            json!({ "greeting": "hola", "log": log_path.to_str().unwrap() }),
        )
        .expect("install plugin");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(
        consumer.state(),
        FiberState::Active,
        "consumer activated by FFI service"
    );

    // 4. Events dispatched by the host reach the plugin listener; the
    //    listener reads the host-provided service through the ABI.
    let reply = ctx
        .bail("greet", &json!({ "who": "world" }))
        .expect("bail reply");
    assert_eq!(
        reply,
        json!({ "text": "hola, world (db: pg://host/test)" }),
        "plugin listener answer"
    );

    // 5. The provided service is typed-downcastable natively.
    let greeter = ctx.get::<Value>("greeter").expect("greeter service");
    assert_eq!(greeter["greeting"], json!("hola"));

    // 6. Hot config update restarts the fiber; service value refreshes.
    handle
        .update(json!({ "greeting": "bonjour", "log": log_path.to_str().unwrap() }))
        .expect("update");
    assert_eq!(
        ctx.get::<Value>("greeter").expect("greeter")["greeting"],
        json!("bonjour")
    );
    assert!(handle.is_active());

    // 7. Dispose: plugin disposers run (file proof), service withdrawn,
    //    the native consumer deactivates.
    handle.dispose().expect("dispose");
    assert_eq!(
        consumer.state(),
        FiberState::Pending,
        "consumer halted with provider"
    );
    assert!(!ctx.has("greeter"));
    let log = std::fs::read_to_string(&log_path).expect("plugin log file");
    assert_eq!(
        log.matches("setup\n").count(),
        2,
        "initial load + update restart: {log}"
    );
    assert_eq!(
        log.matches("disposed\n").count(),
        2,
        "update restart + dispose: {log}"
    );

    // 8. Reinstall from the same mapped library.
    let handle2 = plugin
        .install(
            &ctx,
            json!({ "greeting": "hola", "log": log_path.to_str().unwrap() }),
        )
        .expect("reinstall");
    assert!(handle2.is_active());
    assert!(ctx.has("greeter"));
    handle2.dispose().expect("dispose again");
    assert!(!ctx.has("greeter"));
}

#[test]
fn pending_until_host_provides_the_dependency() {
    let plugin = Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    // No `database` anywhere: the plugin must wait, body unexecuted.
    let handle = plugin
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(!ctx.has("greeter"), "body did not run");

    // Dependency appears: the binary plugin activates reactively.
    ctx.provide("database", Arc::new(json!({ "uri": "pg://late" })))
        .expect("provide");
    assert_eq!(handle.state(), FiberState::Active);
    assert!(ctx.has("greeter"));
    handle.dispose().expect("dispose");
}
