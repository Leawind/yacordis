//! Loading prebuilt cdylib components and adapting them into the core.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use libloading::Library;
use serde_json::Value;

use yacordis_component_api::{ABI_VERSION, Callback, RawBuffer};

use crate::adapter::{FfiComponent, SendCallback};
use crate::lease::{LibraryLease, UnloadError};

/// Why a component could not be installed.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be opened as a shared library.
    Library(libloading::Error),
    /// A required cordis export symbol is missing.
    Symbol(&'static str, libloading::Error),
    /// The component's `yacordis_abi_version` differs from the host's.
    AbiMismatch {
        /// The version the component reported.
        found: u32,
    },
    /// The exported manifest is missing fields or is not valid JSON.
    Manifest(String),
    /// Building from source failed; carries cargo's stderr.
    Build(String),
    /// Fetching from a URL failed.
    Fetch(String),
    /// Copying the artifact into the materialization cache failed.
    Materialize(std::io::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Library(e) => write!(f, "cannot load component library: {e}"),
            LoadError::Symbol(name, e) => {
                write!(f, "missing cordis export `{name}`: {e}")
            }
            LoadError::AbiMismatch { found } => write!(
                f,
                "component ABI version {found} does not match host version {ABI_VERSION}; rebuild the component"
            ),
            LoadError::Manifest(why) => write!(f, "invalid component manifest: {why}"),
            LoadError::Build(stderr) => {
                write!(f, "cargo build failed:\n{stderr}")
            }
            LoadError::Fetch(why) => write!(f, "download failed: {why}"),
            LoadError::Materialize(e) => {
                write!(f, "cannot materialize the component artifact: {e}")
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Library(e) | LoadError::Symbol(_, e) => Some(e),
            LoadError::Materialize(e) => Some(e),
            _ => None,
        }
    }
}

/// A parsed component manifest.
#[derive(Debug, Clone)]
pub struct ComponentManifest {
    /// Human-readable component name, if the component declares one.
    pub name: Option<String>,
    /// Required service names (they gate activation).
    pub required: Vec<String>,
    /// Optional service names (they never gate activation).
    pub optional: Vec<String>,
}

/// A copy of a component artifact, ready to be mapped.
///
/// A published copy lives at `<cache>/<sha256>.<ext>`, where `<ext>` is
/// the platform's shared-library extension, and is shared by digest:
/// identical content reuses the existing copy, and a new copy is written
/// through a temporary file renamed into place. The private copy of an
/// ephemeral load ([`Loader::load_materialized_ephemeral`],
/// [`Loader::load_bytes_ephemeral`]) is temp-named instead and exists
/// only while it is mapped.
#[derive(Debug, Clone)]
pub struct MaterializedArtifact {
    source: Option<PathBuf>,
    path: PathBuf,
    digest: String,
}

impl MaterializedArtifact {
    /// The artifact the copy was made from; `None` for
    /// [`Loader::load_bytes`].
    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The cached copy to map.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lowercase hex sha-256 of the artifact bytes.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// One dynamically loaded component binary.
///
/// The library stays mapped while it can still run component code: every
/// install of this [`ComponentLibrary`] shares the same mapped binary and
/// can be disposed and reinstalled independently. Once every fiber is
/// disposed, [`ComponentLibrary::unload`] unmaps it for real — or refuses
/// and degrades to keep-mapped when the quiet state cannot be proven.
pub struct ComponentLibrary {
    lease: Arc<LibraryLease>,
    /// Parsed from the component's exported manifest.
    pub manifest: ComponentManifest,
    adapter: Arc<FfiComponent>,
    /// Provenance of a materialized load; `None` for in-place loads.
    artifact: Option<MaterializedArtifact>,
}

// SAFETY: the component ABI contract requires closures and state to be
// usable from any thread; the lease serializes library access.
unsafe impl Send for ComponentLibrary {}
unsafe impl Sync for ComponentLibrary {}

impl ComponentLibrary {
    /// Load and verify a component binary. Loading is trusted code
    /// execution — only install components you could have required anyway.
    ///
    /// # Safety
    /// This runs the library's constructors and links its symbols; a
    /// hostile binary is arbitrary code execution. The safety obligation
    /// is on the caller's choice of file, not on FFI handling here.
    pub unsafe fn load(path: &Path) -> Result<Self, LoadError> {
        // SAFETY: delegated to the caller's choice of binary (see the
        // fn-level safety contract).
        let library = unsafe { Library::new(path).map_err(LoadError::Library)? };

        let version = unsafe {
            library
                .get::<unsafe extern "C" fn() -> u32>(b"yacordis_abi_version\0")
                .map_err(|e| LoadError::Symbol("yacordis_abi_version", e))?
        };
        let found = unsafe { version() };
        if found != ABI_VERSION {
            return Err(LoadError::AbiMismatch { found });
        }

        // Every export call below runs while the library is still owned
        // here; a failing step drops it and unmaps the binary again.
        let manifest = unsafe {
            library
                .get::<extern "C" fn() -> RawBuffer>(b"yacordis_component_manifest\0")
                .map_err(|e| LoadError::Symbol("yacordis_component_manifest", e))?
        };
        let manifest_buffer = manifest();
        let manifest_text = yacordis_component_api::buffer_to_string(manifest_buffer);
        yacordis_component_api::free_buffer(manifest_buffer);
        let doc: Value =
            serde_json::from_str(&manifest_text).map_err(|e| LoadError::Manifest(e.to_string()))?;
        let parsed = ComponentManifest {
            name: doc["name"].as_str().map(str::to_owned),
            required: string_list(&doc["inject"]["required"]),
            optional: string_list(&doc["inject"]["optional"]),
        };

        let apply = unsafe {
            library
                .get::<extern "C" fn() -> Callback>(b"yacordis_component_apply\0")
                .map_err(|e| LoadError::Symbol("yacordis_component_apply", e))?
        }();

        // The lease owns the mapping from here on: every component callback
        // in the host is gated through it, and unload goes through it.
        let lease = LibraryLease::new(library, path.to_path_buf());
        let adapter = FfiComponent::create(
            parsed.name.clone(),
            inject_of(&parsed),
            SendCallback::rooted(apply, lease.clone()),
            lease.clone(),
        );

        Ok(Self {
            lease,
            manifest: parsed,
            adapter,
            artifact: None,
        })
    }

    /// The erased component: pass to `ctx.component_dyn` or install directly.
    ///
    /// After a successful [`ComponentLibrary::unload`] the returned adapter
    /// is inert: installing it yields a `Failed` fiber with a clear error
    /// instead of a jump into unmapped code.
    #[must_use]
    pub fn component(&self) -> Arc<dyn yacordis::Component> {
        self.adapter.clone()
    }

    /// Install into a context (the component's `inject` gates activation).
    ///
    /// The fiber is tracked for unload evidence; disposing it (and every
    /// other fiber of this library) is what makes [`ComponentLibrary::unload`]
    /// legal.
    pub fn install(
        &self,
        ctx: &yacordis::Context,
        config: impl Into<serde_json::Value>,
    ) -> Result<yacordis::FiberHandle, yacordis::CordisError> {
        if self.lease.is_closed() {
            return Err(yacordis::CordisError::InactiveContext {
                action: "install a component from an unloaded library",
                fiber: self.manifest.name.clone(),
            });
        }
        let handle = ctx.component_dyn(self.component(), config)?;
        self.lease.register_fiber(
            handle.clone(),
            handle.name().or_else(|| self.manifest.name.clone()),
        );
        Ok(handle)
    }

    /// Unmap the component binary from this process.
    ///
    /// Unloading is only allowed on a proven-quiet state; anything less is
    /// refused with the violation named, and the library stays mapped (the
    /// keep-mapped backstop). The evidence, checked atomically against new
    /// component-code entries:
    ///
    /// 1. no component code in flight anywhere (in-flight framework calls
    ///    count too — dispose drains them; a call that escaped the
    ///    framework is an I1 violation);
    /// 2. no fiber installed from this library is left undisposed;
    /// 3. no component instance is held outside the library's own adapter.
    ///
    /// Before the checks the host waits for every lifecycle cascade in the
    /// process to land ([`yacordis::quiesce`]), so concurrent interleaving
    /// cannot leave the evidence stale — skipped when the unload itself
    /// runs inside a lifecycle op (a disposer uninstalling its own library
    /// mid-cascade), where the checks still guard correctness.
    ///
    /// On success the binary is unmapped: on Windows the artifact file
    /// becomes writable again, so the dev loop can rebuild and reload in
    /// place. The call is idempotent; unloading twice returns `Ok`.
    ///
    /// # Not a sandbox
    /// A component that invisibly violates I1 (raw threads running its own
    /// code without framework mediation) cannot be detected by the host;
    /// unloading such a binary is undefined behavior. Unload only
    /// lint-clean components you trust.
    pub fn unload(&self) -> Result<(), UnloadError> {
        if self.lease.is_closed() {
            return Ok(());
        }
        // Refuse fast while component code is in flight, before any wait.
        self.lease.check_in_flight()?;
        if !yacordis::executing_lifecycle_op() {
            yacordis::quiesce();
        }
        // Final sweep of the library's own residue: after every fiber of
        // this library is disposed, its context handles must leave the
        // table, so the lifecycle `install → use → dispose → unload`
        // really returns the process to the state before the load.
        crate::vtable::handles().sweep_dead_contexts();
        self.lease.close(&self.adapter)
    }

    /// Whether the library is unloaded (or sealed after a failed unmap).
    #[must_use]
    pub fn is_unloaded(&self) -> bool {
        self.lease.is_closed()
    }

    /// The file this library was loaded from (for diagnostics).
    ///
    /// For a materialized load this is the cached copy, not the producer's
    /// artifact; [`ComponentLibrary::source_path`] keeps the latter.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.lease.path()
    }

    /// The sha-256 digest of the artifact, for a materialized load.
    ///
    /// In-place loads ([`Loader::load_file`]) return `None`.
    #[must_use]
    pub fn digest(&self) -> Option<&str> {
        self.artifact
            .as_ref()
            .map(|artifact| artifact.digest.as_str())
    }

    /// The producer's artifact path, for a materialized load.
    ///
    /// `None` for in-place loads and for [`Loader::load_bytes`], which has
    /// no source file.
    #[must_use]
    pub fn source_path(&self) -> Option<&Path> {
        self.artifact
            .as_ref()
            .and_then(|artifact| artifact.source.as_deref())
    }
}

fn inject_of(parsed: &ComponentManifest) -> yacordis::Inject {
    let mut inject = yacordis::Inject::none();
    for name in &parsed.required {
        inject = inject.req(name.clone());
    }
    for name in &parsed.optional {
        inject = inject.opt(name.clone());
    }
    inject
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|array| {
            array
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Options for [`Loader::build`].
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// Pass `--offline` to cargo (use a warm cache; no network).
    pub offline: bool,
    /// Override the cargo target directory.
    pub target_dir: Option<PathBuf>,
    /// Run the ast-grep unload-contract lint over the source tree
    /// before building; findings reject the build. Silently skipped when
    /// ast-grep is not installed or the bundled rules are unavailable.
    pub lint: bool,
}

/// Entry points for obtaining [`ComponentLibrary`] binaries.
pub struct Loader;

/// The bundled ast-grep configuration for the unload-contract rules,
/// embedded at compile time and materialized to a temporary
/// tree when the lint runs — the crate carries no build-machine paths,
/// so host binaries embedding these strings stay distributable.
const LINT_SGCONFIG: &str = include_str!("../ast-grep-rules/sgconfig.yml");
const LINT_RULES: &[(&str, &str)] = &[
    (
        "yacordis-no-ffi-alloc.yml",
        include_str!("../ast-grep-rules/rules/yacordis-no-ffi-alloc.yml"),
    ),
    (
        "yacordis-no-libc-exit-hooks.yml",
        include_str!("../ast-grep-rules/rules/yacordis-no-libc-exit-hooks.yml"),
    ),
    (
        "yacordis-no-mutable-static.yml",
        include_str!("../ast-grep-rules/rules/yacordis-no-mutable-static.yml"),
    ),
    (
        "yacordis-no-raw-thread-spawn.yml",
        include_str!("../ast-grep-rules/rules/yacordis-no-raw-thread-spawn.yml"),
    ),
    (
        "yacordis-no-tokio-scheduling.yml",
        include_str!("../ast-grep-rules/rules/yacordis-no-tokio-scheduling.yml"),
    ),
];

/// Materialize the embedded rule set into a per-process temporary tree
/// (ast-grep reads its configuration from disk; `ruleDirs` resolves
/// relative to the config file). Keyed by process id under the system
/// temp dir and left to the OS cleanup.
///
/// Returns `None` when the tree cannot be written (read-only temp dir,
/// full disk, security software): the lint is advisory, so an
/// unavailable rule tree degrades exactly like a missing ast-grep
/// instead of failing the load path.
fn lint_config_tree() -> Option<String> {
    use std::sync::OnceLock;
    static TREE: OnceLock<Option<String>> = OnceLock::new();
    TREE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("yacordis-lint-rules-{}", std::process::id()));
        write_rule_tree(&root)
            .ok()
            .map(|()| root.join("sgconfig.yml").to_string_lossy().into_owned())
    })
    .clone()
}

/// Write the embedded rule set under `root`. Split out of
/// [`lint_config_tree`] (whose `OnceLock` makes both outcomes
/// unreachable in one process) so the writable and unwritable paths are
/// both testable.
fn write_rule_tree(root: &Path) -> std::io::Result<()> {
    let rules = root.join("rules");
    std::fs::create_dir_all(&rules)?;
    std::fs::write(root.join("sgconfig.yml"), LINT_SGCONFIG)?;
    for (name, text) in LINT_RULES {
        std::fs::write(rules.join(name), text)?;
    }
    Ok(())
}

/// Run the bundled ast-grep unload-contract lint over a component source
/// tree. Returns the combined findings (rejecting the build) when the
/// linter is installed and reports violations; returns `Ok` when
/// ast-grep is unavailable (the lint is best-effort).
pub fn lint_source(source_dir: impl AsRef<Path>) -> Result<(), String> {
    let Some(config) = lint_config_tree() else {
        // Rule tree unavailable: nothing to lint with.
        return Ok(());
    };
    let output = std::process::Command::new("ast-grep")
        .args(["scan", "--config", &config])
        .arg(source_dir.as_ref())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    let Ok(output) = output else {
        // ast-grep not installed: best-effort lint skips.
        return Ok(());
    };
    if output.status.success() {
        return Ok(());
    }
    let findings = String::from_utf8_lossy(&output.stdout).to_string();
    if findings.trim().is_empty() {
        // Non-zero exit without findings: the linter itself failed.
        return Ok(());
    }
    Err(findings)
}

impl Loader {
    /// Load a prebuilt cdylib component in place. See
    /// [`ComponentLibrary::load`] for the trust model.
    ///
    /// The artifact path is the mapped file: rebuilding it is only possible
    /// after [`ComponentLibrary::unload`]. A host that rebuilds before
    /// unloading loads a materialized copy instead
    /// ([`Loader::load_materialized`]).
    pub fn load_file(path: impl AsRef<Path>) -> Result<ComponentLibrary, LoadError> {
        // SAFETY: delegated to the caller's choice of component binary.
        unsafe { ComponentLibrary::load(path.as_ref()) }
    }

    /// Copy `source` into `cache_dir` under its sha-256 digest.
    ///
    /// The copy is named by its digest with the platform's shared-library
    /// extension; identical content reuses the existing copy. A new copy is
    /// written to a temporary file and renamed into place, so a crash or a
    /// concurrent materialize never exposes a partial artifact.
    ///
    /// Components loaded from a copy must be self-contained: a relative
    /// loader search path (`$ORIGIN`, `@loader_path`) resolves next to the
    /// copy, not next to the producer's artifact.
    pub fn materialize(
        source: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<MaterializedArtifact, LoadError> {
        let source = source.as_ref();
        let cache_dir = cache_dir.as_ref();
        std::fs::create_dir_all(cache_dir).map_err(LoadError::Materialize)?;
        let mut input = std::fs::File::open(source).map_err(LoadError::Materialize)?;
        let temp = temp_path(cache_dir);
        let digest = write_copy(&mut input, &temp)?;
        let path = cache_path(cache_dir, &digest);
        publish(&temp, &path)?;
        Ok(MaterializedArtifact {
            source: Some(source.to_path_buf()),
            path,
            digest,
        })
    }

    /// Materialize `source` and map the copy.
    ///
    /// Use this when the artifact may be rebuilt while a previous copy is
    /// still mapped: the producer's path is never mapped, so the rebuild
    /// cannot hit the Windows image lock, and cannot truncate the bytes
    /// under an existing mapping on Unix. Fibers of the old copy keep
    /// serving until they are disposed.
    pub fn load_materialized(
        source: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<ComponentLibrary, LoadError> {
        load_artifact(Self::materialize(source, cache_dir)?)
    }

    /// Materialize an in-memory component binary and map the copy.
    ///
    /// This is the embedded-artifact path; identical bytes reuse the same
    /// cached copy.
    pub fn load_bytes(
        bytes: impl AsRef<[u8]>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<ComponentLibrary, LoadError> {
        load_artifact(materialize_bytes(bytes.as_ref(), cache_dir.as_ref())?)
    }

    /// Materialize `source` into a private copy, map it, and leave nothing
    /// behind once the mapping is gone — or when the load fails.
    ///
    /// The copy is private to this load, which is what makes the cleanup
    /// unconditional: POSIX unlinks it as soon as the mapping exists (the
    /// contents stay live through the mapping), and a shared digest name
    /// would instead race a concurrent load's open against this unlink.
    /// Windows cannot delete a mapped image, so there the copy goes when
    /// the library unloads. A failed load maps nothing, so the copy is
    /// deleted on both platforms; a crashed process leaves its copy for
    /// [`Loader::prune_materialized`], which ages out temp-named files.
    ///
    /// Use [`Loader::load_materialized`] when one shared copy across loads
    /// matters more than leaving no files. The self-containment requirement
    /// of [`Loader::load_materialized`] applies.
    pub fn load_materialized_ephemeral(
        source: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<ComponentLibrary, LoadError> {
        let source = source.as_ref();
        let cache_dir = cache_dir.as_ref();
        load_ephemeral(materialize_ephemeral(source, cache_dir)?)
    }

    /// Materialize an in-memory component binary into a private copy, map
    /// it, and leave nothing behind once the mapping is gone.
    ///
    /// The embedded-artifact counterpart of
    /// [`Loader::load_materialized_ephemeral`].
    pub fn load_bytes_ephemeral(
        bytes: impl AsRef<[u8]>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<ComponentLibrary, LoadError> {
        let cache_dir = cache_dir.as_ref();
        load_ephemeral(materialize_ephemeral_bytes(bytes.as_ref(), cache_dir)?)
    }

    /// Delete cached copies the cache no longer needs.
    ///
    /// Two namespaces live in a materialization cache:
    ///
    /// - **Digest-named copies** (`<sha256>.<ext>`) form the shareable
    ///   cache: a copy whose digest is not in `keep` is deleted. Digests
    ///   are the lowercase hex strings from
    ///   [`MaterializedArtifact::digest`] and [`ComponentLibrary::digest`].
    /// - **Temp-named copies** (`.<pid>-…-….tmp`, also what ephemeral
    ///   loads map) are never shareable, so one older than the grace
    ///   (`TEMP_GRACE`) is a crash leftover and is deleted; a fresh one
    ///   may be an in-flight write by another process and is left alone.
    ///
    /// Deletion is best effort: a copy still mapped by a live process
    /// cannot be removed on Windows, so it stays. Foreign files are left
    /// untouched.
    pub fn prune_materialized(cache_dir: impl AsRef<Path>, keep: &[&str]) -> std::io::Result<()> {
        let cache_dir = cache_dir.as_ref();
        let entries = match std::fs::read_dir(cache_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if is_temp_name(name) {
                // Past the grace, a temp copy cannot be in flight: it is a
                // crash leftover. Best effort: a still-mapped copy on
                // Windows refuses deletion and stays.
                if older_than(&entry, TEMP_GRACE) {
                    let _ = std::fs::remove_file(entry.path());
                }
                continue;
            }
            let Some((stem, _extension)) = name.split_once('.') else {
                continue;
            };
            let is_digest = stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_hexdigit());
            if !is_digest || keep.contains(&stem) {
                continue;
            }
            // Best effort: a mapped copy on Windows refuses deletion.
            let _ = std::fs::remove_file(entry.path());
        }
        Ok(())
    }

    /// Build a component crate from source and load the resulting cdylib.
    ///
    /// This is the developer path: running `cargo` inside the host
    /// process mirrors cordis's write-and-build story. End users normally
    /// receive prebuilt binaries via [`Loader::load_file`] or
    /// `Loader::fetch`.
    pub fn build_and_load(
        source_dir: impl AsRef<Path>,
        options: &BuildOptions,
    ) -> Result<ComponentLibrary, LoadError> {
        let path = Self::build(source_dir, options)?;
        Self::load_file(path)
    }

    /// Build a component crate, returning the cdylib artifact path.
    pub fn build(
        source_dir: impl AsRef<Path>,
        options: &BuildOptions,
    ) -> Result<PathBuf, LoadError> {
        if options.lint {
            let dir = source_dir.as_ref().to_path_buf();
            lint_source(dir).map_err(LoadError::Build)?;
        }
        let manifest_path = source_dir.as_ref().join("Cargo.toml");
        let mut command = std::process::Command::new(
            std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()),
        );
        command
            .args(["build", "--release", "--manifest-path"])
            .arg(&manifest_path)
            .arg("--message-format=json")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(target_dir) = &options.target_dir {
            command.env("CARGO_TARGET_DIR", target_dir);
        }
        if options.offline {
            command.env("CARGO_NET_OFFLINE", "true");
        }
        let output = command
            .output()
            .map_err(|e| LoadError::Build(format!("failed to spawn cargo: {e}")))?;
        let mut diagnostics = String::new();
        let mut artifact: Option<PathBuf> = None;
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if message["reason"].as_str() == Some("compiler-message") {
                if let Some(rendered) = message["message"]["rendered"].as_str() {
                    diagnostics.push_str(rendered);
                }
                continue;
            }
            if message["reason"].as_str() != Some("compiler-artifact") {
                continue;
            }
            let is_cdylib = message["target"]["kind"]
                .as_array()
                .map(|kinds| kinds.iter().any(|k| k.as_str() == Some("cdylib")))
                .unwrap_or(false);
            if is_cdylib {
                if let Some(path) = message["filenames"].as_array().and_then(|f| f.first()) {
                    artifact = Some(PathBuf::from(path.as_str().unwrap_or_default()));
                }
            }
        }
        artifact.ok_or_else(|| {
            let detail = diagnostics.trim();
            let stderr = String::from_utf8_lossy(&output.stderr);
            LoadError::Build(format!(
                "cargo produced no cdylib artifact; set crate-type = [\"cdylib\"]{}",
                if detail.is_empty() && stderr.trim().is_empty() {
                    String::new()
                } else if detail.is_empty() {
                    format!("\n{stderr}")
                } else {
                    format!("\n{detail}")
                }
            ))
        })
    }

    /// Download a component binary into `cache_dir` and return its path.
    /// Requires the `download` feature.
    #[cfg(feature = "download")]
    pub fn fetch(url: &str, cache_dir: impl AsRef<Path>) -> Result<PathBuf, LoadError> {
        let response = ureq::get(url)
            .call()
            .map_err(|e| LoadError::Fetch(e.to_string()))?;
        let file_name = url.rsplit('/').next().unwrap_or("component.so");
        std::fs::create_dir_all(cache_dir.as_ref())
            .map_err(|e| LoadError::Fetch(format!("cannot create cache dir: {e}")))?;
        let path = cache_dir.as_ref().join(file_name);
        let mut reader = response.into_reader();
        let mut file = std::fs::File::create(&path)
            .map_err(|e| LoadError::Fetch(format!("cannot create component file: {e}")))?;
        std::io::copy(&mut reader, &mut file)
            .map_err(|e| LoadError::Fetch(format!("download interrupted: {e}")))?;
        Ok(path)
    }
}

fn load_artifact(artifact: MaterializedArtifact) -> Result<ComponentLibrary, LoadError> {
    // SAFETY: same trust model as `Loader::load_file`: the caller chose the
    // artifact.
    let mut library = unsafe { ComponentLibrary::load(&artifact.path)? };
    library.artifact = Some(artifact);
    Ok(library)
}

/// Load a materialized copy that must not outlive its mapping.
///
/// The copy is private to this load (see `materialize_ephemeral*`), which
/// is what makes the cleanup unconditional: a failed load maps nothing —
/// a failed `dlopen`, or the dropped `Library` after a failed symbol
/// lookup — so the copy is garbage and is deleted on both platforms. On
/// success POSIX drops the directory entry at once (the mapping keeps the
/// contents alive); Windows cannot delete a mapped image, so the lease
/// removes the copy when the library unmaps.
fn load_ephemeral(artifact: MaterializedArtifact) -> Result<ComponentLibrary, LoadError> {
    let path = artifact.path.clone();
    match load_artifact(artifact) {
        Ok(library) => {
            #[cfg(windows)]
            library.lease.mark_remove_on_close();
            #[cfg(not(windows))]
            let _ = std::fs::remove_file(&path);
            Ok(library)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&path);
            Err(error)
        }
    }
}

fn materialize_bytes(bytes: &[u8], cache_dir: &Path) -> Result<MaterializedArtifact, LoadError> {
    std::fs::create_dir_all(cache_dir).map_err(LoadError::Materialize)?;
    let digest = crate::digest::sha256_hex(bytes);
    let path = cache_path(cache_dir, &digest);
    if !path.is_file() {
        let temp = temp_path(cache_dir);
        write_copy(&mut std::io::Cursor::new(bytes), &temp)?;
        publish(&temp, &path)?;
    }
    Ok(MaterializedArtifact {
        source: None,
        path,
        digest,
    })
}

/// Materialize a private copy for an ephemeral load.
///
/// The copy is written straight to a temp-named file instead of a shared
/// digest name. That is what makes ephemeral cleanup unconditional (a
/// private copy is always the caller's to delete), and it avoids a race
/// the digest name would add off Windows: this load unlinks the copy as
/// soon as it is mapped, which would make a concurrent load of the same
/// content fail to open the very copy it just published. The name is
/// temp-namespaced, so [`Loader::prune_materialized`] ages out what a
/// crashed process leaves behind.
fn materialize_ephemeral(
    source: &Path,
    cache_dir: &Path,
) -> Result<MaterializedArtifact, LoadError> {
    std::fs::create_dir_all(cache_dir).map_err(LoadError::Materialize)?;
    let mut input = std::fs::File::open(source).map_err(LoadError::Materialize)?;
    let path = temp_path(cache_dir);
    let digest = write_copy(&mut input, &path)?;
    Ok(MaterializedArtifact {
        source: Some(source.to_path_buf()),
        path,
        digest,
    })
}

/// The in-memory counterpart of [`materialize_ephemeral`].
fn materialize_ephemeral_bytes(
    bytes: &[u8],
    cache_dir: &Path,
) -> Result<MaterializedArtifact, LoadError> {
    std::fs::create_dir_all(cache_dir).map_err(LoadError::Materialize)?;
    let path = temp_path(cache_dir);
    let digest = write_copy(&mut std::io::Cursor::new(bytes), &path)?;
    Ok(MaterializedArtifact {
        source: None,
        path,
        digest,
    })
}

fn cache_path(cache_dir: &Path, digest: &str) -> PathBuf {
    cache_dir.join(format!("{digest}.{}", std::env::consts::DLL_EXTENSION))
}

/// Move a finished temporary file to its digest name, reusing a copy a
/// concurrent loader published first.
fn publish(temp: &Path, path: &Path) -> Result<(), LoadError> {
    if path.is_file() {
        let _ = std::fs::remove_file(temp);
        return Ok(());
    }
    match std::fs::rename(temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Either another materialize won the race, or the existing copy
            // is mapped and Windows refused the replacement.
            let published = path.is_file();
            let _ = std::fs::remove_file(temp);
            if published {
                Ok(())
            } else {
                Err(LoadError::Materialize(error))
            }
        }
    }
}

/// Write `input` to a new temporary file, hashing the bytes in the same
/// pass. A failed write removes the partial file.
fn write_copy(input: &mut dyn std::io::Read, temp: &Path) -> Result<String, LoadError> {
    let result = (|| -> std::io::Result<String> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temp)?;
        let mut hasher = crate::digest::Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read])?;
        }
        output.sync_all()?;
        Ok(hasher.finish_hex())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result.map_err(LoadError::Materialize)
}

fn temp_path(cache_dir: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    cache_dir.join(format!(
        ".{}-{nanos}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// The temp namespace [`temp_path`] writes into and ephemeral loads map:
/// never a shareable copy, so [`Loader::prune_materialized`] may age it.
fn is_temp_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp")
}

/// A temp-named copy older than this is a crash leftover, not an
/// in-flight write: writing a copy takes seconds, so the margin is wide.
const TEMP_GRACE: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Whether `entry` was last modified at least `age` ago. A future or
/// unreadable mtime reads as "not old": pruning never acts on a file it
/// cannot date.
fn older_than(entry: &std::fs::DirEntry, age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|elapsed| elapsed >= age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("yacordis-lint-{tag}-{}", std::process::id()))
    }

    #[test]
    fn rule_tree_writes_every_rule_to_a_writable_root() {
        let root = scratch_path("tree");
        let _ = std::fs::remove_dir_all(&root);
        write_rule_tree(&root).expect("writable temp dir");
        assert!(root.join("sgconfig.yml").is_file(), "missing sgconfig.yml");
        for (name, _) in LINT_RULES {
            assert!(root.join("rules").join(name).is_file(), "missing {name}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unwritable_rule_tree_degrades_instead_of_panicking() {
        // A root whose parent is a regular file cannot be created on any
        // platform: the lint must report "unavailable", never panic the
        // load path (the regression: three `expect`s did).
        let blocker = scratch_path("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker");
        let unwritable = blocker.join("rules-root");
        assert!(write_rule_tree(&unwritable).is_err());
        let _ = std::fs::remove_file(&blocker);
    }
}
