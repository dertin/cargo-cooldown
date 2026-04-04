use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::registry::{RegistryStore, is_registry_source};

#[derive(Debug, Clone, Default)]
pub struct LockfileBaseline {
    packages: HashSet<LockfilePackageKey>,
}

impl LockfileBaseline {
    pub fn capture(path: &Path, registry_store: &mut RegistryStore) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read lockfile baseline {}", path.display()))?;
        let lockfile: RawLockfile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse lockfile baseline {}", path.display()))?;
        let mut packages = HashSet::new();

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
            packages.insert(LockfilePackageKey {
                name: package.name,
                registry,
                version: package.version,
            });
        }

        Ok(Self { packages })
    }

    pub fn contains_registry_version(&self, name: &str, registry: &str, version: &str) -> bool {
        self.packages
            .contains(&LockfilePackageKey::new(name, registry, version))
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

    use crate::config::{Config, LockfilePolicy, Mode};

    fn config_fixture() -> Config {
        Config {
            cooldown_minutes: 60,
            mode: Mode::Enforce,
            lockfile_policy: LockfilePolicy::Changed,
            now_override: None,
            ttl_seconds: 60,
            allowlist_path: None,
            cache_dir: None,
            http_retries: 0,
            verbose: false,
            skip_registries: Vec::new(),
        }
    }

    #[test]
    fn capture_returns_empty_when_lockfile_is_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Cargo.lock");
        let mut registry_store = RegistryStore::new(&config_fixture()).unwrap();

        let baseline = LockfileBaseline::capture(&path, &mut registry_store).unwrap();

        assert!(!baseline.contains_registry_version("demo", "https://example.com/index", "1.0.0"));
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

        let baseline = LockfileBaseline::capture(&path, &mut registry_store).unwrap();

        assert!(baseline.contains_registry_version("demo", &registry, "1.2.3"));
        assert!(!baseline.contains_registry_version("workspace-member", &registry, "0.1.0"));
    }
}
