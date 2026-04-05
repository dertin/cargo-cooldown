use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
pub struct Allowlist {
    #[serde(default)]
    pub allow: AllowSection,
}

#[derive(Debug, Default, Clone, Deserialize, PartialEq, Eq)]
pub struct AllowSection {
    #[serde(default)]
    pub exact: Vec<AllowExact>,
    #[serde(default)]
    pub package: Vec<AllowPackage>,
    pub global: Option<AllowGlobal>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AllowExact {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AllowPackage {
    #[serde(rename = "crate")]
    pub crate_name: String,
    #[serde(default)]
    pub minimum_release_age: Option<u64>,
    #[serde(default)]
    pub minutes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AllowGlobal {
    #[serde(default)]
    pub minimum_release_age: Option<u64>,
    #[serde(default)]
    pub minutes: Option<u64>,
}

impl Allowlist {
    #[cfg(test)]
    pub fn merged(base: &Self, overlay: &Self) -> Self {
        let mut merged = base.clone();
        merged.merge_from(overlay);
        merged
    }

    pub fn merge_from(&mut self, overlay: &Self) {
        self.allow.merge_from(&overlay.allow);
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
            .and_then(AllowGlobal::effective_minutes)
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

impl AllowSection {
    pub fn merge_from(&mut self, overlay: &Self) {
        if let Some(global) = &overlay.global {
            self.global = Some(global.clone());
        }

        for package in &overlay.package {
            if let Some(existing) = self
                .package
                .iter_mut()
                .find(|existing| existing.crate_name == package.crate_name)
            {
                *existing = package.clone();
            } else {
                self.package.push(package.clone());
            }
        }

        for exact in &overlay.exact {
            if self.exact.iter().any(|existing| {
                existing.crate_name == exact.crate_name && existing.version == exact.version
            }) {
                continue;
            }
            self.exact.push(exact.clone());
        }
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

    #[test]
    fn merged_allowlist_deduplicates_exact_and_overrides_package_minutes() {
        let base = Allowlist {
            allow: AllowSection {
                exact: vec![AllowExact {
                    crate_name: "foo".to_string(),
                    version: "1.2.3".to_string(),
                }],
                package: vec![AllowPackage {
                    crate_name: "bar".to_string(),
                    minimum_release_age: Some(10),
                    minutes: None,
                }],
                global: Some(AllowGlobal {
                    minimum_release_age: Some(60),
                    minutes: None,
                }),
            },
        };
        let overlay = Allowlist {
            allow: AllowSection {
                exact: vec![
                    AllowExact {
                        crate_name: "foo".to_string(),
                        version: "1.2.3".to_string(),
                    },
                    AllowExact {
                        crate_name: "baz".to_string(),
                        version: "9.9.9".to_string(),
                    },
                ],
                package: vec![AllowPackage {
                    crate_name: "bar".to_string(),
                    minimum_release_age: Some(5),
                    minutes: None,
                }],
                global: Some(AllowGlobal {
                    minimum_release_age: Some(30),
                    minutes: None,
                }),
            },
        };

        let merged = Allowlist::merged(&base, &overlay);

        assert!(merged.is_exact_allowed("foo", "1.2.3"));
        assert!(merged.is_exact_allowed("baz", "9.9.9"));
        assert_eq!(merged.allow.exact.len(), 2);
        assert_eq!(merged.effective_minutes_for("bar", 90), 5);
        assert_eq!(merged.global_minutes(), Some(30));
    }

    #[test]
    fn package_rule_can_disable_cooldown_for_one_crate() {
        let allowlist = Allowlist {
            allow: AllowSection {
                exact: Vec::new(),
                package: vec![AllowPackage {
                    crate_name: "tokio".to_string(),
                    minimum_release_age: None,
                    minutes: Some(0),
                }],
                global: Some(AllowGlobal {
                    minimum_release_age: Some(1440),
                    minutes: None,
                }),
            },
        };

        assert_eq!(allowlist.effective_minutes_for("tokio", 1440), 0);
        assert_eq!(allowlist.effective_minutes_for("serde", 1440), 1440);
    }
}
