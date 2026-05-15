//! Configuration loading, validation, and merge precedence.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use dirs::home_dir;
use serde::Deserialize;

use crate::allow_rules::{AllowRules, AllowSection};
use crate::project::ProjectContext;
use crate::registry::{
    RegistryContext, RegistryOverrideMatchPriority, registry_override_match_priority,
    validate_registry_override_index,
};

const SECONDS_PER_MINUTE: u64 = 60;
const SECONDS_PER_HOUR: u64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: u64 = 24 * SECONDS_PER_HOUR;
const SECONDS_PER_WEEK: u64 = 7 * SECONDS_PER_DAY;
const SECONDS_PER_MONTH: u64 = 30 * SECONDS_PER_DAY;

/// Behavior when cooldown cannot fully remove fresh versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatiblePublishAgePolicy {
    Deny,
    Fallback,
    Allow,
}

impl IncompatiblePublishAgePolicy {
    pub fn parse_compat_enforcement(value: &str) -> Result<Self> {
        match value {
            "strict" => Ok(IncompatiblePublishAgePolicy::Deny),
            "cargo_compatible" => Ok(IncompatiblePublishAgePolicy::Fallback),
            "off" => Ok(IncompatiblePublishAgePolicy::Allow),
            _ => {
                bail!(
                    "invalid root enforcement `{value}`; expected one of: strict, cargo_compatible, off"
                )
            }
        }
    }

    pub fn parse_incompatible_publish_age(value: &str) -> Result<Self> {
        match value {
            "deny" => Ok(IncompatiblePublishAgePolicy::Deny),
            "allow" => Ok(IncompatiblePublishAgePolicy::Allow),
            "fallback" => Ok(IncompatiblePublishAgePolicy::Fallback),
            _ => {
                bail!(
                    "invalid incompatible publish age policy `{value}`; expected one of: deny, fallback, allow"
                )
            }
        }
    }
}

/// Whether fallback unresolved fresh versions require user confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackAccept {
    Prompt,
    Auto,
}

impl FallbackAccept {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "prompt" => Ok(FallbackAccept::Prompt),
            "auto" => Ok(FallbackAccept::Auto),
            _ => {
                bail!("invalid fallback accept policy `{value}`; expected one of: prompt, auto")
            }
        }
    }
}

/// Whether the initial lockfile is used as a minimum version baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockfileBaselineMode {
    Floor,
    Ignore,
}

impl LockfileBaselineMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "floor" => Ok(LockfileBaselineMode::Floor),
            "ignore" => Ok(LockfileBaselineMode::Ignore),
            _ => bail!("invalid lockfile baseline `{value}`; expected one of: floor, ignore"),
        }
    }

    pub fn uses_initial_lockfile_floor(self) -> bool {
        matches!(self, LockfileBaselineMode::Floor)
    }
}

/// Effective runtime configuration after files and environment are merged.
#[derive(Debug, Clone)]
pub struct Config {
    pub min_publish_age_seconds: u64,
    pub registry_min_publish_age: RegistryMinPublishAgeConfig,
    pub incompatible_publish_age: IncompatiblePublishAgePolicy,
    pub fallback_accept: FallbackAccept,
    pub lockfile_baseline: LockfileBaselineMode,
    pub now_override: Option<DateTime<Utc>>,
    pub ttl_seconds: u64,
    pub cache_dir: Option<PathBuf>,
    pub http_retries: u32,
    pub verbose: bool,
    pub skip_registries: Vec<String>,
    pub allow_rules: AllowRules,
}

impl Config {
    /// Load the effective configuration for one resolved Cargo project.
    ///
    /// Configuration is layered from `$CARGO_HOME/cooldown.toml`, workspace
    /// `cooldown.toml`, an active member override when the command targets one
    /// member, and finally environment variables. The returned value is already
    /// validated and contains defaults for any omitted settings.
    pub fn load(project: &ProjectContext) -> Result<Self> {
        let mut merged = MergedConfig::default();

        if let Some(path) = user_config_path() {
            merged.apply_file(&path)?;
        }
        merged.apply_file(&project.workspace_config_path())?;
        if let Some(path) = project.member_config_path() {
            merged.apply_file(&path)?;
        }
        merged.apply_env()?;
        Ok(merged.finish())
    }

    pub fn min_publish_age_seconds_for(
        &self,
        context: &RegistryContext,
        crate_name: &str,
    ) -> Result<u64> {
        let mut seconds = self
            .registry_min_publish_age
            .for_context(context)?
            .unwrap_or(self.min_publish_age_seconds);

        if let Some(global_minutes) = self.allow_rules.global_minutes() {
            seconds = seconds.min(minutes_to_seconds_saturating(global_minutes));
        }
        if let Some(&crate_seconds) = self
            .allow_rules
            .per_crate_min_publish_age_seconds()
            .get(crate_name)
        {
            seconds = seconds.min(crate_seconds);
        }

        Ok(seconds)
    }

    pub fn has_positive_min_publish_age(&self) -> bool {
        self.min_publish_age_seconds > 0 || self.registry_min_publish_age.has_positive_override()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegistryMinPublishAgeConfig {
    pub crates_io_seconds: Option<u64>,
    pub registries: Vec<RegistryMinPublishAgeOverride>,
}

impl RegistryMinPublishAgeConfig {
    fn has_positive_override(&self) -> bool {
        self.crates_io_seconds.is_some_and(|seconds| seconds > 0)
            || self.registries.iter().any(|registry| {
                registry
                    .min_publish_age_seconds
                    .is_some_and(|seconds| seconds > 0)
            })
    }

    fn for_context(&self, context: &RegistryContext) -> Result<Option<u64>> {
        if context.logical_name == "crates-io" {
            return Ok(self.crates_io_seconds);
        }

        let mut best: Option<(RegistryOverrideMatchPriority, usize, u64)> = None;
        for (index, entry) in self.registries.iter().enumerate() {
            let Some(seconds) = entry.min_publish_age_seconds else {
                continue;
            };
            let Some(priority) =
                registry_override_match_priority(entry, context).with_context(|| {
                    format!(
                        "failed to evaluate min-publish-age override for [registries.{}]",
                        entry.name
                    )
                })?
            else {
                continue;
            };
            let candidate = (priority, index, seconds);
            if best
                .as_ref()
                .is_none_or(|current| (candidate.0, candidate.1) > (current.0, current.1))
            {
                best = Some(candidate);
            }
        }

        Ok(best.map(|(_, _, seconds)| seconds))
    }

    fn merge_from(&mut self, overlay: RegistryMinPublishAgeConfig) {
        if overlay.crates_io_seconds.is_some() {
            self.crates_io_seconds = overlay.crates_io_seconds;
        }

        for registry in overlay.registries {
            if let Some(existing) = self
                .registries
                .iter_mut()
                .find(|existing| existing.name == registry.name)
            {
                let mut merged = registry;
                if merged.index.is_none() {
                    merged.index = existing.index.clone();
                }
                if merged.min_publish_age_seconds.is_none() {
                    merged.min_publish_age_seconds = existing.min_publish_age_seconds;
                }
                *existing = merged;
            } else {
                self.registries.push(registry);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryMinPublishAgeOverride {
    pub name: String,
    pub index: Option<String>,
    pub min_publish_age_seconds: Option<u64>,
    pub name_from_env: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CooldownFile {
    #[serde(rename = "cooldown_minutes")]
    compat_cooldown_minutes: Option<u64>,
    #[serde(rename = "enforcement")]
    compat_enforcement: Option<String>,
    #[serde(rename = "cargo_compatible_accept")]
    compat_fallback_accept: Option<String>,
    #[serde(rename = "lockfile_baseline")]
    compat_lockfile_baseline: Option<String>,
    now: Option<String>,
    ttl_seconds: Option<u64>,
    cache_dir: Option<PathBuf>,
    http_retries: Option<u32>,
    verbose: Option<bool>,
    skip_registries: Option<Vec<String>>,
    cooldown: Option<CooldownFileSection>,
    registry: Option<RootRegistryFileSection>,
    #[serde(default)]
    registries: HashMap<String, NamedRegistryFileSection>,
    #[serde(default)]
    allow: AllowSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CooldownFileSection {
    #[serde(rename = "incompatible-publish-age")]
    incompatible_publish_age: Option<String>,
    #[serde(rename = "fallback-accept")]
    fallback_accept: Option<String>,
    #[serde(rename = "lockfile-baseline")]
    lockfile_baseline: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootRegistryFileSection {
    #[serde(rename = "global-min-publish-age")]
    global_min_publish_age: Option<String>,
    #[serde(rename = "min-publish-age")]
    min_publish_age: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedRegistryFileSection {
    #[serde(rename = "min-publish-age")]
    min_publish_age: Option<String>,
    index: Option<String>,
}

#[derive(Debug, Default)]
struct MergedConfig {
    min_publish_age_seconds: Option<u64>,
    registry_min_publish_age: RegistryMinPublishAgeConfig,
    incompatible_publish_age: Option<IncompatiblePublishAgePolicy>,
    fallback_accept: Option<FallbackAccept>,
    lockfile_baseline: Option<LockfileBaselineMode>,
    now_override: Option<DateTime<Utc>>,
    ttl_seconds: Option<u64>,
    cache_dir: Option<PathBuf>,
    http_retries: Option<u32>,
    verbose: Option<bool>,
    skip_registries: Vec<String>,
    allow_rules: AllowRules,
}

impl MergedConfig {
    fn apply_file(&mut self, path: &Path) -> Result<()> {
        let Some(file) = read_file_config(path)? else {
            return Ok(());
        };

        let compat_global_seconds = file
            .data
            .compat_cooldown_minutes
            .map(minutes_to_seconds)
            .transpose()?;
        let registry_global_seconds = file
            .data
            .registry
            .as_ref()
            .and_then(|registry| registry.global_min_publish_age.as_deref())
            .map(parse_duration_seconds)
            .transpose()?;
        if compat_global_seconds
            .zip(registry_global_seconds)
            .is_some_and(|(compat, registry)| compat != registry)
        {
            bail!(
                "{} defines incompatible `cooldown_minutes` and `[registry].global-min-publish-age` values; use only one global min-publish-age setting",
                file.path.display()
            );
        }

        if let Some(seconds) = compat_global_seconds {
            self.min_publish_age_seconds = Some(seconds);
        }
        if let Some(seconds) = registry_global_seconds {
            self.min_publish_age_seconds = Some(seconds);
        }
        let file_registry_config = file.registry_min_publish_age_config()?;
        self.registry_min_publish_age
            .merge_from(file_registry_config);
        let root_compat_policy = file
            .data
            .compat_enforcement
            .as_deref()
            .map(IncompatiblePublishAgePolicy::parse_compat_enforcement)
            .transpose()?;
        let cooldown_incompatible_publish_age = file
            .data
            .cooldown
            .as_ref()
            .and_then(|cooldown| cooldown.incompatible_publish_age.as_deref())
            .map(IncompatiblePublishAgePolicy::parse_incompatible_publish_age)
            .transpose()?;
        if root_compat_policy
            .zip(cooldown_incompatible_publish_age)
            .is_some_and(|(root, cooldown)| root != cooldown)
        {
            bail!(
                "{} defines incompatible root `enforcement` and `[cooldown].incompatible-publish-age` values; use only one incompatible publish age policy setting",
                file.path.display()
            );
        }
        if let Some(policy) = root_compat_policy {
            self.incompatible_publish_age = Some(policy);
        }
        if let Some(policy) = cooldown_incompatible_publish_age {
            self.incompatible_publish_age = Some(policy);
        }

        let root_accept = file
            .data
            .compat_fallback_accept
            .as_deref()
            .map(FallbackAccept::parse)
            .transpose()?;
        let cooldown_accept = file
            .data
            .cooldown
            .as_ref()
            .and_then(|cooldown| cooldown.fallback_accept.as_deref())
            .map(FallbackAccept::parse)
            .transpose()?;
        if root_accept
            .zip(cooldown_accept)
            .is_some_and(|(root, cooldown)| root != cooldown)
        {
            bail!(
                "{} defines incompatible root `cargo_compatible_accept` and `[cooldown].fallback-accept` values; use only one fallback accept setting",
                file.path.display()
            );
        }
        if let Some(policy) = root_accept {
            self.fallback_accept = Some(policy);
        }
        if let Some(policy) = cooldown_accept {
            self.fallback_accept = Some(policy);
        }

        let root_baseline = file
            .data
            .compat_lockfile_baseline
            .as_deref()
            .map(LockfileBaselineMode::parse)
            .transpose()?;
        let cooldown_baseline = file
            .data
            .cooldown
            .as_ref()
            .and_then(|cooldown| cooldown.lockfile_baseline.as_deref())
            .map(LockfileBaselineMode::parse)
            .transpose()?;
        if root_baseline
            .zip(cooldown_baseline)
            .is_some_and(|(root, cooldown)| root != cooldown)
        {
            bail!(
                "{} defines incompatible root `lockfile_baseline` and `[cooldown].lockfile-baseline` values; use only one lockfile baseline setting",
                file.path.display()
            );
        }
        if let Some(baseline) = root_baseline {
            self.lockfile_baseline = Some(baseline);
        }
        if let Some(baseline) = cooldown_baseline {
            self.lockfile_baseline = Some(baseline);
        }
        if let Some(now) = file.data.now.as_deref() {
            self.now_override = Some(parse_datetime(now)?);
        }
        if let Some(ttl_seconds) = file.data.ttl_seconds {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = file.data.cache_dir.as_ref() {
            self.cache_dir = Some(file.resolve_path(cache_dir));
        }
        if let Some(http_retries) = file.data.http_retries {
            self.http_retries = Some(validate_http_retries(http_retries)?);
        }
        if let Some(verbose) = file.data.verbose {
            self.verbose = Some(verbose);
        }
        if let Some(skip_registries) = file.data.skip_registries.clone() {
            self.skip_registries = merge_registry_skip_lists(
                &self.skip_registries,
                &clean_registry_skip_list(skip_registries),
            );
        }

        let mut allow_rules = AllowRules {
            allow: file.data.allow.clone(),
        };
        normalize_allow_rule_min_publish_age(&mut allow_rules)
            .with_context(|| format!("invalid allow rules in {}", file.path.display()))?;
        self.allow_rules.merge_from(&allow_rules);
        Ok(())
    }

    fn apply_env(&mut self) -> Result<()> {
        let compat_global_seconds = env_u64("COOLDOWN_MINUTES")?
            .map(minutes_to_seconds)
            .transpose()?;
        let cargo_global = env::var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE").ok();
        let rfc_global_seconds = cargo_global
            .as_deref()
            .map(parse_duration_seconds)
            .transpose()?;
        if compat_global_seconds
            .zip(rfc_global_seconds)
            .is_some_and(|(compat, rfc)| compat != rfc)
        {
            bail!(
                "environment defines incompatible `COOLDOWN_MINUTES` and `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE` values; use only one global min-publish-age setting"
            );
        }
        if let Some(seconds) = compat_global_seconds {
            self.min_publish_age_seconds = Some(seconds);
        }
        if let Some(seconds) = rfc_global_seconds {
            self.min_publish_age_seconds = Some(seconds);
        }
        if let Ok(value) = env::var("CARGO_REGISTRY_MIN_PUBLISH_AGE") {
            self.registry_min_publish_age.crates_io_seconds = Some(parse_duration_seconds(&value)?);
        }
        let registry_env_overrides = env_registry_min_publish_age_overrides()?;
        self.registry_min_publish_age
            .merge_from(registry_env_overrides);

        let compat_env_policy = env::var("COOLDOWN_ENFORCEMENT")
            .ok()
            .map(|value| IncompatiblePublishAgePolicy::parse_compat_enforcement(&value))
            .transpose()?;
        let env_policy = env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE")
            .ok()
            .map(|value| IncompatiblePublishAgePolicy::parse_incompatible_publish_age(&value))
            .transpose()?;
        if compat_env_policy
            .zip(env_policy)
            .is_some_and(|(compat, incompatible)| compat != incompatible)
        {
            bail!(
                "environment defines incompatible `COOLDOWN_ENFORCEMENT` and `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE` values; use only one incompatible publish age policy setting"
            );
        }
        if let Some(policy) = compat_env_policy {
            self.incompatible_publish_age = Some(policy);
        }
        if let Some(policy) = env_policy {
            self.incompatible_publish_age = Some(policy);
        }
        let compat_env_accept = env::var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT")
            .ok()
            .map(|value| FallbackAccept::parse(&value))
            .transpose()?;
        let env_accept = env::var("COOLDOWN_FALLBACK_ACCEPT")
            .ok()
            .map(|value| FallbackAccept::parse(&value))
            .transpose()?;
        if compat_env_accept
            .zip(env_accept)
            .is_some_and(|(compat, fallback)| compat != fallback)
        {
            bail!(
                "environment defines incompatible `COOLDOWN_CARGO_COMPATIBLE_ACCEPT` and `COOLDOWN_FALLBACK_ACCEPT` values; use only one fallback accept setting"
            );
        }
        if let Some(policy) = compat_env_accept {
            self.fallback_accept = Some(policy);
        }
        if let Some(policy) = env_accept {
            self.fallback_accept = Some(policy);
        }
        if let Ok(value) = env::var("COOLDOWN_LOCKFILE_BASELINE") {
            self.lockfile_baseline = Some(LockfileBaselineMode::parse(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_NOW") {
            self.now_override = Some(parse_datetime(&value)?);
        }
        if let Some(ttl_seconds) = env_u64("COOLDOWN_TTL_SECONDS")? {
            self.ttl_seconds = Some(ttl_seconds);
        }
        if let Some(cache_dir) = env::var_os("COOLDOWN_CACHE_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
        {
            self.cache_dir = Some(cache_dir);
        }
        if let Some(http_retries) = env_u32("COOLDOWN_HTTP_RETRIES")? {
            self.http_retries = Some(validate_http_retries(http_retries)?);
        }
        if let Ok(value) = env::var("COOLDOWN_VERBOSE") {
            self.verbose = Some(parse_bool(&value)?);
        }
        if let Ok(value) = env::var("COOLDOWN_SKIP_REGISTRIES") {
            self.skip_registries = parse_registry_skip_list(&value);
        }
        Ok(())
    }

    fn finish(self) -> Config {
        Config {
            min_publish_age_seconds: self.min_publish_age_seconds.unwrap_or(0),
            registry_min_publish_age: self.registry_min_publish_age,
            incompatible_publish_age: self
                .incompatible_publish_age
                .unwrap_or(IncompatiblePublishAgePolicy::Deny),
            fallback_accept: self.fallback_accept.unwrap_or(FallbackAccept::Prompt),
            lockfile_baseline: self
                .lockfile_baseline
                .unwrap_or(LockfileBaselineMode::Floor),
            now_override: self.now_override,
            ttl_seconds: self.ttl_seconds.unwrap_or(86_400),
            cache_dir: self.cache_dir,
            http_retries: self.http_retries.unwrap_or(2),
            verbose: self.verbose.unwrap_or(false),
            skip_registries: self.skip_registries,
            allow_rules: self.allow_rules,
        }
    }
}

#[derive(Debug, Clone)]
struct FileConfig {
    path: PathBuf,
    data: CooldownFile,
}

impl FileConfig {
    fn base_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    fn resolve_path(&self, candidate: &Path) -> PathBuf {
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.base_dir().join(candidate)
        }
    }

    fn registry_min_publish_age_config(&self) -> Result<RegistryMinPublishAgeConfig> {
        let mut config = RegistryMinPublishAgeConfig::default();

        if let Some(registry) = self.data.registry.as_ref()
            && let Some(value) = registry.min_publish_age.as_deref()
        {
            config.crates_io_seconds = Some(parse_duration_seconds(value)?);
        }

        let mut registries: Vec<_> = self.data.registries.iter().collect();
        registries.sort_by_key(|(left, _)| *left);

        for (name, registry) in registries {
            if registry.min_publish_age.is_none() && registry.index.is_none() {
                continue;
            }
            if let Some(index) = registry.index.as_deref() {
                validate_registry_override_index(index).with_context(|| {
                    format!(
                        "invalid index for [registries.{name}] in {}",
                        self.path.display()
                    )
                })?;
            }
            config.registries.push(RegistryMinPublishAgeOverride {
                name: name.clone(),
                index: registry.index.clone(),
                min_publish_age_seconds: registry
                    .min_publish_age
                    .as_deref()
                    .map(parse_duration_seconds)
                    .transpose()?,
                name_from_env: false,
            });
        }

        Ok(config)
    }
}

fn normalize_allow_rule_min_publish_age(allow_rules: &mut AllowRules) -> Result<()> {
    for package in &mut allow_rules.allow.package {
        let compat_seconds = package
            .minutes
            .map(minutes_to_seconds)
            .transpose()
            .with_context(|| {
                format!(
                    "invalid `minutes` value for [[allow.package]] crate `{}`",
                    package.crate_name
                )
            })?;
        let rfc_seconds = package
            .min_publish_age
            .as_deref()
            .map(parse_duration_seconds)
            .transpose()
            .with_context(|| {
                format!(
                    "invalid `min-publish-age` value for [[allow.package]] crate `{}`",
                    package.crate_name
                )
            })?;

        if compat_seconds
            .zip(rfc_seconds)
            .is_some_and(|(compat, rfc)| compat != rfc)
        {
            bail!(
                "[[allow.package]] crate `{}` defines incompatible `minutes` and `min-publish-age` values; use only one per-package cooldown setting",
                package.crate_name
            );
        }

        package.min_publish_age_seconds = rfc_seconds.or(compat_seconds);
    }
    Ok(())
}

fn merge_registry_skip_lists(base: &[String], overlay: &[String]) -> Vec<String> {
    let mut merged = base.to_vec();
    for entry in overlay {
        if merged.iter().any(|existing| existing == entry) {
            continue;
        }
        merged.push(entry.clone());
    }
    merged
}

fn clean_registry_skip_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn parse_registry_skip_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "1" => Ok(true),
        "0" => Ok(false),
        _ if value.eq_ignore_ascii_case("true") => Ok(true),
        _ if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => bail!("invalid boolean `{value}`; expected one of: true, false, 1, 0"),
    }
}

fn env_registry_min_publish_age_overrides() -> Result<RegistryMinPublishAgeConfig> {
    let mut config = RegistryMinPublishAgeConfig::default();
    let mut overrides = Vec::new();
    for (key, value) in env::vars() {
        let Some(raw_name) = key
            .strip_prefix("CARGO_REGISTRIES_")
            .and_then(|name| name.strip_suffix("_MIN_PUBLISH_AGE"))
        else {
            continue;
        };
        if raw_name.is_empty() {
            continue;
        }
        overrides.push((raw_name.to_ascii_lowercase(), value));
    }

    overrides.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (name, value) in overrides {
        config.registries.push(RegistryMinPublishAgeOverride {
            name,
            index: None,
            min_publish_age_seconds: Some(parse_duration_seconds(&value)?),
            name_from_env: true,
        });
    }
    Ok(config)
}

pub(crate) fn parse_duration_seconds(value: &str) -> Result<u64> {
    let value = value.trim();
    if value == "0" {
        return Ok(0);
    }

    let mut parts = value.split_whitespace();
    let Some(amount) = parts.next() else {
        bail!("invalid min-publish-age duration `{value}`; expected `0` or `N unit`")
    };
    let Some(unit) = parts.next() else {
        bail!("invalid min-publish-age duration `{value}`; expected `0` or `N unit`")
    };
    if parts.next().is_some() {
        bail!("invalid min-publish-age duration `{value}`; expected `0` or `N unit`");
    }

    let amount = amount
        .parse::<u64>()
        .with_context(|| format!("invalid min-publish-age duration amount `{amount}`"))?;
    let multiplier = match unit {
        "second" | "seconds" => 1,
        "minute" | "minutes" => SECONDS_PER_MINUTE,
        "hour" | "hours" => SECONDS_PER_HOUR,
        "day" | "days" => SECONDS_PER_DAY,
        "week" | "weeks" => SECONDS_PER_WEEK,
        "month" | "months" => SECONDS_PER_MONTH,
        _ => bail!(
            "invalid min-publish-age duration unit `{unit}`; expected one of: seconds, minutes, hours, days, weeks, months"
        ),
    };

    amount
        .checked_mul(multiplier)
        .with_context(|| format!("min-publish-age duration `{value}` is too large"))
}

fn minutes_to_seconds(minutes: u64) -> Result<u64> {
    minutes
        .checked_mul(SECONDS_PER_MINUTE)
        .with_context(|| format!("cooldown_minutes value `{minutes}` is too large"))
}

fn minutes_to_seconds_saturating(minutes: u64) -> u64 {
    minutes.saturating_mul(SECONDS_PER_MINUTE)
}

fn validate_http_retries(value: u32) -> Result<u32> {
    if value <= 8 {
        Ok(value)
    } else {
        bail!("invalid http_retries `{value}`; expected a value from 0 to 8")
    }
}

fn env_u64(key: &str) -> Result<Option<u64>> {
    let Ok(value) = env::var(key) else {
        return Ok(None);
    };

    value
        .parse()
        .with_context(|| format!("invalid {key} value `{value}`"))
        .map(Some)
}

fn env_u32(key: &str) -> Result<Option<u32>> {
    let Ok(value) = env::var(key) else {
        return Ok(None);
    };

    value
        .parse()
        .with_context(|| format!("invalid {key} value `{value}`"))
        .map(Some)
}

fn parse_datetime(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .with_context(|| format!("invalid RFC 3339 datetime `{value}`"))
}

fn read_file_config(path: &Path) -> Result<Option<FileConfig>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let data = toml::from_str::<CooldownFile>(&contents)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    Ok(Some(FileConfig {
        path: path.to_path_buf(),
        data,
    }))
}

fn user_config_path() -> Option<PathBuf> {
    let cargo_home = cargo_home_dir()?;
    let path = cargo_home.join("cooldown.toml");
    if path.exists() { Some(path) } else { None }
}

fn cargo_home_dir() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".cargo")))
}

/// Unit tests for configuration loading, precedence, and validation.
#[cfg(test)]
mod tests {
    use super::*;

    use assert_fs::TempDir;
    use assert_fs::prelude::*;
    use std::sync::{Mutex, OnceLock};

    use crate::project::{ProjectKind, ProjectMember};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn with_env_var<F: FnOnce()>(key: &str, value: Option<&str>, f: F) {
        let _guard = env_lock().lock().unwrap();
        let previous = env::var(key).ok();
        match value {
            Some(val) => unsafe { env::set_var(key, val) },
            None => unsafe { env::remove_var(key) },
        }
        f();
        match previous {
            Some(val) => unsafe { env::set_var(key, val) },
            None => unsafe { env::remove_var(key) },
        }
    }

    fn project_fixture(root: &Path, member: Option<&Path>) -> ProjectContext {
        ProjectContext {
            cwd: root.to_path_buf(),
            kind: ProjectKind::Workspace,
            workspace_root: root.to_path_buf(),
            target_directory: root.join("target"),
            members: member
                .map(|path| {
                    vec![ProjectMember {
                        name: "member-a".to_string(),
                        manifest_path: path.join("Cargo.toml"),
                        dir: path.to_path_buf(),
                    }]
                })
                .unwrap_or_default(),
            active_member: member.map(|path| ProjectMember {
                name: "member-a".to_string(),
                manifest_path: path.join("Cargo.toml"),
                dir: path.to_path_buf(),
            }),
        }
    }

    #[test]
    fn skip_registries_support_comma_separated_env() {
        with_env_var(
            "COOLDOWN_SKIP_REGISTRIES",
            Some("crates-io, sparse+https://codeartifact.example/index , mirror"),
            || {
                let root = TempDir::new().unwrap();
                let config = Config::load(&project_fixture(root.path(), None)).unwrap();
                assert_eq!(
                    config.skip_registries,
                    vec![
                        "crates-io".to_string(),
                        "sparse+https://codeartifact.example/index".to_string(),
                        "mirror".to_string(),
                    ]
                );
            },
        );
    }

    #[test]
    fn lockfile_baseline_supports_ignore_env() {
        with_env_var("COOLDOWN_LOCKFILE_BASELINE", Some("ignore"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None)).unwrap();
            assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Ignore);
        });
    }

    #[test]
    fn cooldown_now_parses_rfc3339_override() {
        with_env_var("COOLDOWN_NOW", Some("2026-04-03T00:00:00Z"), || {
            let root = TempDir::new().unwrap();
            let config = Config::load(&project_fixture(root.path(), None)).unwrap();
            assert_eq!(
                config.now_override,
                Some(
                    DateTime::parse_from_rfc3339("2026-04-03T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                )
            );
        });
    }

    #[test]
    fn loads_workspace_cooldown_file() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"skip_registries = ["crates-io", "mirror"]
verbose = true

[cooldown]
incompatible-publish-age = "fallback"
fallback-accept = "auto"
lockfile-baseline = "ignore"

[registry]
global-min-publish-age = "15 minutes"

[[allow.exact]]
crate = "demo"
version = "1.2.3"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, 15 * 60);
        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Fallback
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Auto);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Ignore);
        assert_eq!(
            config.skip_registries,
            vec!["crates-io".to_string(), "mirror".to_string()]
        );
        assert!(config.verbose);
        assert!(config.allow_rules.is_exact_allowed("demo", "1.2.3"));
    }

    #[test]
    fn loads_user_cargo_cooldown_file_when_workspace_missing() {
        let _guard = env_lock().lock().unwrap();

        let root = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        fake_home
            .child(".cargo/cooldown.toml")
            .write_str(
                r#"http_retries = 3

[cooldown]
incompatible-publish-age = "allow"

[registry]
global-min-publish-age = "5 minutes"
"#,
            )
            .unwrap();
        let original_cargo_home = env::var_os("CARGO_HOME");
        let original_home = env::var("HOME").ok();
        let original_user = env::var("USERPROFILE").ok();

        unsafe { env::remove_var("CARGO_HOME") };
        unsafe { env::set_var("HOME", fake_home.path()) };
        unsafe { env::set_var("USERPROFILE", fake_home.path()) };

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, 5 * 60);
        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Allow
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Prompt);
        assert_eq!(config.http_retries, 3);

        match original_cargo_home {
            Some(val) => unsafe { env::set_var("CARGO_HOME", val) },
            None => unsafe { env::remove_var("CARGO_HOME") },
        }
        match original_home {
            Some(val) => unsafe { env::set_var("HOME", val) },
            None => unsafe { env::remove_var("HOME") },
        }
        match original_user {
            Some(val) => unsafe { env::set_var("USERPROFILE", val) },
            None => unsafe { env::remove_var("USERPROFILE") },
        }
    }

    #[test]
    fn environment_overrides_file_configuration() {
        let _guard = env_lock().lock().unwrap();

        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"skip_registries = ["from-file"]

[cooldown]
incompatible-publish-age = "fallback"
fallback-accept = "prompt"
lockfile-baseline = "ignore"
"#,
            )
            .unwrap();

        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        let original_incompatible = env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE").ok();
        let original_accept = env::var("COOLDOWN_FALLBACK_ACCEPT").ok();
        let original_compat_accept = env::var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT").ok();
        let original_lockfile_baseline = env::var("COOLDOWN_LOCKFILE_BASELINE").ok();
        let original_skips = env::var("COOLDOWN_SKIP_REGISTRIES").ok();

        unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") };
        unsafe { env::remove_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT") };
        unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", "allow") };
        unsafe { env::set_var("COOLDOWN_FALLBACK_ACCEPT", "auto") };
        unsafe { env::set_var("COOLDOWN_LOCKFILE_BASELINE", "floor") };
        unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", "from-env") };

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();
        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Allow
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Auto);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Floor);
        assert_eq!(config.skip_registries, vec!["from-env".to_string()]);

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
        match original_incompatible {
            Some(val) => unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", val) },
            None => unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") },
        }
        match original_accept {
            Some(val) => unsafe { env::set_var("COOLDOWN_FALLBACK_ACCEPT", val) },
            None => unsafe { env::remove_var("COOLDOWN_FALLBACK_ACCEPT") },
        }
        match original_compat_accept {
            Some(val) => unsafe { env::set_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", val) },
            None => unsafe { env::remove_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT") },
        }
        match original_lockfile_baseline {
            Some(val) => unsafe { env::set_var("COOLDOWN_LOCKFILE_BASELINE", val) },
            None => unsafe { env::remove_var("COOLDOWN_LOCKFILE_BASELINE") },
        }
        match original_skips {
            Some(val) => unsafe { env::set_var("COOLDOWN_SKIP_REGISTRIES", val) },
            None => unsafe { env::remove_var("COOLDOWN_SKIP_REGISTRIES") },
        }
    }

    #[test]
    fn rejects_unknown_compat_enforcement_form_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"enforcement = "soft""#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid root enforcement `soft`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_compat_enforcement_env_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        unsafe { env::set_var("COOLDOWN_ENFORCEMENT", "soft") };

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid root enforcement `soft`"),
            "{err:#}"
        );

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
    }

    #[test]
    fn rejects_unknown_fallback_accept_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[cooldown]
fallback-accept = "always"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid fallback accept policy `always`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_lockfile_baseline_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[cooldown]
lockfile-baseline = "everything"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("invalid lockfile baseline `everything`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_unknown_file_keys() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(r#"cooldown_seconds = 9"#)
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn rejects_unknown_allow_rule_keys() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[[allow.package]]
crate = "serde"
seconds = 60
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(format!("{err:#}").contains("unknown field"), "{err:#}");
    }

    #[test]
    fn loads_allow_package_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[[allow.package]]
crate = "serde"
min-publish-age = "1 day"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(
            config
                .allow_rules
                .per_crate_min_publish_age_seconds()
                .get("serde"),
            Some(&SECONDS_PER_DAY)
        );
    }

    #[test]
    fn rejects_conflicting_allow_package_duration_forms() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[[allow.package]]
crate = "serde"
minutes = 60
min-publish-age = "1 day"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
        assert!(
            format!("{err:#}").contains("defines incompatible `minutes` and `min-publish-age`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_invalid_verbose_env_value() {
        with_env_var("COOLDOWN_VERBOSE", Some("yes"), || {
            let root = TempDir::new().unwrap();
            let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid boolean `yes`"),
                "{err:#}"
            );
        });
    }

    #[test]
    fn member_file_overrides_workspace_and_merges_allow_rules() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let member_dir = root.child("member-a");
        member_dir.create_dir_all().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"skip_registries = ["workspace-registry"]

[registry]
global-min-publish-age = "30 minutes"

[[allow.package]]
crate = "serde"
minutes = 20
"#,
            )
            .unwrap();
        member_dir
            .child("cooldown.toml")
            .write_str(
                r#"skip_registries = ["member-registry"]

[registry]
global-min-publish-age = "5 minutes"

[allow.global]
minutes = 3

[[allow.package]]
crate = "serde"
minutes = 1

[[allow.exact]]
crate = "foo"
version = "1.2.3"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), Some(member_dir.path()))).unwrap();

        assert_eq!(config.min_publish_age_seconds, 5 * 60);
        assert_eq!(
            config.skip_registries,
            vec![
                "workspace-registry".to_string(),
                "member-registry".to_string(),
            ]
        );
        assert_eq!(config.allow_rules.global_minutes(), Some(3));
        assert_eq!(config.allow_rules.effective_minutes_for("serde", 90), 1);
        assert!(config.allow_rules.is_exact_allowed("foo", "1.2.3"));
    }

    #[test]
    fn member_registry_override_preserves_workspace_index_when_overlay_omits_it() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let member_dir = root.child("member-a");
        member_dir.create_dir_all().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registries.policy-name]
index = "sparse+https://example.com/index/"
min-publish-age = "5 days"
"#,
            )
            .unwrap();
        member_dir
            .child("cooldown.toml")
            .write_str(
                r#"[registries.policy-name]
min-publish-age = "1 day"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), Some(member_dir.path()))).unwrap();

        assert_eq!(config.registry_min_publish_age.registries.len(), 1);
        let registry = &config.registry_min_publish_age.registries[0];
        assert_eq!(registry.name, "policy-name");
        assert_eq!(
            registry.index.as_deref(),
            Some("sparse+https://example.com/index/")
        );
        assert_eq!(registry.min_publish_age_seconds, Some(SECONDS_PER_DAY));
    }

    #[test]
    fn member_registry_override_preserves_workspace_min_publish_age_when_overlay_sets_index() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let member_dir = root.child("member-a");
        member_dir.create_dir_all().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registries.policy-name]
index = "sparse+https://workspace.example/index/"
min-publish-age = "5 days"
"#,
            )
            .unwrap();
        member_dir
            .child("cooldown.toml")
            .write_str(
                r#"[registries.policy-name]
index = "sparse+https://member.example/index/"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), Some(member_dir.path()))).unwrap();

        assert_eq!(config.registry_min_publish_age.registries.len(), 1);
        let registry = &config.registry_min_publish_age.registries[0];
        assert_eq!(registry.name, "policy-name");
        assert_eq!(
            registry.index.as_deref(),
            Some("sparse+https://member.example/index/")
        );
        assert_eq!(registry.min_publish_age_seconds, Some(5 * SECONDS_PER_DAY));
    }

    #[test]
    fn loads_rfc_style_global_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registry]
global-min-publish-age = "14 days"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, 14 * SECONDS_PER_DAY);
    }

    #[test]
    fn loads_canonical_cooldown_section() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[cooldown]
incompatible-publish-age = "fallback"
fallback-accept = "auto"
lockfile-baseline = "ignore"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Fallback
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Auto);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Ignore);
    }

    #[test]
    fn cooldown_incompatible_publish_age_file_maps_modes() {
        let _guard = env_lock().lock().unwrap();

        for (value, expected) in [
            ("deny", IncompatiblePublishAgePolicy::Deny),
            ("allow", IncompatiblePublishAgePolicy::Allow),
            ("fallback", IncompatiblePublishAgePolicy::Fallback),
        ] {
            let root = TempDir::new().unwrap();
            root.child("cooldown.toml")
                .write_str(&format!(
                    "[cooldown]\nincompatible-publish-age = \"{value}\"\n"
                ))
                .unwrap();

            let config = Config::load(&project_fixture(root.path(), None)).unwrap();

            assert_eq!(config.incompatible_publish_age, expected);
        }
    }

    #[test]
    fn rejects_unpublished_cargo_compatible_new_policy_value() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[cooldown]
incompatible-publish-age = "cargo-compatible"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}")
                .contains("invalid incompatible publish age policy `cargo-compatible`"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_conflicting_root_and_cooldown_section_incompatible_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"enforcement = "strict"

[cooldown]
incompatible-publish-age = "fallback"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}")
                .contains("root `enforcement` and `[cooldown].incompatible-publish-age`"),
            "{err:#}"
        );
    }

    #[test]
    fn accepts_equivalent_root_and_cooldown_section_policy() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"enforcement = "cargo_compatible"
cargo_compatible_accept = "prompt"
lockfile_baseline = "floor"

[cooldown]
incompatible-publish-age = "fallback"
fallback-accept = "prompt"
lockfile-baseline = "floor"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Fallback
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Prompt);
        assert_eq!(config.lockfile_baseline, LockfileBaselineMode::Floor);
    }

    #[test]
    fn loads_rfc_style_crates_io_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registry]
global-min-publish-age = "14 days"
min-publish-age = "5 days"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, 14 * SECONDS_PER_DAY);
        assert_eq!(
            config.registry_min_publish_age.crates_io_seconds,
            Some(5 * SECONDS_PER_DAY)
        );
    }

    #[test]
    fn loads_rfc_style_named_registry_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registries.my-org]
index = "https://my.org"
min-publish-age = "0"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.registry_min_publish_age.registries.len(), 1);
        let registry = &config.registry_min_publish_age.registries[0];
        assert_eq!(registry.name, "my-org");
        assert_eq!(registry.index.as_deref(), Some("https://my.org"));
        assert_eq!(registry.min_publish_age_seconds, Some(0));
    }

    #[test]
    fn registry_overrides_load_in_deterministic_name_order() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registries.zeta]
min-publish-age = "3 days"

[registries.alpha]
min-publish-age = "1 day"

[registries.middle]
min-publish-age = "2 days"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(
            config
                .registry_min_publish_age
                .registries
                .iter()
                .map(|registry| registry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "middle", "zeta"]
        );
    }

    #[test]
    fn rejects_invalid_named_registry_index() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"[registries.my-org]
index = "not-a-url"
min-publish-age = "5 days"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}").contains("invalid index for [registries.my-org]"),
            "{err:#}"
        );
    }

    #[test]
    fn rejects_file_with_compat_and_rfc_global_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 60

[registry]
global-min-publish-age = "1 day"
"#,
            )
            .unwrap();

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}").contains("defines incompatible `cooldown_minutes`"),
            "{err:#}"
        );
    }

    #[test]
    fn accepts_file_with_equivalent_compat_and_rfc_global_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        root.child("cooldown.toml")
            .write_str(
                r#"cooldown_minutes = 1440

[registry]
global-min-publish-age = "1 day"
"#,
            )
            .unwrap();

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, SECONDS_PER_DAY);
    }

    #[test]
    fn rejects_env_with_compat_and_rfc_global_min_publish_age() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_compat = env::var("COOLDOWN_MINUTES").ok();
        let original_global = env::var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE").ok();

        unsafe { env::set_var("COOLDOWN_MINUTES", "60") };
        unsafe { env::set_var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", "1 day") };

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}").contains("defines incompatible `COOLDOWN_MINUTES`"),
            "{err:#}"
        );

        match original_compat {
            Some(val) => unsafe { env::set_var("COOLDOWN_MINUTES", val) },
            None => unsafe { env::remove_var("COOLDOWN_MINUTES") },
        }
        match original_global {
            Some(val) => unsafe { env::set_var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", val) },
            None => unsafe { env::remove_var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE") },
        }
    }

    #[test]
    fn rfc_style_min_publish_age_env_is_loaded() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let originals = [
            ("COOLDOWN_MINUTES", env::var("COOLDOWN_MINUTES").ok()),
            (
                "COOLDOWN_ENFORCEMENT",
                env::var("COOLDOWN_ENFORCEMENT").ok(),
            ),
            (
                "COOLDOWN_INCOMPATIBLE_PUBLISH_AGE",
                env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE").ok(),
            ),
            (
                "CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE",
                env::var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE").ok(),
            ),
            (
                "CARGO_REGISTRY_MIN_PUBLISH_AGE",
                env::var("CARGO_REGISTRY_MIN_PUBLISH_AGE").ok(),
            ),
            (
                "CARGO_REGISTRIES_MY_ORG_MIN_PUBLISH_AGE",
                env::var("CARGO_REGISTRIES_MY_ORG_MIN_PUBLISH_AGE").ok(),
            ),
            (
                "CARGO_REGISTRIES_MY_REGISTRY_MIN_PUBLISH_AGE",
                env::var("CARGO_REGISTRIES_MY_REGISTRY_MIN_PUBLISH_AGE").ok(),
            ),
        ];

        unsafe { env::remove_var("COOLDOWN_MINUTES") };
        unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") };
        unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") };
        unsafe { env::set_var("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", "2 weeks") };
        unsafe { env::set_var("CARGO_REGISTRY_MIN_PUBLISH_AGE", "5 days") };
        unsafe { env::set_var("CARGO_REGISTRIES_MY_ORG_MIN_PUBLISH_AGE", "0") };
        unsafe { env::set_var("CARGO_REGISTRIES_MY_REGISTRY_MIN_PUBLISH_AGE", "1 day") };

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(config.min_publish_age_seconds, 2 * SECONDS_PER_WEEK);
        assert_eq!(
            config.registry_min_publish_age.crates_io_seconds,
            Some(5 * SECONDS_PER_DAY)
        );
        assert!(
            config
                .registry_min_publish_age
                .registries
                .iter()
                .any(|registry| {
                    registry.name == "my_org"
                        && registry.name_from_env
                        && registry.min_publish_age_seconds == Some(0)
                })
        );
        assert!(
            config
                .registry_min_publish_age
                .registries
                .iter()
                .any(|registry| {
                    registry.name == "my_registry"
                        && registry.name_from_env
                        && registry.min_publish_age_seconds == Some(SECONDS_PER_DAY)
                })
        );

        for (key, value) in originals {
            match value {
                Some(val) => unsafe { env::set_var(key, val) },
                None => unsafe { env::remove_var(key) },
            }
        }
    }

    #[test]
    fn compat_enforcement_env_keeps_fallback_mode() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        let original_incompatible = env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE").ok();
        let original_accept = env::var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT").ok();
        let original_fallback_accept = env::var("COOLDOWN_FALLBACK_ACCEPT").ok();

        unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") };
        unsafe { env::remove_var("COOLDOWN_FALLBACK_ACCEPT") };
        unsafe { env::set_var("COOLDOWN_ENFORCEMENT", "cargo_compatible") };
        unsafe { env::set_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", "prompt") };

        let config = Config::load(&project_fixture(root.path(), None)).unwrap();

        assert_eq!(
            config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Fallback
        );
        assert_eq!(config.fallback_accept, FallbackAccept::Prompt);

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
        match original_incompatible {
            Some(val) => unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", val) },
            None => unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") },
        }
        match original_accept {
            Some(val) => unsafe { env::set_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", val) },
            None => unsafe { env::remove_var("COOLDOWN_CARGO_COMPATIBLE_ACCEPT") },
        }
        match original_fallback_accept {
            Some(val) => unsafe { env::set_var("COOLDOWN_FALLBACK_ACCEPT", val) },
            None => unsafe { env::remove_var("COOLDOWN_FALLBACK_ACCEPT") },
        }
    }

    #[test]
    fn cooldown_incompatible_publish_age_env_maps_modes() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        let original_incompatible = env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE").ok();

        unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") };
        unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", "deny") };
        let deny_config = Config::load(&project_fixture(root.path(), None)).unwrap();
        assert_eq!(
            deny_config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Deny
        );

        unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", "allow") };
        let allow_config = Config::load(&project_fixture(root.path(), None)).unwrap();
        assert_eq!(
            allow_config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Allow
        );

        unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", "fallback") };
        let compatible_config = Config::load(&project_fixture(root.path(), None)).unwrap();
        assert_eq!(
            compatible_config.incompatible_publish_age,
            IncompatiblePublishAgePolicy::Fallback
        );

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
        match original_incompatible {
            Some(val) => unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", val) },
            None => unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") },
        }
    }

    #[test]
    fn rejects_conflicting_policy_env_forms() {
        let _guard = env_lock().lock().unwrap();
        let root = TempDir::new().unwrap();
        let original_enforcement = env::var("COOLDOWN_ENFORCEMENT").ok();
        let original_incompatible = env::var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE").ok();

        unsafe { env::set_var("COOLDOWN_ENFORCEMENT", "strict") };
        unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", "allow") };

        let err = Config::load(&project_fixture(root.path(), None)).unwrap_err();

        assert!(
            format!("{err:#}").contains("defines incompatible `COOLDOWN_ENFORCEMENT`"),
            "{err:#}"
        );

        match original_enforcement {
            Some(val) => unsafe { env::set_var("COOLDOWN_ENFORCEMENT", val) },
            None => unsafe { env::remove_var("COOLDOWN_ENFORCEMENT") },
        }
        match original_incompatible {
            Some(val) => unsafe { env::set_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE", val) },
            None => unsafe { env::remove_var("COOLDOWN_INCOMPATIBLE_PUBLISH_AGE") },
        }
    }
}
