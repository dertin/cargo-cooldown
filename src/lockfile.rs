use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use semver::Version;
use serde::Deserialize;

use crate::registry::{RegistryStore, is_registry_source};

#[derive(Debug, Clone)]
pub struct LockfileSnapshot {
    baseline: LockfileBaseline,
    contents: Option<String>,
}

impl LockfileSnapshot {
    pub fn capture(path: &Path, registry_store: &mut RegistryStore) -> Result<Self> {
        let contents = fs::read_to_string(path).ok();
        let baseline = LockfileBaseline::from_contents(contents.as_deref(), registry_store)?;
        Ok(Self { baseline, contents })
    }

    pub fn baseline(&self) -> &LockfileBaseline {
        &self.baseline
    }

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

    pub fn contains_registry_version(&self, name: &str, registry: &str, version: &str) -> bool {
        self.packages
            .contains(&LockfilePackageKey::new(name, registry, version))
    }

    pub fn newest_version_at_or_below(
        &self,
        name: &str,
        registry: &str,
        current_version: &str,
    ) -> Option<Version> {
        let versions = self.versions_by_package.get(name)?.get(registry)?;
        let current = Version::parse(current_version).ok()?;
        let index = versions.partition_point(|version| version <= &current);
        index.checked_sub(1).map(|index| versions[index].clone())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::allow_rules::AllowRules;
    use crate::config::{Config, LockfilePolicy, Mode};

    fn config_fixture() -> Config {
        Config {
            cooldown_minutes: 60,
            mode: Mode::Strict,
            lockfile_policy: LockfilePolicy::Changed,
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
}
