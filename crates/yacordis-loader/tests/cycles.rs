//! Repeated ephemeral load/unload cycles: the quit-state proof and the
//! copy cleanup must not degrade as a host swaps a component in and out.
//!
//! One test function on purpose: the live-object counters are
//! process-global, so a parallel case would race the baseline.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, Loader};

const CYCLES: usize = 100;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-component")
}

fn scratch(test: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/materialize-test")
        .join(test)
}

fn build_options(target_dir: &Path) -> BuildOptions {
    BuildOptions {
        offline: true,
        lint: false,
        target_dir: Some(target_dir.to_path_buf()),
    }
}

fn context() -> Context {
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.provide("database", Arc::new(json!({ "uri": "pg://host/test" })))
        .expect("provide database");
    ctx
}

fn copy_fixture(destination: &Path) {
    std::fs::create_dir_all(destination.join("src")).expect("fixture source dir");
    for file in ["Cargo.lock", "src/lib.rs"] {
        std::fs::copy(fixtures().join(file), destination.join(file)).expect("copy fixture file");
    }
    let api = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../yacordis-component-api")
        .to_string_lossy()
        .replace('\\', "/");
    let manifest = std::fs::read_to_string(fixtures().join("Cargo.toml")).expect("read manifest");
    let manifest = manifest.replace(
        "path = \"../../../../yacordis-component-api\"",
        &format!("path = \"{api}\""),
    );
    std::fs::write(destination.join("Cargo.toml"), manifest).expect("write manifest");
}

/// Windows keeps a deleted copy visible for a moment while handles close.
fn eventually(check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// One load → install → dispose → unload round trip.
fn cycle(ctx: &Context, artifact: &Path, cache: &Path, round: usize) {
    let library = Loader::load_materialized_ephemeral(artifact, cache)
        .unwrap_or_else(|error| panic!("round {round}: load failed: {error}"));
    let copy = library.path().to_path_buf();
    let digest = library
        .digest()
        .expect("materialized loads carry a digest")
        .to_owned();

    #[cfg(not(windows))]
    assert!(
        eventually(|| !copy.exists()),
        "round {round}: the copy survives its own load off Windows"
    );

    let handle = library
        .install(ctx, json!({ "greeting": "hi" }))
        .unwrap_or_else(|error| panic!("round {round}: install failed: {error}"));
    assert!(handle.is_active(), "round {round}: the fiber is not active");
    assert_eq!(
        ctx.get::<Value>("greeter").expect("provided service")["greeting"],
        json!("hi"),
        "round {round}: the component does not serve"
    );

    handle.dispose();
    library
        .unload()
        .unwrap_or_else(|error| panic!("round {round}: unload refused: {error}"));
    assert!(
        library.is_unloaded(),
        "round {round}: the library is not closed"
    );
    assert!(
        eventually(|| !copy.exists()),
        "round {round}: the copy outlived its mapping ({digest})"
    );
}

#[test]
fn repeated_load_and_unload_cycles_leave_nothing_behind() {
    let root = scratch("cycles");
    let _ = std::fs::remove_dir_all(&root);
    let working = root.join("component");
    copy_fixture(&working);
    let target = root.join("target");
    let cache = root.join("cache");

    let artifact = Loader::build(&working, &build_options(&target)).expect("build the fixture");

    let ctx = context();
    // Warm-up: one-off allocations (registries, thread-locals) are not
    // residue.
    cycle(&ctx, &artifact, &cache, 0);
    let baseline = yacordis::test_support::live_objects();
    // The context's own effects (the provided service) live as long as the
    // root fiber; a cycle may leave at most its last, already-spent entry
    // until the next push prunes it.
    let batches = yacordis::test_support::effect_batches(&ctx);

    for round in 1..=CYCLES {
        cycle(&ctx, &artifact, &cache, round);
    }

    assert_eq!(
        yacordis::test_support::live_objects(),
        baseline,
        "the cycles leave no live core or fiber behind"
    );
    assert!(
        eventually(|| names(&cache).is_empty()),
        "the cache is empty after {CYCLES} cycles: {:?}",
        names(&cache)
    );
    let batches_after = yacordis::test_support::effect_batches(&ctx);
    assert!(
        batches_after <= batches + 1,
        "the parent fiber retires each cycle's cascade entry: {batches} -> {batches_after}"
    );
}
