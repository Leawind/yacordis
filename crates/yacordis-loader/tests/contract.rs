//! Unload-contract mechanization (§4.1): the ast-grep lint profile
//! rejects contract-violating component sources before they build, and the
//! dispose audit collects context handles the component stashed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use yacordis::prelude::*;
use yacordis_loader::{BuildOptions, Loader, lint_source};
use serde_json::json;

fn ast_grep_available() -> bool {
    std::process::Command::new("ast-grep")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/contract-test")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[test]
fn lint_rejects_contract_violations_and_passes_clean_sources() {
    if !ast_grep_available() {
        eprintln!("ast-grep not installed; skipping lint regression test");
        return;
    }

    let violating = scratch_dir("violating");
    std::fs::write(
        violating.join("bad.rs"),
        "fn leaky() {\n    std::thread::spawn(|| 1);\n    static mut BAD: u32 = 0;\n}\n",
    )
    .expect("write violation");
    let findings = lint_source(&violating).expect_err("violations found");
    assert!(
        findings.contains("yacordis-no-raw-thread-spawn"),
        "{findings}"
    );
    assert!(findings.contains("yacordis-no-mutable-static"), "{findings}");

    let clean = scratch_dir("clean");
    std::fs::write(
        clean.join("good.rs"),
        "fn fine(ctx: &yacordis::Context) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {\n    ctx.run_blocking(\"w\", |_| Ok(serde_json::json!(null)), |_, _| {})?;\n    Ok(())\n}\n",
    )
    .expect("write clean");
    assert!(
        lint_source(&clean).is_ok(),
        "framework-mediated concurrency must pass the lint"
    );
}

#[test]
fn lint_gated_build_rejects_violating_sources() {
    if !ast_grep_available() {
        eprintln!("ast-grep not installed; skipping lint regression test");
        return;
    }

    let violating = scratch_dir("violating-build");
    std::fs::create_dir_all(violating.join("src")).expect("src dir");
    std::fs::write(
        violating.join("Cargo.toml"),
        "[package]\nname = \"bad-component\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n",
    )
    .expect("write manifest");
    std::fs::write(
        violating.join("src/lib.rs"),
        "pub fn leaky() {\n    std::thread::spawn(|| 1);\n}\n",
    )
    .expect("write source");

    let options = BuildOptions {
        lint: true,
        ..Default::default()
    };
    let error = Loader::build(&violating, &options).expect_err("lint-gated build must reject");
    assert!(
        error.to_string().contains("yacordis-no-raw-thread-spawn"),
        "{error}"
    );
}

#[test]
fn dispose_audit_collects_stashed_context_handles() {
    let component =
        Loader::build_and_load(fixture_dir(), &build_options()).expect("build and load");
    let ctx = Context::new();
    ctx.set_logger(Arc::new(yacordis::DiscardLogger));
    ctx.provide("database", Arc::new(json!({ "uri": "pg://x" })))
        .expect("provide");

    let handle = component
        .install(&ctx, json!({ "greeting": "hi" }))
        .expect("install");
    let fiber_id = handle.id().expect("fiber id");
    // The FFI component caches a context handle for its fiber while alive.
    // (The audit drains what it finds; a later component call re-mints.)
    assert!(
        yacordis_loader::audit_fiber_handles(fiber_id) > 0,
        "a live fiber has at least one context handle stashed"
    );

    handle.dispose();
    assert_eq!(
        yacordis_loader::audit_fiber_handles(fiber_id),
        0,
        "dispose must collect every context handle bound to the dead fiber"
    );
}

fn fixture_dir() -> PathBuf {
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
