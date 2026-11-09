//! Repository index: sha-256 correctness (NIST vectors), semver
//! constraint matching, best-version resolution, and end-to-end
//! install with checksum + ABI verification against a real fixture
//! artifact (no network: local artifact paths).

use std::path::{Path, PathBuf};

use cordis_loader::{
    BuildOptions, Constraint, Loader, RegistryError, RegistryIndex, Repository, Sha256Verifier,
    Version, sha256_hex,
};
use serde_json::json;

#[test]
fn sha256_matches_nist_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    // Exactly 56 bytes: exercises the multi-block + long-padding path.
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn version_constraints_match_and_gate() {
    let caret = Constraint::parse("^1.2").expect("caret");
    assert!(caret.matches(Version::parse("1.2.0").unwrap()));
    assert!(caret.matches(Version::parse("1.9.3").unwrap()));
    assert!(!caret.matches(Version::parse("2.0.0").unwrap()));
    assert!(!caret.matches(Version::parse("1.1.9").unwrap()));

    let tilde = Constraint::parse("~1.2.3").expect("tilde");
    assert!(tilde.matches(Version::parse("1.2.9").unwrap()));
    assert!(!tilde.matches(Version::parse("1.3.0").unwrap()));

    let range = Constraint::parse(">=1.0, <2.0").expect("range");
    assert!(range.matches(Version::parse("1.5.0").unwrap()));
    assert!(!range.matches(Version::parse("2.0.0").unwrap()));

    let exact = Constraint::parse("1.2.3").expect("exact");
    assert!(exact.matches(Version::parse("1.2.3").unwrap()));
    assert!(!exact.matches(Version::parse("1.2.4").unwrap()));

    let any = Constraint::parse("*").expect("wildcard");
    assert!(any.matches(Version::parse("0.0.1").unwrap()));

    assert!(matches!(
        Constraint::parse("^not-a-version"),
        Err(RegistryError::Version(_))
    ));
}

#[test]
fn resolve_picks_the_highest_matching_version() {
    let index = RegistryIndex::from_json(&json!({
        "plugins": {
            "greeter": [
                { "version": "1.0.0", "artifact": "a.cdylib", "sha256": "x", "abi": 2 },
                { "version": "1.4.2", "artifact": "b.cdylib", "sha256": "y", "abi": 2 },
                { "version": "2.0.0", "artifact": "c.cdylib", "sha256": "z", "abi": 2 }
            ]
        }
    }))
    .expect("index");

    let entry = index
        .resolve("greeter", &Constraint::parse("^1.0").expect("constraint"))
        .expect("resolved");
    assert_eq!(entry.version, Version::parse("1.4.2").unwrap());

    assert!(index
        .resolve("greeter", &Constraint::parse("^3.0").expect("constraint"))
        .is_err());
    assert!(index
        .resolve("missing", &Constraint::parse("*").expect("constraint"))
        .is_err());
}

#[test]
fn install_verifies_checksum_and_abi_then_loads() {
    // Build the real fixture artifact and hash the actual bytes.
    let fixture_cdylib = build_fixture();
    let bytes = std::fs::read(&fixture_cdylib).expect("artifact bytes");
    let digest = sha256_hex(&bytes);

    let index = RegistryIndex::from_json(&json!({
        "plugins": {
            "demo-greeter": [
                { "version": "1.0.0", "artifact": fixture_cdylib.to_str().unwrap(),
                  "sha256": digest, "abi": 2 }
            ]
        }
    }))
    .expect("index");

    let repo = Repository::new("https://registry.invalid/v1", temp_cache_dir("registry-ok"));
    let plugin = repo
        .install(
            &index,
            "demo-greeter",
            &Constraint::parse("^1").expect("constraint"),
            &Sha256Verifier,
        )
        .expect("install");
    assert_eq!(plugin.manifest.name.as_deref(), Some("demo-greeter"));

    // Corrupted checksum is rejected before any code is mapped.
    let bad_index = RegistryIndex::from_json(&json!({
        "plugins": {
            "demo-greeter": [
                { "version": "1.0.0", "artifact": fixture_cdylib.to_str().unwrap(),
                  "sha256": "0".repeat(64), "abi": 2 }
            ]
        }
    }))
    .expect("bad index");
    let error = repo
        .install(
            &bad_index,
            "demo-greeter",
            &Constraint::parse("*").expect("constraint"),
            &Sha256Verifier,
        )
        .err()
        .expect("checksum mismatch");
    assert!(matches!(error, RegistryError::Checksum { .. }), "{error}");

    // Wrong ABI is rejected.
    let abi_index = RegistryIndex::from_json(&json!({
        "plugins": {
            "demo-greeter": [
                { "version": "1.0.0", "artifact": fixture_cdylib.to_str().unwrap(),
                  "sha256": digest, "abi": 999 }
            ]
        }
    }))
    .expect("abi index");
    let error = repo
        .install(
            &abi_index,
            "demo-greeter",
            &Constraint::parse("*").expect("constraint"),
            &Sha256Verifier,
        )
        .err()
        .expect("abi mismatch");
    assert!(matches!(error, RegistryError::Abi { .. }), "{error}");
}

fn build_fixture() -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/demo-plugin");
    let options = BuildOptions {
        offline: true,
        target_dir: Some(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../target/loader-integration"),
        ),
    };
    Loader::build(&fixture, &options).expect("build fixture")
}

fn temp_cache_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/registry-test")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    dir
}
