//! End-to-end proof of runtime binary component installation: cargo-build a
//! real cdylib component from source, load it into a running host, and
//! drive its full lifecycle across the C ABI.

use std::path::Path;
use std::sync::Arc;

use cordis::prelude::*;
use cordis_loader::{BuildOptions, Loader};
use serde_json::{Value, json};

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-component")
}

fn build_options() -> BuildOptions {
    BuildOptions {
        offline: true,
        lint: false,
        target_dir: Some(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/loader-integration"),
        ),
    }
}

#[test]
fn full_binary_component_lifecycle() {
    // 1. Build the component from source (the developer path) and load it.
    let component =
        Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    assert_eq!(component.manifest.name.as_deref(), Some("demo-greeter"));
    assert_eq!(component.manifest.required, vec!["database".to_owned()]);
    assert!(component.manifest.optional.is_empty());

    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    // 2. Host provides the dependency the component requires, then installs.
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide");

    // A native consumer of the FFI-provided service: it only activates
    // once the *binary component* provides `greeter`.
    let consumer = ctx
        .inject(Inject::required(&["greeter"]), |ctx: &Context| {
            assert!(ctx.get::<Value>("greeter").is_some(), "FFI service visible");
            Ok(())
        })
        .expect("install consumer");
    assert_eq!(consumer.state(), FiberState::Pending);

    // 3. Install the binary component: it activates and its service feeds
    //    the native consumer reactively.
    let log_path = build_options()
        .target_dir
        .unwrap()
        .join("demo-lifecycle.log");
    let _ = std::fs::remove_file(&log_path);
    let handle = component
        .install(
            &ctx,
            json!({ "greeting": "hola", "log": log_path.to_str().unwrap() }),
        )
        .expect("install component");
    assert_eq!(handle.state(), FiberState::Active);
    assert_eq!(
        consumer.state(),
        FiberState::Active,
        "consumer activated by FFI service"
    );

    // 4. Events dispatched by the host reach the component listener; the
    //    listener reads the host-provided service through the ABI.
    let reply = ctx
        .bail("greet", &json!({ "who": "world" }))
        .expect("bail reply");
    assert_eq!(
        reply,
        json!({ "text": "hola, world (db: pg://host/test)" }),
        "component listener answer"
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

    // 7. Dispose: component disposers run (file proof), service withdrawn,
    //    the native consumer deactivates.
    handle.dispose();
    assert_eq!(
        consumer.state(),
        FiberState::Pending,
        "consumer halted with provider"
    );
    assert!(!ctx.has("greeter"));
    let log = std::fs::read_to_string(&log_path).expect("component log file");
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
    let handle2 = component
        .install(
            &ctx,
            json!({ "greeting": "hola", "log": log_path.to_str().unwrap() }),
        )
        .expect("reinstall");
    assert!(handle2.is_active());
    assert!(ctx.has("greeter"));
    handle2.dispose();
    assert!(!ctx.has("greeter"));
}

#[test]
fn pending_until_host_provides_the_dependency() {
    let component =
        Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));

    // No `database` anywhere: the component must wait, body unexecuted.
    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    assert_eq!(handle.state(), FiberState::Pending);
    assert!(!ctx.has("greeter"), "body did not run");

    // Dependency appears: the binary component activates reactively.
    ctx.provide("database", Arc::new(json!({ "uri": "pg://late" })))
        .expect("provide");
    assert_eq!(handle.state(), FiberState::Active);
    assert!(ctx.has("greeter"));
    handle.dispose();
}

/// The FFI `run_blocking` bridge: a binary component runs work on a
/// framework thread, polls the cancellation token, and its completion
/// callback (a framework-initiated call on the worker thread) appends
/// to the log. Unload drains the in-flight unit before rollback.
#[test]
fn binary_component_blocking_work() {
    let component =
        Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    let ctx = Context::new();
    ctx.set_logger(Arc::new(cordis::DiscardLogger));
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide");

    let target = build_options().target_dir.unwrap();

    // Completion path: the work runs to its delay and delivers Done.
    let log_path = target.join("demo-work.log");
    let _ = std::fs::remove_file(&log_path);
    let handle = component
        .install(
            &ctx,
            json!({ "greeting": "hi", "log": log_path.to_str().unwrap(), "work_delay_ms": 120 }),
        )
        .expect("install");
    assert!(handle.is_active());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("work-done") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "work did not complete in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    handle.dispose();

    // Drain path: a long-polling work must be cancelled and joined at
    // dispose, not waited out.
    let cancel_log = target.join("demo-work-cancel.log");
    let _ = std::fs::remove_file(&cancel_log);
    let handle2 = component
        .install(
            &ctx,
            json!({ "greeting": "hi", "log": cancel_log.to_str().unwrap(), "work_delay_ms": 10_000 }),
        )
        .expect("install long work");
    let start = std::time::Instant::now();
    handle2.dispose();
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "dispose must join the cancelled unit promptly (elapsed {:?})",
        start.elapsed()
    );
    let log = std::fs::read_to_string(&cancel_log).expect("cancel log");
    assert!(log.contains("work-cancelled"), "{log}");
    assert!(!log.contains("work-done"), "{log}");
}
