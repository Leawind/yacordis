//! Ephemeral materialization: the copy exists only while it is mapped.
//!
//! Each ephemeral load maps a private, temp-named copy. Off Windows the
//! copy is unlinked as soon as the mapping exists, so the cache directory
//! is empty while the component runs. Windows cannot delete a mapped
//! image, so there the copy goes when the library is unloaded. A failed
//! load maps nothing and deletes its copy on every platform.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, LoadError, Loader};

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

/// Poll until `check` holds: file handles and deletions are asynchronous
/// against watchers and the indexer on Windows.
fn eventually(check: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
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

#[test]
fn an_ephemeral_load_leaves_no_copy_after_the_mapping_closes() {
    let root = scratch("ephemeral-source");
    let _ = std::fs::remove_dir_all(&root);
    let working = root.join("component");
    copy_fixture(&working);
    let target = root.join("target");
    let cache = root.join("cache");

    let artifact =
        Loader::build(&working, &build_options(&target)).expect("build the fixture artifact");
    let library =
        Loader::load_materialized_ephemeral(&artifact, &cache).expect("load an ephemeral copy");
    let copy = library.path().to_path_buf();
    assert_ne!(copy, artifact, "the producer's artifact is not mapped");
    assert!(
        library.digest().is_some(),
        "materialized loads carry a digest"
    );
    let copy_name = copy
        .file_name()
        .and_then(|name| name.to_str())
        .expect("utf8 copy name");
    assert!(
        copy_name.starts_with('.') && copy_name.ends_with(".tmp"),
        "an ephemeral copy lives in the temp namespace `prune_materialized` ages out: {copy_name}"
    );

    let ctx = context();
    let handle = library
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install the ephemeral component");
    assert_eq!(
        ctx.get::<Value>("greeter").expect("provided service")["greeting"],
        json!("hi")
    );

    #[cfg(windows)]
    {
        // Windows cannot delete a mapped image: the copy is still there.
        assert!(copy.exists(), "the mapped copy stays on Windows");
        assert_eq!(names(&cache).len(), 1, "one copy while loaded");
    }
    #[cfg(not(windows))]
    {
        // The unlink happened at load: the directory is already empty
        // while the component runs.
        assert!(
            eventually(|| !copy.exists()),
            "the copy is unlinked at load off Windows"
        );
        assert!(names(&cache).is_empty(), "no cache entry while loaded");
        assert!(
            ctx.get::<Value>("greeter").is_some(),
            "the unmapped file's contents stay live through the mapping"
        );
    }

    handle.dispose();
    library.unload().expect("unload the ephemeral component");
    assert!(
        eventually(|| !copy.exists()),
        "the copy is gone once the mapping closes"
    );
    assert!(
        eventually(|| names(&cache).is_empty()),
        "no copy is left in the cache"
    );
}

#[test]
fn an_ephemeral_bytes_load_leaves_no_copy_after_the_mapping_closes() {
    let root = scratch("ephemeral-bytes");
    let _ = std::fs::remove_dir_all(&root);
    let working = root.join("component");
    copy_fixture(&working);
    let target = root.join("target");
    let cache = root.join("cache");

    let artifact =
        Loader::build(&working, &build_options(&target)).expect("build the fixture artifact");
    let bytes = std::fs::read(&artifact).expect("read the artifact");
    let library = Loader::load_bytes_ephemeral(&bytes, &cache).expect("load ephemeral bytes");
    let copy = library.path().to_path_buf();
    assert_eq!(
        library.digest(),
        Some(yacordis_loader::sha256_hex(&bytes).as_str()),
        "the digest names the loaded bytes"
    );

    let ctx = context();
    let handle = library
        .install(&ctx, json!({ "greeting": "hello" }))
        .expect("install the embedded component");
    assert_eq!(
        ctx.get::<Value>("greeter").expect("provided service")["greeting"],
        json!("hello")
    );

    handle.dispose();
    library.unload().expect("unload the embedded component");
    assert!(
        eventually(|| !copy.exists()),
        "the copy is gone once the mapping closes"
    );
    assert!(
        eventually(|| names(&cache).is_empty()),
        "no copy is left in the cache"
    );
}

#[test]
fn concurrent_ephemeral_loads_own_separate_copies() {
    let root = scratch("ephemeral-shared");
    let _ = std::fs::remove_dir_all(&root);
    let working = root.join("component");
    copy_fixture(&working);
    let target = root.join("target");
    let cache = root.join("cache");

    let artifact =
        Loader::build(&working, &build_options(&target)).expect("build the fixture artifact");
    let first =
        Loader::load_materialized_ephemeral(&artifact, &cache).expect("load the first copy");
    let second =
        Loader::load_materialized_ephemeral(&artifact, &cache).expect("load the second copy");
    assert_eq!(
        first.digest(),
        second.digest(),
        "identical content, one digest"
    );
    assert_ne!(
        first.path(),
        second.path(),
        "each ephemeral load owns a private copy"
    );
    let first_copy = first.path().to_path_buf();

    let ctx = context();
    let handle1 = first
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install the first");
    let handle2 = second
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install the second");

    handle1.dispose();
    first.unload().expect("unload the first");
    assert!(
        eventually(|| !first_copy.exists()),
        "the first load's copy goes with its own unload"
    );
    #[cfg(windows)]
    assert!(
        second.path().exists(),
        "the second load's copy is untouched"
    );

    handle2.dispose();
    second.unload().expect("unload the second");
    assert!(
        eventually(|| names(&cache).is_empty()),
        "each load removes its own copy: {:?}",
        names(&cache)
    );
}

#[test]
fn a_failed_ephemeral_load_leaves_nothing() {
    let root = scratch("ephemeral-failed");
    let _ = std::fs::remove_dir_all(&root);
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).expect("create cache");

    // Bytes that are no component binary fail to map on every platform.
    let error = Loader::load_bytes_ephemeral(b"not a component binary", &cache)
        .err()
        .expect("garbage bytes do not load");
    assert!(matches!(error, LoadError::Library(_)), "{error}");
    assert!(
        eventually(|| names(&cache).is_empty()),
        "the failed bytes load leaves no copy: {:?}",
        names(&cache)
    );

    // The same holds for a source artifact whose file cannot be mapped.
    let garbage = root.join("garbage.dll");
    std::fs::write(&garbage, b"not a component binary").expect("write garbage artifact");
    let error = Loader::load_materialized_ephemeral(&garbage, &cache)
        .err()
        .expect("a garbage artifact does not load");
    assert!(matches!(error, LoadError::Library(_)), "{error}");
    assert!(
        eventually(|| names(&cache).is_empty()),
        "the failed source load leaves no copy: {:?}",
        names(&cache)
    );
}
