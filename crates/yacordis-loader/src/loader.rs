//! Loading prebuilt cdylib components and adapting them into the core.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use serde_json::Value;

use yacordis_component_api::{ABI_VERSION, Callback, RawBuffer};

use crate::adapter::{FfiComponent, SendCallback};

/// Why a component could not be installed.
#[derive(Debug)]
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
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Library(e) | LoadError::Symbol(_, e) => Some(e),
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

/// One dynamically loaded component binary.
///
/// The library stays mapped for the life of the process (see
/// `docs/design.md` §4 for the unload policy); every install of this
/// [`ComponentLibrary`] shares the same mapped code and can be disposed and
/// reinstalled independently.
pub struct ComponentLibrary {
    #[allow(dead_code)]
    library: &'static Library,
    /// Parsed from the component's exported manifest.
    pub manifest: ComponentManifest,
    adapter: Arc<FfiComponent>,
}

// SAFETY: the component ABI contract requires closures and state to be
// usable from any thread; the Library handle is process-global.
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

        // Leak the Library: the unload policy is keep-mapped, so symbols
        // (and the apply callback) stay valid for the process lifetime.
        let library: &'static Library = Box::leak(Box::new(library));

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

        let inject = {
            let mut inject = yacordis::Inject::none();
            for name in &parsed.required {
                inject = inject.req(name.clone());
            }
            for name in &parsed.optional {
                inject = inject.opt(name.clone());
            }
            inject
        };
        let adapter = Arc::new(FfiComponent::new(
            parsed.name.clone(),
            inject,
            SendCallback::new(apply),
        ));

        Ok(Self {
            library,
            manifest: parsed,
            adapter,
        })
    }

    /// The erased component: pass to `ctx.component_dyn` or install directly.
    #[must_use]
    pub fn component(&self) -> Arc<dyn yacordis::Component> {
        self.adapter.clone()
    }

    /// Install into a context (the component's `inject` gates activation).
    pub fn install(
        &self,
        ctx: &yacordis::Context,
        config: impl Into<serde_json::Value>,
    ) -> Result<yacordis::FiberHandle, yacordis::CordisError> {
        ctx.component_dyn(self.component(), config)
    }
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
    /// Run the ast-grep unload-contract lint (§4.1) over the source tree
    /// before building; findings reject the build. Silently skipped when
    /// ast-grep is not installed or the bundled rules are unavailable.
    pub lint: bool,
}

/// Entry points for obtaining [`ComponentLibrary`] binaries.
pub struct Loader;

/// The bundled ast-grep configuration for the unload-contract rules
/// (§4.1), embedded at compile time and materialized to a temporary
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
fn lint_config_tree() -> String {
    use std::sync::OnceLock;
    static TREE: OnceLock<String> = OnceLock::new();
    TREE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("yacordis-lint-rules-{}", std::process::id()));
        let rules = root.join("rules");
        std::fs::create_dir_all(&rules).expect("create the lint rule tree");
        std::fs::write(root.join("sgconfig.yml"), LINT_SGCONFIG).expect("write sgconfig.yml");
        for (name, text) in LINT_RULES {
            std::fs::write(rules.join(name), text).expect("write the lint rule");
        }
        root.join("sgconfig.yml").to_string_lossy().into_owned()
    })
    .clone()
}

/// Run the bundled ast-grep unload-contract lint over a component source
/// tree. Returns the combined findings (rejecting the build) when the
/// linter is installed and reports violations; returns `Ok` when
/// ast-grep is unavailable (the lint is best-effort).
pub fn lint_source(source_dir: impl AsRef<Path>) -> Result<(), String> {
    let output = std::process::Command::new("ast-grep")
        .args(["scan", "--config", &lint_config_tree()])
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
    /// Load a prebuilt cdylib component. See [`ComponentLibrary::load`] for
    /// the trust model.
    pub fn load_file(path: impl AsRef<Path>) -> Result<ComponentLibrary, LoadError> {
        // SAFETY: delegated to the caller's choice of component binary.
        unsafe { ComponentLibrary::load(path.as_ref()) }
    }

    /// Build a component crate from source and load the resulting cdylib.
    ///
    /// This is the developer path — running `cargo` inside the host
    /// process — mirroring cordis's write-and-build story; end users
    /// normally receive prebuilt binaries via [`Loader::load_file`] or
    /// [`Loader::fetch`].
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
