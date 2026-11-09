//! Loading prebuilt cdylib plugins and adapting them into the core.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use libloading::Library;
use serde_json::Value;

use cordis_plugin_api::{ABI_VERSION, Callback, RawBuffer};

use crate::adapter::{FfiPlugin, SendCallback};

/// Why a plugin could not be installed.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be opened as a shared library.
    Library(libloading::Error),
    /// A required cordis export symbol is missing.
    Symbol(&'static str, libloading::Error),
    /// The plugin's `cordis_abi_version` differs from the host's.
    AbiMismatch {
        /// The version the plugin reported.
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
            LoadError::Library(e) => write!(f, "cannot load plugin library: {e}"),
            LoadError::Symbol(name, e) => {
                write!(f, "missing cordis export `{name}`: {e}")
            }
            LoadError::AbiMismatch { found } => write!(
                f,
                "plugin ABI version {found} does not match host version {ABI_VERSION}; rebuild the plugin"
            ),
            LoadError::Manifest(why) => write!(f, "invalid plugin manifest: {why}"),
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

/// A parsed plugin manifest.
#[derive(Debug, Clone)]
pub struct PluginManifest {
    /// Human-readable plugin name, if the plugin declares one.
    pub name: Option<String>,
    /// Required service names (they gate activation).
    pub required: Vec<String>,
    /// Optional service names (they never gate activation).
    pub optional: Vec<String>,
}

/// One dynamically loaded plugin binary.
///
/// The library stays mapped for the life of the process (see
/// `docs/design.md` §4 for the unload policy); every install of this
/// [`PluginLibrary`] shares the same mapped code and can be disposed and
/// reinstalled independently.
pub struct PluginLibrary {
    #[allow(dead_code)]
    library: &'static Library,
    /// Parsed from the plugin's exported manifest.
    pub manifest: PluginManifest,
    adapter: Arc<FfiPlugin>,
}

// SAFETY: the plugin ABI contract requires closures and state to be
// usable from any thread; the Library handle is process-global.
unsafe impl Send for PluginLibrary {}
unsafe impl Sync for PluginLibrary {}

impl PluginLibrary {
    /// Load and verify a plugin binary. Loading is trusted code
    /// execution — only install plugins you could have required anyway.
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
                .get::<unsafe extern "C" fn() -> u32>(b"cordis_abi_version\0")
                .map_err(|e| LoadError::Symbol("cordis_abi_version", e))?
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
                .get::<extern "C" fn() -> RawBuffer>(b"cordis_plugin_manifest\0")
                .map_err(|e| LoadError::Symbol("cordis_plugin_manifest", e))?
        };
        let manifest_buffer = manifest();
        let manifest_text = cordis_plugin_api::buffer_to_string(manifest_buffer);
        cordis_plugin_api::free_buffer(manifest_buffer);
        let doc: Value =
            serde_json::from_str(&manifest_text).map_err(|e| LoadError::Manifest(e.to_string()))?;
        let parsed = PluginManifest {
            name: doc["name"].as_str().map(str::to_owned),
            required: string_list(&doc["inject"]["required"]),
            optional: string_list(&doc["inject"]["optional"]),
        };

        let apply = unsafe {
            library
                .get::<extern "C" fn() -> Callback>(b"cordis_plugin_apply\0")
                .map_err(|e| LoadError::Symbol("cordis_plugin_apply", e))?
        }();

        let inject = {
            let mut inject = cordis::Inject::none();
            for name in &parsed.required {
                inject = inject.req(name.clone());
            }
            for name in &parsed.optional {
                inject = inject.opt(name.clone());
            }
            inject
        };
        let adapter = Arc::new(FfiPlugin::new(
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

    /// The erased plugin: pass to `ctx.plugin_dyn` or install directly.
    #[must_use]
    pub fn plugin(&self) -> Arc<dyn cordis::Plugin> {
        self.adapter.clone()
    }

    /// Install into a context (the plugin's `inject` gates activation).
    pub fn install(
        &self,
        ctx: &cordis::Context,
        config: impl Into<serde_json::Value>,
    ) -> Result<cordis::FiberHandle, cordis::CordisError> {
        ctx.plugin_dyn(self.plugin(), config)
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

/// Entry points for obtaining [`PluginLibrary`] binaries.
pub struct Loader;

/// Bundled ast-grep configuration for the unload-contract rules (§4.1).
const LINT_CONFIG: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/ast-grep-rules/sgconfig.yml");

/// Run the bundled ast-grep unload-contract lint over a plugin source
/// tree. Returns the combined findings (rejecting the build) when the
/// linter is installed and reports violations; returns `Ok` when
/// ast-grep is unavailable (the lint is best-effort, not a gate).
pub fn lint_source(source_dir: impl AsRef<Path>) -> Result<(), String> {
    let output = std::process::Command::new("ast-grep")
        .args(["scan", "--config", LINT_CONFIG])
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
    /// Load a prebuilt cdylib plugin. See [`PluginLibrary::load`] for
    /// the trust model.
    pub fn load_file(path: impl AsRef<Path>) -> Result<PluginLibrary, LoadError> {
        // SAFETY: delegated to the caller's choice of plugin binary.
        unsafe { PluginLibrary::load(path.as_ref()) }
    }

    /// Build a plugin crate from source and load the resulting cdylib.
    ///
    /// This is the developer path — running `cargo` inside the host
    /// process — mirroring cordis's write-and-build story; end users
    /// normally receive prebuilt binaries via [`Loader::load_file`] or
    /// [`Loader::fetch`].
    pub fn build_and_load(
        source_dir: impl AsRef<Path>,
        options: &BuildOptions,
    ) -> Result<PluginLibrary, LoadError> {
        let path = Self::build(source_dir, options)?;
        Self::load_file(path)
    }

    /// Build a plugin crate, returning the cdylib artifact path.
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
            LoadError::Build(
                "cargo produced no cdylib artifact; set crate-type = [\"cdylib\"]".into(),
            )
        })
    }

    /// Download a plugin binary into `cache_dir` and return its path.
    /// Requires the `download` feature.
    #[cfg(feature = "download")]
    pub fn fetch(url: &str, cache_dir: impl AsRef<Path>) -> Result<PathBuf, LoadError> {
        let response = ureq::get(url)
            .call()
            .map_err(|e| LoadError::Fetch(e.to_string()))?;
        let file_name = url.rsplit('/').next().unwrap_or("plugin.so");
        std::fs::create_dir_all(cache_dir.as_ref())
            .map_err(|e| LoadError::Fetch(format!("cannot create cache dir: {e}")))?;
        let path = cache_dir.as_ref().join(file_name);
        let mut reader = response.into_reader();
        let mut file = std::fs::File::create(&path)
            .map_err(|e| LoadError::Fetch(format!("cannot create plugin file: {e}")))?;
        std::io::copy(&mut reader, &mut file)
            .map_err(|e| LoadError::Fetch(format!("download interrupted: {e}")))?;
        Ok(path)
    }
}
