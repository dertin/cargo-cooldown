use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};

#[derive(Debug, Default, Deserialize)]
pub struct Allowlist {
    #[serde(default)]
    pub allow: AllowSection,
}

#[derive(Debug, Default, Deserialize)]
pub struct AllowSection {
    #[serde(default)]
    pub exact: Vec<AllowExact>,
    #[serde(default)]
    pub package: Vec<AllowPackage>,
    pub global: Option<AllowGlobal>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AllowExact {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub version: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AllowPackage {
    #[serde(rename = "crate")]
    pub crate_name: String,
    #[serde(default)]
    pub minimum_release_age: Option<u64>,
    #[serde(default)]
    pub minutes: Option<u64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AllowGlobal {
    #[serde(default)]
    pub minimum_release_age: Option<u64>,
    #[serde(default)]
    pub minutes: Option<u64>,
}

impl Allowlist {
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(p) => p,
            None => PathBuf::from("cooldown-allowlist.toml"),
        };

        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read allowlist at {}", path.display()))?;
        let allowlist: Allowlist = toml::from_str(&contents)
            .with_context(|| format!("failed to parse allowlist at {}", path.display()))?;
        Ok(allowlist)
    }

    pub fn is_exact_allowed(&self, name: &str, version: &str) -> bool {
        self.allow
            .exact
            .iter()
            .any(|entry| entry.crate_name == name && entry.version == version)
    }

    pub fn per_crate_minutes(&self) -> HashMap<String, u64> {
        self.allow
            .package
            .iter()
            .filter_map(|pkg| pkg.effective_minutes().map(|m| (pkg.crate_name.clone(), m)))
            .collect()
    }

    pub fn global_minutes(&self) -> Option<u64> {
        self.allow
            .global
            .as_ref()
            .and_then(|g| g.effective_minutes())
    }
    #[cfg(test)]
    pub fn effective_minutes_for(&self, name: &str, default_minutes: u64) -> u64 {
        let mut effective = default_minutes;
        if let Some(global) = self.global_minutes() {
            effective = effective.min(global);
        }
        if let Some(rule) = self.allow.package.iter().find(|pkg| pkg.crate_name == name)
            && let Some(minutes) = rule.effective_minutes()
        {
            effective = effective.min(minutes);
        }
        effective
    }
}

impl AllowPackage {
    pub fn effective_minutes(&self) -> Option<u64> {
        self.minimum_release_age.or(self.minutes)
    }
}

impl AllowGlobal {
    pub fn effective_minutes(&self) -> Option<u64> {
        self.minimum_release_age.or(self.minutes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn loads_allowlist_and_respects_exact() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "[[allow.exact]]\ncrate = \"foo\"\nversion = \"1.2.3\"\n[[allow.package]]\ncrate = \"bar\"\nminimum_release_age = 3\n[allow.global]\nminutes = 5\n"
        )
        .unwrap();

        let allowlist = Allowlist::load(Some(file.path().to_path_buf())).unwrap();
        assert!(allowlist.is_exact_allowed("foo", "1.2.3"));
        assert!(!allowlist.is_exact_allowed("foo", "1.2.4"));

        let per_crate = allowlist.per_crate_minutes();
        assert_eq!(per_crate.get("bar"), Some(&3));
        assert_eq!(allowlist.global_minutes(), Some(5));
        assert_eq!(allowlist.effective_minutes_for("bar", 7), 3);
        assert_eq!(allowlist.effective_minutes_for("baz", 7), 5);
    }
}
