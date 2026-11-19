//! True unload: the compliant path unmaps the binary (on Windows this
//! releases the artifact's loader lock), violation paths are refused with
//! the violation named and degrade to keep-mapped while the host keeps
//! serving.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, Loader, UnloadError};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-component")
}

/// Build the demo component into a private target dir, then copy the
/// artifact to a unique file: two mappings of the same bytes keep each
/// other locked (the OS refcounts per mapping), so every test needs its
/// own copy to assert lock release.
fn unique_artifact(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/unload-test")
        .join(test);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let options = BuildOptions {
        offline: true,
        lint: false,
        target_dir: Some(dir.clone()),
    };
    let artifact = Loader::build(fixture_dir(), &options).expect("build the demo component");
    let unique = dir.join("demo-under-test.cdylib");
    std::fs::copy(&artifact, &unique).expect("copy the artifact");
    unique
}

fn context() -> Context {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx
}

fn provide_database(ctx: &Context) {
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide");
}

/// Open the artifact for writing: the observable form of "the loader lock
/// is released" (on Unix a mapped file is always writable, so the real
/// cross-platform proof is the successful unmap itself). Retries ride out
/// transient interference (indexing, antivirus).
fn file_is_writable(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::OpenOptions::new().write(true).open(path).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn unload_releases_the_file_and_rejects_later_installs() {
    let artifact = unique_artifact("release");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = context();
    provide_database(&ctx);

    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    assert!(handle.is_active());
    assert!(
        ctx.bail("greet", &json!({ "who": "world" })).is_some(),
        "component listener serves"
    );

    handle.dispose();
    component.unload().expect("unload after full dispose");

    assert!(component.is_unloaded());
    assert!(
        file_is_writable(&artifact),
        "unloading must release the artifact file lock"
    );

    // The library is gone: installs are refused outright.
    let error = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect_err("install after unload must fail");
    assert!(error.to_string().contains("unloaded"), "{error}");

    // A component obtained anyway is inert: the fiber fails cleanly
    // instead of jumping into unmapped code.
    let handle = ctx
        .component_dyn(component.component(), json!({ "greeting": "hi" }))
        .expect("install the inert adapter");
    assert_eq!(handle.state(), FiberState::Failed);
    let error = handle.error().expect("failure recorded");
    assert!(error.contains("unloaded"), "{error}");
}

#[test]
fn unload_is_refused_while_a_fiber_is_installed() {
    let artifact = unique_artifact("refuse-fiber");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = context();
    provide_database(&ctx);
    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");

    let error = component
        .unload()
        .expect_err("an installed fiber must refuse");
    match &error {
        UnloadError::FibersAlive { fibers } => {
            assert_eq!(fibers.len(), 1, "{fibers:?}");
            assert!(fibers[0].starts_with("demo-greeter#"), "{}", fibers[0]);
            assert!(fibers[0].contains("active"), "{}", fibers[0]);
        }
        other => panic!("expected FibersAlive, got {other:?}"),
    }

    // The refusal degraded to keep-mapped: the host keeps serving, and
    // more fibers of the same library can be installed (isolating the
    // provided service, the documented repeated-install pattern).
    assert!(
        ctx.bail("greet", &json!({ "who": "world" })).is_some(),
        "the refused library still serves"
    );
    let second = component
        .install(&ctx.isolate("greeter"), json!({ "greeting": "ho" }))
        .expect("install again after refusal");
    assert!(second.is_active());
    assert!(component.unload().is_err(), "still held by two fibers");

    handle.dispose();
    second.dispose();
    component.unload().expect("unload after disposing both");
    assert!(file_is_writable(&artifact));
}

#[test]
fn unload_is_refused_while_component_work_is_in_flight() {
    let artifact = unique_artifact("refuse-work");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = context();
    provide_database(&ctx);
    let log = artifact.with_extension("log");
    let _ = std::fs::remove_file(&log);
    let handle = component
        .install(
            &ctx,
            json!({
                "greeting": "hi",
                "log": log.to_str().unwrap(),
                "work_delay_ms": 30_000,
            }),
        )
        .expect("install");

    // Wait until the work trampoline is provably entered ("started" is
    // written inside component code on the worker thread).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        if text.contains("started\n") {
            break;
        }
        assert!(Instant::now() < deadline, "work never started");
        std::thread::sleep(Duration::from_millis(10));
    }

    let error = component.unload().expect_err("in-flight work must refuse");
    match &error {
        UnloadError::InFlight { calls } => {
            assert!(calls.iter().any(|call| call.contains("work")), "{calls:?}");
        }
        other => panic!("expected InFlight, got {other:?}"),
    }

    // The backstop: the library stays mapped and the fiber keeps serving.
    assert!(!component.is_unloaded());
    assert!(ctx.has("greeter"));

    // Dispose drains the work (cancel + join); the unload then succeeds.
    handle.dispose();
    component.unload().expect("unload after the drain");
    assert!(file_is_writable(&artifact));
}

#[test]
fn a_failed_fiber_blocks_unload_until_disposed() {
    let artifact = unique_artifact("failed-fiber");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = context();
    provide_database(&ctx);
    let handle = component
        .install(&ctx, json!({ "fail": true }))
        .expect("install");
    assert_eq!(handle.state(), FiberState::Failed);

    // A failed fiber still holds its component (it can recover through
    // update), so the unload is refused until it is disposed.
    assert!(matches!(
        component.unload(),
        Err(UnloadError::FibersAlive { .. })
    ));

    handle.dispose();
    component
        .unload()
        .expect("unload after disposing the failed fiber");
    assert!(file_is_writable(&artifact));
}

#[test]
fn child_components_do_not_block_unload_after_dispose() {
    let artifact = unique_artifact("child");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = context();
    provide_database(&ctx);
    let handle = component
        .install(&ctx, json!({ "child": true }))
        .expect("install");
    assert!(ctx.has("greeter/child"), "the child component is active");

    assert!(component.unload().is_err(), "held by the parent and child");

    handle.dispose();
    component
        .unload()
        .expect("unload after the cascade disposed the child");
    assert!(file_is_writable(&artifact));
}

#[test]
fn compliant_components_survive_repeated_load_unload_cycles() {
    let artifact = unique_artifact("cycles");
    let ctx = context();
    provide_database(&ctx);

    for round in 0..3 {
        let component = Loader::load_file(&artifact).expect("load");
        let handle = component
            .install(&ctx, json!({ "greeting": format!("hi-{round}") }))
            .expect("install");
        assert!(handle.is_active());
        let reply = ctx
            .bail("greet", &json!({ "who": "world" }))
            .expect("reply");
        assert_eq!(
            reply,
            json!({ "text": format!("hi-{round}, world (db: pg://host/test)") }),
            "round {round}: a fresh instance serves with its own config"
        );
        handle.dispose();
        component.unload().expect("unload");

        // Overwriting the artifact between rounds proves the lock is gone.
        std::fs::write(&artifact, std::fs::read(&artifact).unwrap())
            .expect("overwrite the artifact");
    }
}

#[test]
fn unload_from_inside_a_lifecycle_op_refuses_without_deadlock() {
    let artifact = unique_artifact("op-unload");
    let component = Arc::new(Loader::load_file(&artifact).expect("load"));
    let ctx = context();
    provide_database(&ctx);
    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    assert!(handle.is_active());

    // A native component whose body calls unload while the install
    // cascade (a lifecycle op on this very thread) is still running:
    // waiting for quiescence there would wait for itself.
    let for_body = component.clone();
    let attempted = Arc::new(Mutex::new(None));
    let attempted_for_body = attempted.clone();
    let unloader = ctx
        .component(
            move |_: &Context, _: &Value| -> ComponentResult {
                let result = for_body.unload();
                *attempted_for_body.lock().unwrap() = Some(result.is_err());
                Ok(())
            },
            json!(null),
        )
        .expect("install the native unloader");
    assert!(unloader.is_active());

    let refused = attempted.lock().unwrap().take().expect("unload attempted");
    assert!(refused, "unload inside a lifecycle op must be refused");
    assert!(
        ctx.bail("greet", &json!({ "who": "world" })).is_some(),
        "the host keeps serving"
    );

    handle.dispose();
    component.unload().expect("unload after dispose");
}

#[test]
fn concurrent_dispose_and_unload_settle_without_crashes() {
    let artifact = unique_artifact("concurrent");
    let ctx = context();
    provide_database(&ctx);

    for round in 0..10u32 {
        let component = Loader::load_file(&artifact).expect("load");
        let handle = component
            .install(&ctx, json!({ "greeting": "hi" }))
            .expect("install");
        assert!(handle.is_active());

        // One thread disposes after a jittered delay; this thread retries
        // the unload. Every refusal is evidence-backed, and quiescence
        // makes the round settle: unload only succeeds once the dispose
        // cascade has fully landed.
        let handle_for_thread = handle.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_micros(u64::from(round) * 137 % 900));
                handle_for_thread.dispose();
            });
            loop {
                match component.unload() {
                    Ok(()) => break,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => panic!("round {round}: unload never succeeded: {error}"),
                }
            }
        });
        assert!(component.is_unloaded());
        assert!(file_is_writable(&artifact), "round {round}: lock released");
    }
}
