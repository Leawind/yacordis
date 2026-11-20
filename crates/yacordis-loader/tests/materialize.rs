//! Content-addressed materialization: the mapped file is a copy named by
//! its sha-256 digest, never the producer's artifact path. A host that
//! rebuilds an artifact before releasing the previous copy therefore never
//! writes to a mapped file (the Windows image lock, or an in-place
//! truncation under a mapping on Unix).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, LoadError, Loader, sha256_hex};

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

fn files_in(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .expect("read cache dir")
        .map(|entry| {
            entry
                .expect("cache entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// Open the file for writing: the observable form of "no mapping keeps the
/// file locked". Retries ride out transient interference (indexing,
/// antivirus).
fn file_is_writable(path: &Path) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if std::fs::OpenOptions::new().write(true).open(path).is_ok() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Copy the fixture to a writable location, pointing its API dependency at
/// the workspace so the copy builds outside its original directory.
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
    assert!(
        manifest.contains(&api),
        "the copied fixture points at the workspace API crate"
    );
    std::fs::write(destination.join("Cargo.toml"), manifest).expect("write manifest");
}

#[test]
fn materialized_copies_are_content_addressed_and_shared() {
    let root = scratch("shared");
    let _ = std::fs::remove_dir_all(&root);
    let source = Loader::build(fixtures(), &build_options(&root.join("target")))
        .expect("build the demo component");
    let bytes = std::fs::read(&source).expect("read the artifact");
    let expected_digest = sha256_hex(&bytes);
    let cache = root.join("cache");

    // Concurrent materializes of the same content settle on one copy.
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let source = source.clone();
            let cache = cache.clone();
            std::thread::spawn(move || Loader::materialize(&source, &cache).expect("materialize"))
        })
        .collect();
    let artifacts: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().expect("materialize thread"))
        .collect();
    let copy = artifacts[0].path().to_path_buf();
    for artifact in &artifacts {
        assert_eq!(artifact.digest(), expected_digest);
        assert_eq!(artifact.source_path(), Some(source.as_path()));
        assert_eq!(
            artifact.path().file_stem().and_then(|stem| stem.to_str()),
            Some(expected_digest.as_str())
        );
        assert_eq!(artifact.path(), copy);
        assert_eq!(std::fs::read(artifact.path()).expect("read copy"), bytes);
    }
    assert_eq!(
        files_in(&cache),
        vec![copy.file_name().unwrap().to_string_lossy().into_owned()]
    );

    // The materialized loads map the copy; the producer's path stays writable.
    let first = Loader::load_materialized(&source, &cache).expect("load the first copy");
    let second = Loader::load_materialized(&source, &cache).expect("load the second copy");
    assert_eq!(first.path(), copy);
    assert_eq!(second.path(), copy);
    assert_eq!(first.digest(), Some(expected_digest.as_str()));
    assert_eq!(first.source_path(), Some(source.as_path()));
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .is_ok(),
        "the producer's artifact is not mapped"
    );

    // Embedded bytes with the same content reuse the same copy.
    let embedded = Loader::load_bytes(&bytes, &cache).expect("load embedded bytes");
    assert_eq!(embedded.path(), copy);
    assert_eq!(embedded.digest(), Some(expected_digest.as_str()));
    assert_eq!(embedded.source_path(), None);

    // In-place loads expose neither a digest nor a source.
    let in_place = Loader::load_file(&source).expect("load in place");
    assert_eq!(in_place.digest(), None);
    assert_eq!(in_place.source_path(), None);
    in_place.unload().expect("unload the in-place library");

    // Every mapping of the copy must be released for Windows to unlock it.
    first.unload().expect("unload the first copy");
    second.unload().expect("unload the second copy");
    embedded.unload().expect("unload the embedded copy");
    assert!(file_is_writable(&copy), "all mappings are released");

    // Pruning never invalidates a loaded copy: on Unix the file disappears
    // while the mapping lives on; on Windows the deletion is refused.
    let mapped = Loader::load_materialized(&source, &cache).expect("load the copy again");
    Loader::prune_materialized(&cache, &[]).expect("a mapped copy is not an error");
    assert_eq!(mapped.manifest.name.as_deref(), Some("demo-greeter"));
    mapped.unload().expect("unload after the prune");
}

#[test]
fn rebuilding_the_artifact_while_the_old_copy_is_mapped_loads_new_code() {
    let root = scratch("rebuild");
    let _ = std::fs::remove_dir_all(&root);
    let working = root.join("component");
    copy_fixture(&working);
    let target = root.join("target");
    let cache = root.join("cache");

    let first_path =
        Loader::build(&working, &build_options(&target)).expect("build the first version");
    let first = Loader::load_materialized(&first_path, &cache).expect("load the first version");
    let ctx1 = context();
    let handle1 = first
        .install(&ctx1, json!({ "greeting": "hi" }))
        .expect("install the first version");
    assert_eq!(
        ctx1.get::<Value>("greeter").expect("v1 service")["build"],
        Value::Null
    );
    assert!(
        std::fs::OpenOptions::new()
            .write(true)
            .open(&first_path)
            .is_ok(),
        "the producer's artifact is not mapped"
    );

    // Change the component: its provided service gains a marker.
    let source_file = working.join("src/lib.rs");
    let source = std::fs::read_to_string(&source_file).expect("read the component source");
    let source = source.replace(
        "json!({ \"greeting\": greeting })",
        "json!({ \"greeting\": greeting, \"build\": 2 })",
    );
    assert!(
        source.contains("\"build\": 2"),
        "the fixture change applied to the copied source"
    );
    std::fs::write(&source_file, source).expect("write the component source");

    // The first copy is still mapped. Rebuilding the artifact in place is
    // the Windows regression: with an in-place load this step fails with
    // `failed to remove file`.
    let second_path = Loader::build(&working, &build_options(&target))
        .expect("rebuild while the old copy is mapped");
    let second = Loader::load_materialized(&second_path, &cache).expect("load the second version");
    assert_ne!(first.digest(), second.digest());
    assert_ne!(first.path(), second.path());
    assert_eq!(
        ctx1.bail("greet", &json!({ "who": "world" }))
            .expect("the old version keeps serving")["text"],
        json!("hi, world (db: pg://host/test)")
    );

    let ctx2 = context();
    let handle2 = second
        .install(&ctx2, json!({ "greeting": "hello" }))
        .expect("install the second version");
    assert_eq!(
        ctx2.get::<Value>("greeter").expect("v2 service")["build"],
        json!(2)
    );
    assert_eq!(
        ctx2.bail("greet", &json!({ "who": "world" }))
            .expect("the new version serves")["text"],
        json!("hello, world (db: pg://host/test)")
    );
    assert_eq!(files_in(&cache).len(), 2, "each version keeps its own copy");

    handle2.dispose();
    handle1.dispose();
    second.unload().expect("unload the second version");
    first.unload().expect("unload the first version");
}

#[test]
fn prune_removes_unkept_copies_and_stale_temps_leaving_the_rest() {
    let cache = scratch("prune");
    let _ = std::fs::remove_dir_all(&cache);
    std::fs::create_dir_all(&cache).expect("create cache");
    let kept = format!("{:064x}", 1);
    let dropped = format!("{:064x}", 2);
    std::fs::write(cache.join(format!("{kept}.dll")), b"kept").expect("write kept copy");
    std::fs::write(cache.join(format!("{dropped}.dll")), b"dropped").expect("write dropped copy");
    std::fs::write(cache.join("README"), b"not a copy").expect("write foreign file");
    std::fs::write(cache.join(".123-0.tmp"), b"in progress").expect("write temporary file");

    Loader::prune_materialized(&cache, &[&kept]).expect("prune the cache");

    assert!(
        cache.join(format!("{kept}.dll")).is_file(),
        "kept copy survives"
    );
    assert!(
        !cache.join(format!("{dropped}.dll")).exists(),
        "unkept copy is removed"
    );
    assert!(
        cache.join("README").is_file(),
        "foreign files are left alone"
    );
    assert!(
        cache.join(".123-0.tmp").is_file(),
        "temporary files are left to their writer"
    );

    // A temp copy older than the grace cannot be in flight: it is what a
    // crashed process leaves behind, and prune reclaims it.
    let stale = cache.join(".123-1.tmp");
    std::fs::write(&stale, b"crash leftover").expect("write stale temp");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&stale)
        .expect("open the stale temp");
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(1200)),
    )
    .expect("backdate the stale temp");
    drop(file);

    Loader::prune_materialized(&cache, &[&kept]).expect("prune the cache again");
    assert!(!stale.exists(), "a stale temp copy is a crash leftover");
    assert!(
        cache.join(".123-0.tmp").is_file(),
        "a fresh temp copy is still left to its writer"
    );
    Loader::prune_materialized(scratch("prune-missing"), &[]).expect("a missing cache is fine");
}

#[test]
fn materialize_reports_the_io_error_instead_of_panicking() {
    let root = scratch("io-error");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create scratch");
    let blocker = root.join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("write blocker");
    let source = root.join("artifact.dll");
    std::fs::write(&source, b"component bytes").expect("write source");

    let error = Loader::materialize(&source, blocker.join("cache"))
        .expect_err("a cache root inside a regular file cannot be created");
    assert!(matches!(error, LoadError::Materialize(_)), "{error}");
}
