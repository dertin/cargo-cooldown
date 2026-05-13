//! `Cargo.lock` snapshotting, restoration, and baseline indexing.
//!
//! Cooldown rewrites lockfiles speculatively, so every attempt starts from a
//! snapshot that can be restored exactly. The same snapshot also records which
//! registry versions were already present, allowing the default baseline to avoid
//! accidental downgrades of existing locked dependencies.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;

use crate::registry::{RegistryStore, is_registry_source};

/// Captured lockfile contents plus the registry package baseline derived from them.
///
/// A snapshot is intentionally both bytes and meaning: the original file text is
/// needed to restore the workspace exactly, while the parsed baseline is used by
/// cooldown baseline to understand which registry versions were already present.
#[derive(Debug, Clone)]
pub struct LockfileSnapshot {
    baseline: LockfileBaseline,
    contents: Option<String>,
}

impl LockfileSnapshot {
    /// Read the current `Cargo.lock` and build the baseline index.
    ///
    /// Missing lockfiles are represented as an empty snapshot, not as an error.
    /// Existing lockfiles are parsed only enough to index registry packages by
    /// crate, effective registry URL, and version. The returned snapshot can be
    /// queried during resolution and later restored byte-for-byte.
    pub fn capture(path: &Path, registry_store: &mut RegistryStore) -> Result<Self> {
        let contents = match fs::read_to_string(path) {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read lockfile {}", path.display()));
            }
        };
        let baseline = LockfileBaseline::from_contents(contents.as_deref(), registry_store)?;
        Ok(Self { baseline, contents })
    }

    pub fn baseline(&self) -> &LockfileBaseline {
        &self.baseline
    }

    /// Restore the exact captured file state, including a missing lockfile.
    ///
    /// If the snapshot came from an existing file, that content is written back.
    /// If the snapshot came from a missing lockfile, any generated lockfile is
    /// removed. Callers use this after failed resolver attempts so users never
    /// keep a partial cooldown rewrite.
    pub fn restore(&self, path: &Path) -> Result<()> {
        match &self.contents {
            Some(contents) => fs::write(path, contents)
                .with_context(|| format!("failed to restore lockfile {}", path.display())),
            None if path.exists() => fs::remove_file(path)
                .with_context(|| format!("failed to remove generated lockfile {}", path.display())),
            None => Ok(()),
        }
    }
}

/// Registry-package index used to protect or compare the initial lockfile state.
///
/// The index answers two baseline questions quickly: whether an exact
/// name/registry/version was already locked, and which already locked version is
/// the newest floor below a current candidate.
#[derive(Debug, Clone, Default)]
pub struct LockfileBaseline {
    packages: HashSet<LockfilePackageKey>,
    versions_by_package: BTreeMap<String, BTreeMap<String, Vec<Version>>>,
}

impl LockfileBaseline {
    fn from_contents(contents: Option<&str>, registry_store: &mut RegistryStore) -> Result<Self> {
        let Some(contents) = contents else {
            return Ok(Self::default());
        };
        let lockfile: RawLockfile =
            toml::from_str(contents).context("failed to parse lockfile baseline")?;
        let mut packages = HashSet::new();
        let mut versions_by_package = BTreeMap::new();

        for package in lockfile.package {
            let Some(source_id) = package.source else {
                continue;
            };
            if !is_registry_source(&source_id) {
                continue;
            }

            let registry = registry_store
                .context_for_source(&source_id)?
                .effective_index_url
                .clone();
            let name = package.name;
            let version = package.version;
            // Keep a sorted parsed-version index so the default baseline can find the
            // pre-run floor without reparsing the whole lockfile on each lookup.
            if let Ok(parsed_version) = Version::parse(&version) {
                versions_by_package
                    .entry(name.clone())
                    .or_insert_with(BTreeMap::new)
                    .entry(registry.clone())
                    .or_insert_with(Vec::new)
                    .push(parsed_version);
            }
            packages.insert(LockfilePackageKey {
                name,
                registry,
                version,
            });
        }

        for registries in versions_by_package.values_mut() {
            for versions in registries.values_mut() {
                versions.sort();
                versions.dedup();
            }
        }

        Ok(Self {
            packages,
            versions_by_package,
        })
    }

    /// Return whether this exact registry package existed in the captured lockfile.
    pub fn contains_registry_version(&self, name: &str, registry: &str, version: &str) -> bool {
        self.packages
            .contains(&LockfilePackageKey::new(name, registry, version))
    }

    /// Find the newest captured version that is not greater than the current one.
    ///
    /// This is used as the effective minimum under the default lockfile baseline:
    /// cooldown may block future upgrades, but it should not downgrade versions
    /// the user already had locked before the command started.
    pub fn newest_version_at_or_below(
        &self,
        name: &str,
        registry: &str,
        current_version: &str,
    ) -> Option<Version> {
        let versions = self.versions_by_package.get(name)?.get(registry)?;
        let current = Version::parse(current_version).ok()?;
        // Versions are sorted once at capture time, so the floor lookup stays O(log n).
        let index = versions.partition_point(|version| version <= &current);
        index.checked_sub(1).map(|index| versions[index].clone())
    }

    /// Return a human-oriented inventory grouped by crate name and registry URL.
    ///
    /// Final summaries compare inventories from the initial and final snapshots
    /// to explain which versions were added, removed, downgraded, or preserved.
    pub fn version_inventory(&self) -> BTreeMap<(String, String), Vec<String>> {
        let mut inventory = BTreeMap::new();

        for package in &self.packages {
            inventory
                .entry((package.name.clone(), package.registry.clone()))
                .or_insert_with(Vec::new)
                .push(package.version.clone());
        }

        inventory
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct LockfilePackageKey {
    name: String,
    registry: String,
    version: String,
}

impl LockfilePackageKey {
    fn new(name: &str, registry: &str, version: &str) -> Self {
        Self {
            name: name.to_string(),
            registry: registry.to_string(),
            version: version.to_string(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawLockfile {
    #[serde(default)]
    package: Vec<RawLockfilePackage>,
}

#[derive(Debug, Deserialize)]
struct RawLockfilePackage {
    name: String,
    version: String,
    source: Option<String>,
}

/// Unit tests for lockfile baseline capture and restoration.
#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::allow_rules::AllowRules;
    use crate::config::{
        Config, FallbackAccept, IncompatiblePublishAgePolicy, LockfileBaselineMode,
    };

    fn config_fixture() -> Config {
        Config {
            min_publish_age_seconds: 60,
            registry_min_publish_age: Default::default(),
            incompatible_publish_age: IncompatiblePublishAgePolicy::Deny,
            fallback_accept: FallbackAccept::Prompt,
            lockfile_baseline: LockfileBaselineMode::Floor,
            now_override: None,
            ttl_seconds: 60,
            cache_dir: None,
            http_retries: 0,
            verbose: false,
            skip_registries: Vec::new(),
            allow_rules: AllowRules::default(),
        }
    }

    #[test]
    fn capture_returns_empty_when_lockfile_is_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();

        let snapshot = LockfileSnapshot::capture(&path, &mut registry_store).unwrap();

        assert!(!snapshot.baseline().contains_registry_version(
            "demo",
            "https://example.com/index",
            "1.0.0"
        ));
    }

    #[test]
    fn capture_tracks_registry_packages_only() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        fs::write(
            &path,
            r#"version = 4

[[package]]
name = "demo"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "workspace-member"
version = "0.1.0"
"#,
        )
        .unwrap();
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();
        let registry = registry_store
            .context_for_source("registry+https://github.com/rust-lang/crates.io-index")
            .unwrap()
            .effective_index_url
            .clone();

        let snapshot = LockfileSnapshot::capture(&path, &mut registry_store).unwrap();

        assert!(
            snapshot
                .baseline()
                .contains_registry_version("demo", &registry, "1.2.3")
        );
        assert!(!snapshot.baseline().contains_registry_version(
            "workspace-member",
            &registry,
            "0.1.0"
        ));
    }

    #[test]
    fn snapshot_restore_removes_generated_lockfile() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();
        let snapshot = LockfileSnapshot::capture(&path, &mut registry_store).unwrap();

        fs::write(&path, "version = 4\n").unwrap();
        snapshot.restore(&path).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn snapshot_restore_reinstates_existing_lockfile_contents() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        fs::write(
            &path,
            r#"version = 4

[[package]]
name = "demo"
version = "1.2.3"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#,
        )
        .unwrap();
        let original = fs::read_to_string(&path).unwrap();
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();
        let snapshot = LockfileSnapshot::capture(&path, &mut registry_store).unwrap();

        fs::write(&path, "version = 4\n").unwrap();
        snapshot.restore(&path).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn capture_fails_when_existing_lockfile_cannot_be_read_as_text() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        fs::write(&path, [0xff]).unwrap();
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();

        let err = LockfileSnapshot::capture(&path, &mut registry_store).unwrap_err();

        assert!(
            format!("{err:#}").contains("failed to read lockfile"),
            "{err:#}"
        );
        assert!(path.exists());
    }
}
