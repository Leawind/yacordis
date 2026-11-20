//! Component repository index: version resolution, checksum verification,
//! and installation layered on [`Loader::fetch`](crate::Loader::fetch).
//!
//! A repository is a static index (`index.json`) plus its artifacts:
//!
//! ```json
//! {
//!   "components": {
//!     "greeter": [
//!       { "version": "1.2.0", "artifact": "greeter-1.2.0.cdylib",
//!         "sha256": "…", "abi": 2 }
//!     ]
//!   }
//! }
//! ```
//!
//! `Repository::install` resolves `name + constraint` against the index
//! (best matching version), downloads the artifact into the cache,
//! verifies its sha-256 checksum and ABI version, and loads it. Relative
//! artifact paths are joined with the repository base URL; a path that
//! already exists on disk is used directly, which keeps the layer
//! testable offline.
//!
//! Integrity is content-addressed (sha-256 over the artifact bytes),
//! so a compromised index entry with a mismatched checksum is rejected
//! before any code is mapped. Asymmetric signatures need a crypto
//! dependency and are future work ([`Verifier`] is the plug point).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub(crate) use crate::digest::{Sha256, sha256_hex};
use crate::loader::{ComponentLibrary, LoadError, Loader};

/// Index/repository layer errors.
#[derive(Debug)]
pub enum RegistryError {
    /// The index JSON is malformed.
    Format(String),
    /// The name/constraint pair matched nothing.
    NotFound(String),
    /// A version string or constraint is not valid semver (minimal
    /// `major.minor.patch` subset; prereleases unsupported).
    Version(String),
    /// The artifact checksum does not match the index entry.
    Checksum {
        /// Checksum recorded in the index.
        expected: String,
        /// Checksum computed over the artifact bytes.
        actual: String,
    },
    /// The entry targets a different ABI version.
    Abi {
        /// ABI version recorded in the index.
        expected: u32,
        /// ABI version of the host.
        found: u32,
    },
    /// Download/IO failure.
    Fetch(String),
    /// Loading the verified artifact failed.
    Load(LoadError),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Format(message) => write!(f, "malformed index: {message}"),
            Self::NotFound(message) => write!(f, "no matching component: {message}"),
            Self::Version(message) => write!(f, "bad version: {message}"),
            Self::Checksum { expected, actual } => write!(
                f,
                "checksum mismatch: expected {expected}, computed {actual}"
            ),
            Self::Abi { expected, found } => {
                write!(f, "ABI mismatch: index targets {expected}, host is {found}")
            }
            Self::Fetch(message) => write!(f, "fetch failed: {message}"),
            Self::Load(error) => write!(f, "load failed: {error}"),
        }
    }
}

impl std::error::Error for RegistryError {}

// ---------------------------------------------------------------------
// Minimal semver: major.minor.patch, `^` / `~` / comparison / exact.
// ---------------------------------------------------------------------

/// A plain `major.minor.patch` version (prerelease tags unsupported).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
}

impl Version {
    /// Parse `major.minor.patch`.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        let parts: Vec<&str> = text.split('.').collect();
        if parts.len() != 3 {
            return Err(RegistryError::Version(text.to_owned()));
        }
        let numbers: Result<Vec<u64>, _> = parts
            .iter()
            .map(|p| {
                p.parse::<u64>()
                    .map_err(|_| RegistryError::Version(text.to_owned()))
            })
            .collect();
        let numbers = numbers?;
        Ok(Self {
            major: numbers[0],
            minor: numbers[1],
            patch: numbers[2],
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One semver comparator atom.
#[derive(Clone, Copy, Debug)]
enum Atom {
    /// `^X.Y[.Z]`: same major, at least the stated minor/patch.
    Caret(Version),
    /// `~X.Y[.Z]`: same major.minor, at least the stated patch.
    Tilde(Version),
    /// `>`, `>=`, `<`, `<=`, `=` against an exact version.
    Compare(u8, Version),
}

impl Atom {
    fn parse(text: &str) -> Result<Self, RegistryError> {
        let text = text.trim();
        let (op, rest) = match text {
            t if t.starts_with("^") => ('^', &t[1..]),
            t if t.starts_with("~") => ('~', &t[1..]),
            t if t.starts_with(">=") => ('G', &t[2..]),
            t if t.starts_with("<=") => ('L', &t[2..]),
            t if t.starts_with('>') => ('>', &t[1..]),
            t if t.starts_with('<') => ('<', &t[1..]),
            t if t.starts_with('=') => ('=', &t[1..]),
            _ => ('=', text),
        };
        let version = parse_partial(rest)?;
        Ok(match op {
            '^' => Self::Caret(version),
            '~' => Self::Tilde(version),
            '>' => Self::Compare(b'>', version),
            'G' => Self::Compare(b'G', version),
            '<' => Self::Compare(b'<', version),
            'L' => Self::Compare(b'L', version),
            _ => Self::Compare(b'=', version),
        })
    }

    fn matches(&self, candidate: Version) -> bool {
        match *self {
            Self::Caret(base) => candidate.major == base.major && candidate >= floor(base),
            Self::Tilde(base) => {
                candidate.major == base.major
                    && candidate.minor == base.minor
                    && candidate >= floor(base)
            }
            Self::Compare(op, base) => match op {
                b'>' => candidate > base,
                b'G' => candidate >= base,
                b'<' => candidate < base,
                b'L' => candidate <= base,
                _ => candidate == base,
            },
        }
    }
}

/// `^1.2` means ">= 1.2.0"; missing patch/minor default to zero.
fn floor(partial: Version) -> Version {
    partial
}

/// Parse `X.Y` or `X.Y.Z` (missing parts are zero).
fn parse_partial(text: &str) -> Result<Version, RegistryError> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(RegistryError::Version(text.to_owned()));
    }
    let mut numbers = [0u64; 3];
    for (index, part) in parts.iter().enumerate() {
        numbers[index] = part
            .parse::<u64>()
            .map_err(|_| RegistryError::Version(text.to_owned()))?;
    }
    Ok(Version {
        major: numbers[0],
        minor: numbers[1],
        patch: numbers[2],
    })
}

/// A version constraint: comma-separated atoms, all must match.
#[derive(Clone, Debug)]
pub struct Constraint {
    atoms: Vec<Atom>,
}

impl Constraint {
    /// Parse a constraint like `^1.2`, `~1.2.3`, `>=1.0, <2.0`, `1.2.3`.
    pub fn parse(text: &str) -> Result<Self, RegistryError> {
        let trimmed = text.trim();
        if trimmed == "*" || trimmed.is_empty() {
            return Ok(Self { atoms: Vec::new() });
        }
        let atoms: Result<Vec<Atom>, _> = trimmed.split(',').map(Atom::parse).collect();
        Ok(Self { atoms: atoms? })
    }

    /// Whether `version` satisfies every atom.
    pub fn matches(&self, version: Version) -> bool {
        self.atoms.iter().all(|atom| atom.matches(version))
    }
}

// ---------------------------------------------------------------------
// Index model
// ---------------------------------------------------------------------

/// One component version in the index.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The component version.
    pub version: Version,
    /// Artifact file name or absolute URL; a path that exists locally is
    /// loaded directly.
    pub artifact: String,
    /// sha-256 of the artifact bytes, lowercase hex.
    pub sha256: String,
    /// ABI version the artifact was built against.
    pub abi: u32,
}

/// Parsed `index.json`.
#[derive(Clone, Debug, Default)]
pub struct RegistryIndex {
    components: HashMap<String, Vec<Entry>>,
}

impl RegistryIndex {
    /// Parse an index document.
    pub fn from_json(document: &Value) -> Result<Self, RegistryError> {
        let components = document
            .get("components")
            .and_then(Value::as_object)
            .ok_or_else(|| RegistryError::Format("missing `components` object".into()))?;
        let mut index = HashMap::new();
        for (name, versions) in components {
            let list = versions
                .as_array()
                .ok_or_else(|| RegistryError::Format(format!("`{name}` is not an array")))?;
            let mut entries = Vec::new();
            for entry in list {
                let version_text =
                    entry
                        .get("version")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            RegistryError::Format(format!("`{name}` entry missing version"))
                        })?;
                let artifact = entry
                    .get("artifact")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        RegistryError::Format(format!("`{name}` entry missing artifact"))
                    })?
                    .to_owned();
                let sha256 = entry
                    .get("sha256")
                    .and_then(Value::as_str)
                    .ok_or_else(|| RegistryError::Format(format!("`{name}` entry missing sha256")))?
                    .to_lowercase();
                let abi = entry.get("abi").and_then(Value::as_u64).unwrap_or(1) as u32;
                entries.push(Entry {
                    version: Version::parse(version_text)?,
                    artifact,
                    sha256,
                    abi,
                });
            }
            entries.sort_by_key(|entry| entry.version);
            index.insert(name.clone(), entries);
        }
        Ok(Self { components: index })
    }

    /// Best version matching `constraint` (highest wins).
    pub fn resolve(&self, name: &str, constraint: &Constraint) -> Result<&Entry, RegistryError> {
        let entries = self
            .components
            .get(name)
            .ok_or_else(|| RegistryError::NotFound(format!("`{name}` not in index")))?;
        entries
            .iter()
            .filter(|entry| constraint.matches(entry.version))
            .max_by_key(|entry| entry.version)
            .ok_or_else(|| {
                RegistryError::NotFound(format!("`{name}` has no version matching the constraint"))
            })
    }
}

// ---------------------------------------------------------------------
// Integrity
// ---------------------------------------------------------------------

/// Artifact integrity plug point. The built-in sha-256 verifier rejects
/// index entries whose recorded checksum does not match the bytes on
/// disk; asymmetric signatures can replace it later.
pub trait Verifier {
    /// Verify `bytes` against an index entry.
    fn verify(&self, entry: &Entry, bytes: &[u8]) -> Result<(), RegistryError>;
}

/// The default verifier: sha-256 hex equality plus ABI gate.
#[derive(Default, Clone, Copy)]
pub struct Sha256Verifier;

impl Verifier for Sha256Verifier {
    fn verify(&self, entry: &Entry, bytes: &[u8]) -> Result<(), RegistryError> {
        let actual = sha256_hex(bytes);
        if actual != entry.sha256 {
            return Err(RegistryError::Checksum {
                expected: entry.sha256.clone(),
                actual,
            });
        }
        let host_abi = yacordis_component_api::ABI_VERSION;
        if entry.abi != host_abi {
            return Err(RegistryError::Abi {
                expected: entry.abi,
                found: host_abi,
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Repository client
// ---------------------------------------------------------------------

/// A repository client: base URL + local artifact cache.
pub struct Repository {
    /// Only used by the download path (kept for non-download builds).
    #[cfg_attr(not(feature = "download"), allow(dead_code))]
    base_url: String,
    cache_dir: PathBuf,
}

impl Repository {
    /// A repository rooted at `base_url` (e.g. `https://components.example/v1`).
    pub fn new(base_url: impl Into<String>, cache_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            cache_dir: cache_dir.into(),
        }
    }

    /// Fetch and parse `index.json` (requires the `download` feature).
    #[cfg(feature = "download")]
    pub fn fetch_index(&self) -> Result<RegistryIndex, RegistryError> {
        let path = Loader::fetch(&format!("{}/index.json", self.base_url), &self.cache_dir)
            .map_err(|e| RegistryError::Fetch(e.to_string()))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| RegistryError::Fetch(format!("cannot read index: {e}")))?;
        let document: Value = serde_json::from_str(&text)
            .map_err(|e| RegistryError::Format(format!("index is not JSON: {e}")))?;
        RegistryIndex::from_json(&document)
    }

    /// Resolve `name + constraint` against `index`, obtain the artifact
    /// (from the local cache / filesystem when the path exists, otherwise
    /// downloaded — requiring the `download` feature), verify integrity
    /// and ABI, and load it.
    pub fn install<V: Verifier>(
        &self,
        index: &RegistryIndex,
        name: &str,
        constraint: &Constraint,
        verifier: &V,
    ) -> Result<ComponentLibrary, RegistryError> {
        let entry = index.resolve(name, constraint)?;
        let bytes = self.artifact_bytes(entry)?;
        verifier.verify(entry, &bytes)?;
        // Cache by content hash: identical artifacts share one file.
        let cache_path = self.cache_dir.join(format!("{}.cdylib", entry.sha256));
        if !cache_path.exists() {
            std::fs::create_dir_all(&self.cache_dir)
                .map_err(|e| RegistryError::Fetch(format!("cannot create cache dir: {e}")))?;
            std::fs::write(&cache_path, &bytes)
                .map_err(|e| RegistryError::Fetch(format!("cannot cache artifact: {e}")))?;
        }
        Loader::load_file(&cache_path).map_err(RegistryError::Load)
    }

    fn artifact_bytes(&self, entry: &Entry) -> Result<Vec<u8>, RegistryError> {
        let local = Path::new(&entry.artifact);
        if local.exists() {
            return std::fs::read(local)
                .map_err(|e| RegistryError::Fetch(format!("cannot read artifact: {e}")));
        }
        #[cfg(feature = "download")]
        {
            let url = if entry.artifact.starts_with("http://")
                || entry.artifact.starts_with("https://")
            {
                entry.artifact.clone()
            } else {
                format!(
                    "{}/{}",
                    self.base_url,
                    entry.artifact.trim_start_matches('/')
                )
            };
            let path = Loader::fetch(&url, &self.cache_dir)
                .map_err(|e| RegistryError::Fetch(e.to_string()))?;
            std::fs::read(&path)
                .map_err(|e| RegistryError::Fetch(format!("cannot read artifact: {e}")))
        }
        #[cfg(not(feature = "download"))]
        {
            Err(RegistryError::Fetch(format!(
                "artifact `{}` is not available locally and the download feature is off",
                entry.artifact
            )))
        }
    }
}
