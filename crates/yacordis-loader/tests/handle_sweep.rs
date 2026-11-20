//! Context handles must not outlive their fiber: the process-global
//! table is swept at component activity and at library unload, so the
//! lifecycle `install → use → dispose → unload` leaves no residue in a
//! long-running host.
//!
//! This file is deliberately a single test: the handle table is
//! process-global, and a test binary of its own keeps the assertions
//! deterministic against sibling tests running in parallel elsewhere.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, Loader};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-component")
}

fn unique_artifact(test: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/handle-sweep-test")
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

#[test]
fn dispose_and_unload_leave_no_stale_context_handles() {
    let artifact = unique_artifact("sweep");
    let component = Loader::load_file(&artifact).expect("load");
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide");

    // Dead-fiber handles surface under the u64::MAX audit key; the table
    // starts out clean.
    assert_eq!(
        yacordis_loader::audit_fiber_handles(u64::MAX),
        0,
        "fresh process, clean table"
    );

    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    assert!(handle.is_active());
    assert_eq!(
        yacordis_loader::audit_fiber_handles(u64::MAX),
        0,
        "the installed fiber's context handle is alive, not residue"
    );

    // Host-side disposal: the fiber's handle is stale from here on, but
    // nothing swept it yet (the audit hook only covers component-initiated
    // disposal).
    handle.dispose();
    assert_eq!(
        yacordis_loader::audit_fiber_handles(u64::MAX),
        1,
        "the dead fiber's context handle is still in the table before the sweep point"
    );

    // Unloading is the lifecycle's final step and sweeps the residue.
    component.unload().expect("unload");
    assert_eq!(
        yacordis_loader::audit_fiber_handles(u64::MAX),
        0,
        "unload swept the stale handle: no residue pins the runtime"
    );
    assert!(file_is_writable(&artifact), "unmap released the file lock");
}

fn file_is_writable(path: &Path) -> bool {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .is_ok()
}
