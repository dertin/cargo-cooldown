//! Registry resolution and release metadata loading.
//!
//! Cargo metadata gives cooldown a source ID, not a ready-to-use release
//! database. This module resolves that source to the effective registry index,
//! reads local index data for versions, timestamps, dependencies, and checksums,
//! and fills missing publish times through the registry API when available.

use std::collections::{BTreeMap, HashMap};
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::Url;
use reqwest::blocking::Client;
use semver::VersionReq;
use serde::{Deserialize, Serialize};
use tame_index::index::{FileLock, IndexCache, IndexConfig, IndexLocation, IndexUrl};
use tame_index::krate::DependencyKind;
use tame_index::utils::canonicalize_url;
use tame_index::{IndexKrate, PathBuf as TamePathBuf};

use crate::cache::Cache;
use crate::config::{Config, RegistryMinPublishAgeOverride};

const CRATES_IO_LEGACY_SOURCE_ID: &str = "registry+https://github.com/rust-lang/crates.io-index";
const CRATES_IO_SPARSE_SOURCE_ID: &str = "sparse+https://index.crates.io/";

/// Source used for a release timestamp.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseSource {
    Index,
    Api,
}

impl ReleaseSource {
    pub fn log_label(self) -> &'static str {
        match self {
            Self::Index => "index_pubtime",
            Self::Api => "registry_api_fallback",
        }
    }
}

/// One crate release with the metadata needed for cooldown decisions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub published_at: Option<DateTime<Utc>>,
    pub yanked: bool,
    pub source: ReleaseSource,
}

/// Chronological release list for one crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseTimeline {
    pub releases: Vec<Release>,
}

impl ReleaseTimeline {
    pub fn release(&self, version: &str) -> Option<&Release> {
        self.releases
            .iter()
            .find(|release| release.version == version)
    }

    pub fn has_missing_timestamps(&self) -> bool {
        self.releases
            .iter()
            .any(|release| release.published_at.is_none())
    }
}

/// Dependency metadata read from the local registry index for one release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseDependency {
    pub crate_name: String,
    pub requirement: VersionReq,
    pub optional: bool,
    pub target_specific: bool,
}

/// Local index metadata required to rewrite a lockfile package entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalReleaseMetadata {
    pub dependencies: Vec<ReleaseDependency>,
    pub checksum: String,
}

/// Resolved registry identity after Cargo config, mirrors, and source replacement.
#[derive(Debug, Clone)]
pub struct RegistryContext {
    pub logical_name: String,
    pub source_id: String,
    pub effective_index_url: String,
    pub api: Option<Url>,
    pub cache_fingerprint: String,
    pub skipped: bool,
    index_root: TamePathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ApiVersion {
    created_at: DateTime<Utc>,
    yanked: bool,
    #[serde(default, alias = "vers")]
    num: String,
}

#[derive(Debug, Deserialize)]
struct CrateResponse {
    versions: Vec<ApiVersion>,
}

/// Cached registry access used across one cooldown execution.
///
/// The store receives Cargo source IDs from metadata and translates them into
/// effective registry contexts. It then serves release timelines, dependency
/// metadata, and checksums from local registry indexes, using the HTTP API only
/// when timestamps are missing from the local data.
pub struct RegistryStore {
    cache: Cache,
    http: Client,
    retries: u32,
    registries: HashMap<String, RegistryContext>,
    timelines: HashMap<(String, String), ReleaseTimeline>,
    index_krates: HashMap<(String, String), Option<IndexKrate>>,
    release_metadata: HashMap<(String, String, String), Option<LocalReleaseMetadata>>,
    skip_registries: Vec<String>,
}

impl RegistryStore {
    /// Create a registry store for one cooldown run.
    ///
    /// Configuration supplies the on-disk cache location, cache TTL, HTTP retry
    /// count, and registries to skip. The returned store keeps additional
    /// in-memory caches so repeated resolver passes do not reload the same crate
    /// metadata.
    pub fn new(config: &Config) -> Result<Self> {
        let cache = if let Some(ref root) = config.cache_dir {
            Cache::with_root(root.clone(), Duration::from_secs(config.ttl_seconds))?
        } else {
            Cache::new(config.ttl_seconds)?
        };

        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("cargo-cooldown/", env!("CARGO_PKG_VERSION")))
            .build()?;

        Ok(Self {
            cache,
            http,
            retries: config.http_retries,
            registries: HashMap::new(),
            timelines: HashMap::new(),
            index_krates: HashMap::new(),
            release_metadata: HashMap::new(),
            skip_registries: config.skip_registries.clone(),
        })
    }

    /// Resolve a Cargo metadata source ID into the effective registry context.
    ///
    /// Cargo may report git-index, sparse, mirrored, or replacement registry URLs.
    /// This function normalizes that input, finds the local index location and
    /// optional API endpoint, applies `skip_registries`, and caches the result for
    /// later timeline or checksum lookups.
    pub fn context_for_source(&mut self, source_id: &str) -> Result<&RegistryContext> {
        if !self.registries.contains_key(source_id) {
            let context = resolve_registry_context(source_id, &self.skip_registries)?;
            self.registries.insert(source_id.to_string(), context);
        }

        self.registries
            .get(source_id)
            .context("resolved registry context should be present")
    }

    /// Load and cache the release timeline for one crate.
    ///
    /// The caller passes the Cargo source ID and crate name from the resolved
    /// graph. The store first reads local index data; if any release timestamp is
    /// missing and the registry exposes an API endpoint, it fetches cached HTTP
    /// fallback data and merges it in. The returned timeline is what cooldown uses
    /// to decide whether a version is fresh and which older versions are valid.
    pub fn timeline_for(&mut self, source_id: &str, crate_name: &str) -> Result<ReleaseTimeline> {
        let cache_key = (source_id.to_string(), crate_name.to_string());
        if let Some(cached) = self.timelines.get(&cache_key) {
            return Ok(cached.clone());
        }

        let context = self.context_for_source(source_id)?.clone();
        if context.skipped {
            return Ok(ReleaseTimeline::default());
        }

        let local = load_local_timeline(&context, crate_name)?;
        // The local index is authoritative when it has `pubtime`; HTTP fallback is
        // only for registries or cache entries that do not expose timestamps.
        let api = if local
            .as_ref()
            .is_none_or(ReleaseTimeline::has_missing_timestamps)
        {
            self.fetch_api_versions(&context, crate_name)?
        } else {
            None
        };

        let timeline = merge_timelines(local, api);
        self.timelines.insert(cache_key, timeline.clone());
        Ok(timeline)
    }

    /// Read non-dev dependencies for one exact release from the local index.
    ///
    /// The dependency solver calls this when it needs to know what would happen
    /// after pinning a package to `version`. `None` means the local index does not
    /// have enough metadata, so the caller should avoid speculative rewriting.
    pub fn local_release_dependencies(
        &mut self,
        source_id: &str,
        crate_name: &str,
        version: &str,
    ) -> Result<Option<Vec<ReleaseDependency>>> {
        Ok(self
            .local_release_metadata(source_id, crate_name, version)?
            .map(|metadata| metadata.dependencies))
    }

    /// Read the checksum Cargo expects for one exact release.
    ///
    /// Lockfile rewrites must update both version and checksum. Returning `None`
    /// means the release cannot be safely written directly from local metadata.
    pub fn local_release_checksum(
        &mut self,
        source_id: &str,
        crate_name: &str,
        version: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .local_release_metadata(source_id, crate_name, version)?
            .map(|metadata| metadata.checksum))
    }

    fn local_release_metadata(
        &mut self,
        source_id: &str,
        crate_name: &str,
        version: &str,
    ) -> Result<Option<LocalReleaseMetadata>> {
        let metadata_key = (
            source_id.to_string(),
            crate_name.to_string(),
            version.to_string(),
        );
        if let Some(cached) = self.release_metadata.get(&metadata_key) {
            return Ok(cached.clone());
        }

        let cache_key = (source_id.to_string(), crate_name.to_string());
        if !self.index_krates.contains_key(&cache_key) {
            let context = self.context_for_source(source_id)?.clone();
            let krate = load_index_krate(&context, crate_name)?;
            self.index_krates.insert(cache_key.clone(), krate);
        }

        let Some(krate) = self
            .index_krates
            .get(&cache_key)
            .and_then(|cached| cached.as_ref())
        else {
            self.release_metadata.insert(metadata_key, None);
            return Ok(None);
        };
        let Some(release) = krate
            .versions
            .iter()
            .find(|candidate| candidate.version == version)
        else {
            self.release_metadata.insert(metadata_key, None);
            return Ok(None);
        };

        let dependencies = release
            .dependencies()
            .iter()
            .filter(|dependency| dependency.kind() != DependencyKind::Dev)
            .map(|dependency| {
                let requirement = dependency.req.parse::<VersionReq>().with_context(|| {
                    format!(
                        "invalid dependency requirement {} for {}@{} -> {}",
                        dependency.req,
                        crate_name,
                        version,
                        dependency.crate_name()
                    )
                })?;
                Ok(ReleaseDependency {
                    crate_name: dependency.crate_name().to_string(),
                    requirement,
                    optional: dependency.is_optional(),
                    target_specific: dependency.target().is_some(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let metadata = Some(LocalReleaseMetadata {
            dependencies,
            checksum: checksum_hex(release.checksum()),
        });
        self.release_metadata.insert(metadata_key, metadata.clone());
        Ok(metadata)
    }

    fn fetch_api_versions(
        &self,
        context: &RegistryContext,
        crate_name: &str,
    ) -> Result<Option<Vec<ApiVersion>>> {
        let Some(api_root) = context.api.as_ref() else {
            return Ok(None);
        };

        let key = format!("registry_api/{}/{crate_name}", context.cache_fingerprint);
        if let Some(cached) = self.cache.get::<Vec<ApiVersion>>(&key)? {
            return Ok(Some(cached));
        }

        let base = registry_api_base(api_root)?;
        let url = base
            .join(&format!("crates/{crate_name}"))
            .with_context(|| format!("failed to build registry API URL for {crate_name}"))?;

        let versions = self.get_json::<CrateResponse>(&url)?.versions;
        self.cache.put(&key, &versions)?;
        Ok(Some(versions))
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &Url) -> Result<T> {
        let mut attempt = 0;
        loop {
            let response = self.http.get(url.clone()).send();
            match response {
                Ok(resp) => {
                    let status_resp = resp.error_for_status()?;
                    let value = status_resp.json::<T>()?;
                    return Ok(value);
                }
                Err(err) => {
                    attempt += 1;
                    if attempt > self.retries {
                        return Err(err.into());
                    }
                    sleep(Duration::from_millis(200 * u64::from(attempt)));
                }
            }
        }
    }
}

/// Return whether a Cargo source id refers to a registry dependency.
pub fn is_registry_source(source: &str) -> bool {
    source.starts_with("registry+") || source.starts_with("sparse+")
}

fn resolve_registry_context(
    source_id: &str,
    skip_registries: &[String],
) -> Result<RegistryContext> {
    let config_root = cargo_config_root();
    let is_crates_io = is_crates_io_source_id(source_id);
    let (index_root, effective_index_url) = if is_crates_io {
        let index_url = IndexUrl::crates_io(config_root.clone(), None, None)?;
        IndexLocation::new(index_url).into_parts()?
    } else {
        resolve_non_crates_io_index_location(source_id)?
    };
    let api = load_index_api(&index_root, is_crates_io)?;
    let normalized_source_id = normalize_registry_identifier(source_id)?;
    let normalized_effective_index_url = normalize_registry_identifier(&effective_index_url)?;
    let skipped = skip_registries
        .iter()
        .try_fold(false, |matched, candidate| {
            if matched {
                Ok(true)
            } else {
                skip_registry_matches(
                    candidate,
                    is_crates_io,
                    &normalized_source_id,
                    &normalized_effective_index_url,
                    config_root.clone(),
                )
            }
        })?;

    Ok(RegistryContext {
        logical_name: if is_crates_io {
            "crates-io".to_string()
        } else {
            effective_index_url.clone()
        },
        source_id: source_id.to_string(),
        effective_index_url: effective_index_url.clone(),
        api,
        cache_fingerprint: registry_cache_fingerprint(&effective_index_url),
        skipped,
        index_root,
    })
}

fn resolve_non_crates_io_index_location(source_id: &str) -> Result<(TamePathBuf, String)> {
    let primary = IndexLocation::new(IndexUrl::from(source_id)).into_parts()?;
    let Some(url) = source_id.strip_prefix("registry+") else {
        return Ok(primary);
    };

    // Cargo can represent a sparse registry in metadata as `registry+URL`.
    // Prefer the sparse cache if that is the cache Cargo actually populated.
    let sparse_source = format!("sparse+{url}");
    let sparse = IndexLocation::new(IndexUrl::from(sparse_source.as_str())).into_parts()?;

    match (
        index_location_exists(&primary.0),
        index_location_exists(&sparse.0),
    ) {
        (false, true) => Ok(sparse),
        _ => Ok(primary),
    }
}

fn index_location_exists(path: &TamePathBuf) -> bool {
    path.join("config.json").exists() || path.join(".cache").exists()
}

fn load_local_timeline(
    context: &RegistryContext,
    crate_name: &str,
) -> Result<Option<ReleaseTimeline>> {
    let Some(krate) = load_index_krate(context, crate_name)? else {
        return Ok(None);
    };

    index_krate_to_timeline(crate_name, &krate).map(Some)
}

fn load_index_krate(context: &RegistryContext, crate_name: &str) -> Result<Option<IndexKrate>> {
    let cache = IndexCache::at_path(context.index_root.clone());
    let lock = FileLock::unlocked();
    let Some(krate) = cache.cached_krate(crate_name.try_into()?, None, &lock)? else {
        return Ok(None);
    };

    Ok(Some(krate))
}

fn index_krate_to_timeline(crate_name: &str, krate: &IndexKrate) -> Result<ReleaseTimeline> {
    let releases = krate
        .versions
        .iter()
        .map(|version| {
            let published_at = version
                .pubtime
                .as_deref()
                .map(|value| parse_pubtime(crate_name, version.version.as_str(), value))
                .transpose()?;

            Ok(Release {
                version: version.version.to_string(),
                published_at,
                yanked: version.yanked,
                source: ReleaseSource::Index,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(ReleaseTimeline { releases })
}

fn merge_timelines(
    local: Option<ReleaseTimeline>,
    api: Option<Vec<ApiVersion>>,
) -> ReleaseTimeline {
    let Some(api_versions) = api else {
        return local.unwrap_or_default();
    };

    let mut api_versions = api_versions;
    api_versions.sort_by_key(|version| version.created_at);

    let Some(local) = local else {
        return ReleaseTimeline {
            releases: api_versions
                .into_iter()
                .map(|version| Release {
                    version: version.num,
                    published_at: Some(version.created_at),
                    yanked: version.yanked,
                    source: ReleaseSource::Api,
                })
                .collect(),
        };
    };

    let mut api_map: BTreeMap<String, ApiVersion> = api_versions
        .into_iter()
        .map(|version| (version.num.clone(), version))
        .collect();

    let mut releases = Vec::with_capacity(local.releases.len() + api_map.len());
    for release in local.releases {
        if let Some(api_version) = api_map.remove(&release.version) {
            // Preserve index data when available, but fill missing publish times
            // from the API so downstream freshness checks can stay fail-closed.
            let published_at = release.published_at.or(Some(api_version.created_at));
            let source = if release.published_at.is_some() {
                release.source
            } else {
                ReleaseSource::Api
            };

            releases.push(Release {
                version: release.version,
                published_at,
                yanked: release.yanked,
                source,
            });
        } else {
            releases.push(release);
        }
    }

    releases.extend(api_map.into_values().map(|version| Release {
        version: version.num,
        published_at: Some(version.created_at),
        yanked: version.yanked,
        source: ReleaseSource::Api,
    }));
    sort_releases_chronologically(&mut releases);

    ReleaseTimeline { releases }
}

fn sort_releases_chronologically(releases: &mut [Release]) {
    releases.sort_by_key(|release| release.published_at);
}

fn load_index_api(index_root: &TamePathBuf, is_crates_io: bool) -> Result<Option<Url>> {
    let path = index_root.join("config.json");
    let api = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str::<IndexConfig>(&contents)?.api,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && is_crates_io => {
            Some("https://crates.io".to_string())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(err).with_context(|| format!("failed to read {path}")),
    };

    api.map(|value| {
        Url::parse(&value).with_context(|| format!("invalid registry API URL in {path}"))
    })
    .transpose()
}

fn registry_api_base(api_root: &Url) -> Result<Url> {
    let raw = api_root.as_str();
    if raw.ends_with("/api/v1/") {
        return Ok(api_root.clone());
    }
    if raw.ends_with("/api/v1") {
        return Url::parse(&format!("{raw}/")).context("invalid registry API base URL");
    }

    api_root
        .join("api/v1/")
        .context("invalid registry API base URL")
}

fn parse_pubtime(crate_name: &str, version: &str, value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .with_context(|| format!("invalid pubtime for {crate_name}@{version}: {value}"))
}

fn skip_registry_matches(
    raw: &str,
    is_crates_io: bool,
    normalized_source_id: &str,
    normalized_effective_index_url: &str,
    config_root: Option<TamePathBuf>,
) -> Result<bool> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(false);
    }

    if trimmed.eq_ignore_ascii_case("crates-io") {
        return Ok(is_crates_io);
    }

    let normalized = if looks_like_registry_identifier(trimmed) {
        normalize_registry_identifier(trimmed)?
    } else {
        let resolved = IndexUrl::for_registry_name(config_root, None, trimmed)?;
        normalize_registry_identifier(resolved.as_str())?
    };

    Ok(normalized == normalized_source_id || normalized == normalized_effective_index_url)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RegistryOverrideMatchPriority {
    ResolvedName = 1,
    LogicalName = 2,
    ExplicitIndex = 3,
}

pub fn registry_override_match_priority(
    override_config: &RegistryMinPublishAgeOverride,
    context: &RegistryContext,
) -> Result<Option<RegistryOverrideMatchPriority>> {
    let normalized_source_id = normalize_registry_identifier(&context.source_id)?;
    let normalized_effective_index_url =
        normalize_registry_identifier(&context.effective_index_url)?;

    if let Some(index) = override_config.index.as_deref() {
        let normalized_index = normalize_registry_identifier(index)?;
        return Ok((normalized_index == normalized_source_id
            || normalized_index == normalized_effective_index_url)
            .then_some(RegistryOverrideMatchPriority::ExplicitIndex));
    }

    if override_config
        .name
        .eq_ignore_ascii_case(context.logical_name.as_str())
        || (override_config.name_from_env
            && override_config.name == cargo_env_registry_name(&context.logical_name))
    {
        return Ok(Some(RegistryOverrideMatchPriority::LogicalName));
    }

    if override_config.name_from_env {
        return env_registry_override_match_priority(
            &override_config.name,
            context.logical_name == "crates-io",
            &normalized_source_id,
            &normalized_effective_index_url,
        );
    }

    let matches = skip_registry_matches(
        &override_config.name,
        context.logical_name == "crates-io",
        &normalized_source_id,
        &normalized_effective_index_url,
        cargo_config_root(),
    )?;
    Ok(matches.then_some(RegistryOverrideMatchPriority::ResolvedName))
}

fn env_registry_override_match_priority(
    env_name: &str,
    is_crates_io: bool,
    normalized_source_id: &str,
    normalized_effective_index_url: &str,
) -> Result<Option<RegistryOverrideMatchPriority>> {
    match skip_registry_matches(
        env_name,
        is_crates_io,
        normalized_source_id,
        normalized_effective_index_url,
        cargo_config_root(),
    ) {
        Ok(matches) => Ok(matches.then_some(RegistryOverrideMatchPriority::ResolvedName)),
        Err(raw_err) => {
            let dashed_name = env_name.replace('_', "-");
            if dashed_name == env_name {
                return Err(raw_err);
            }

            let matches = skip_registry_matches(
                &dashed_name,
                is_crates_io,
                normalized_source_id,
                normalized_effective_index_url,
                cargo_config_root(),
            )
            .map_err(|_| raw_err)?;
            Ok(matches.then_some(RegistryOverrideMatchPriority::ResolvedName))
        }
    }
}

pub fn validate_registry_override_index(index: &str) -> Result<()> {
    if !looks_like_registry_identifier(index.trim()) {
        bail!("registry override index must be a registry URL or source ID");
    }
    normalize_registry_identifier(index).map(|_| ())
}

fn looks_like_registry_identifier(value: &str) -> bool {
    value.starts_with("registry+") || value.starts_with("sparse+") || value.contains("://")
}

fn cargo_env_registry_name(logical_name: &str) -> String {
    logical_name.to_ascii_lowercase().replace('-', "_")
}

fn is_crates_io_source_id(source_id: &str) -> bool {
    source_id == CRATES_IO_LEGACY_SOURCE_ID || source_id == CRATES_IO_SPARSE_SOURCE_ID
}

fn normalize_registry_identifier(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Some(url) = trimmed.strip_prefix("sparse+") {
        return Ok(format!("sparse+{}", canonicalize_url(url)?));
    }
    if let Some(url) = trimmed.strip_prefix("registry+") {
        return Ok(canonicalize_url(url)?);
    }
    if trimmed.contains("://") {
        return Ok(canonicalize_url(trimmed)?);
    }

    Ok(trimmed.to_ascii_lowercase())
}

fn registry_cache_fingerprint(value: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn cargo_config_root() -> Option<TamePathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|path| TamePathBuf::from_path_buf(path).ok())
}

fn checksum_hex(bytes: &[u8; 32]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// Extract a release timestamp or explain which registry metadata is missing.
pub fn assert_has_timestamp(
    context: &RegistryContext,
    crate_name: &str,
    release: &Release,
) -> Result<DateTime<Utc>> {
    release.published_at.with_context(|| {
        format!(
            "missing release timestamp for {crate_name}@{} from registry {}. Either provide registry metadata or skip that registry via COOLDOWN_SKIP_REGISTRIES.",
            release.version, context.logical_name
        )
    })
}

/// Find a release in a loaded timeline with a registry-aware error message.
pub fn require_release<'a>(
    timeline: &'a ReleaseTimeline,
    context: &RegistryContext,
    crate_name: &str,
    version: &str,
) -> Result<&'a Release> {
    timeline.release(version).with_context(|| {
        format!(
            "registry {} ({}) does not contain metadata for {crate_name}@{version}",
            context.logical_name, context.source_id
        )
    })
}

/// Fail early when a registry timeline has no usable release metadata.
pub fn ensure_timeline_available(
    context: &RegistryContext,
    crate_name: &str,
    timeline: &ReleaseTimeline,
) -> Result<()> {
    if timeline.releases.is_empty() {
        bail!(
            "registry {} ({}) does not provide cached metadata for crate {crate_name}, and no fallback data could be loaded",
            context.logical_name,
            context.source_id
        );
    }

    Ok(())
}

/// Unit tests for registry resolution, timeline merging, and skip matching.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::allow_rules::AllowRules;
    use crate::config::{
        Config, FallbackAccept, IncompatiblePublishAgePolicy, LockfileBaselineMode,
        RegistryMinPublishAgeConfig,
    };
    use chrono::TimeZone;
    use std::fs;
    use tame_index::IndexVersion;
    use tempfile::tempdir;

    #[test]
    fn merge_prefers_index_data_and_fills_missing_pubtime() {
        let local = ReleaseTimeline {
            releases: vec![
                Release {
                    version: "1.0.0".to_string(),
                    published_at: None,
                    yanked: false,
                    source: ReleaseSource::Index,
                },
                Release {
                    version: "1.1.0".to_string(),
                    published_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()),
                    yanked: false,
                    source: ReleaseSource::Index,
                },
            ],
        };
        let api = vec![
            ApiVersion {
                created_at: Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
                yanked: false,
                num: "1.0.0".to_string(),
            },
            ApiVersion {
                created_at: Utc.with_ymd_and_hms(2026, 1, 3, 0, 0, 0).unwrap(),
                yanked: false,
                num: "1.2.0".to_string(),
            },
        ];

        let merged = merge_timelines(Some(local), Some(api));
        assert_eq!(merged.releases.len(), 3);
        assert_eq!(
            merged
                .release("1.0.0")
                .and_then(|release| release.published_at),
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
        );
        assert_eq!(
            merged.release("1.0.0").map(|release| release.source),
            Some(ReleaseSource::Api)
        );
    }

    #[test]
    fn merge_keeps_api_only_releases_chronological() {
        let local = ReleaseTimeline {
            releases: vec![Release {
                version: "2.0.0".to_string(),
                published_at: Some(Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap()),
                yanked: false,
                source: ReleaseSource::Index,
            }],
        };
        let api = vec![
            ApiVersion {
                created_at: Utc.with_ymd_and_hms(2026, 1, 20, 0, 0, 0).unwrap(),
                yanked: false,
                num: "1.9.0".to_string(),
            },
            ApiVersion {
                created_at: Utc.with_ymd_and_hms(2026, 1, 10, 0, 0, 0).unwrap(),
                yanked: false,
                num: "1.10.0".to_string(),
            },
        ];

        let merged = merge_timelines(Some(local), Some(api));

        assert_eq!(
            merged
                .releases
                .iter()
                .map(|release| release.version.as_str())
                .collect::<Vec<_>>(),
            vec!["1.10.0", "1.9.0", "2.0.0"]
        );
    }

    #[test]
    fn registry_api_base_adds_api_v1_only_once() {
        let base = Url::parse("https://example.com").unwrap();
        assert_eq!(
            registry_api_base(&base).unwrap().as_str(),
            "https://example.com/api/v1/"
        );

        let already = Url::parse("https://example.com/api/v1/").unwrap();
        assert_eq!(
            registry_api_base(&already).unwrap().as_str(),
            "https://example.com/api/v1/"
        );
    }

    #[test]
    fn skip_registry_matches_name_or_url() {
        let source_id =
            normalize_registry_identifier("registry+https://mirror.example/index").unwrap();
        let effective = normalize_registry_identifier("https://mirror.example/index").unwrap();
        let root = cargo_config_root();

        assert!(
            skip_registry_matches(
                "https://mirror.example/index",
                false,
                &source_id,
                &effective,
                root.clone()
            )
            .unwrap()
        );
        assert!(
            skip_registry_matches(
                "registry+https://mirror.example/index",
                false,
                &source_id,
                &effective,
                root
            )
            .unwrap()
        );
    }

    #[test]
    fn crates_io_min_publish_age_override_wins_over_global() {
        let context = RegistryContext {
            logical_name: "crates-io".to_string(),
            source_id: CRATES_IO_LEGACY_SOURCE_ID.to_string(),
            effective_index_url: "https://github.com/rust-lang/crates.io-index".to_string(),
            api: None,
            cache_fingerprint: "test".to_string(),
            skipped: false,
            index_root: TamePathBuf::new(),
        };
        let config = Config {
            min_publish_age_seconds: 14 * 24 * 60 * 60,
            registry_min_publish_age: RegistryMinPublishAgeConfig {
                crates_io_seconds: Some(5 * 24 * 60 * 60),
                registries: Vec::new(),
            },
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
        };

        assert_eq!(
            config
                .min_publish_age_seconds_for(&context, "serde")
                .unwrap(),
            5 * 24 * 60 * 60
        );
    }

    #[test]
    fn explicit_index_registry_override_wins_over_name_match_regardless_of_order() {
        let context = RegistryContext {
            logical_name: "cool-reg".to_string(),
            source_id: "sparse+https://example.com/index/".to_string(),
            effective_index_url: "sparse+https://example.com/index/".to_string(),
            api: None,
            cache_fingerprint: "test".to_string(),
            skipped: false,
            index_root: TamePathBuf::new(),
        };
        let name_override = RegistryMinPublishAgeOverride {
            name: "cool-reg".to_string(),
            index: None,
            min_publish_age_seconds: Some(24 * 60 * 60),
            name_from_env: false,
        };
        let index_override = RegistryMinPublishAgeOverride {
            name: "policy-name".to_string(),
            index: Some("sparse+https://example.com/index/".to_string()),
            min_publish_age_seconds: Some(0),
            name_from_env: false,
        };

        for registries in [
            vec![name_override.clone(), index_override.clone()],
            vec![index_override, name_override],
        ] {
            let config = Config {
                min_publish_age_seconds: 14 * 24 * 60 * 60,
                registry_min_publish_age: RegistryMinPublishAgeConfig {
                    crates_io_seconds: None,
                    registries,
                },
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
            };

            assert_eq!(
                config
                    .min_publish_age_seconds_for(&context, "serde")
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn invalid_registry_override_name_errors_instead_of_falling_back() {
        let context = RegistryContext {
            logical_name: "cool-reg".to_string(),
            source_id: "sparse+https://example.com/index/".to_string(),
            effective_index_url: "sparse+https://example.com/index/".to_string(),
            api: None,
            cache_fingerprint: "test".to_string(),
            skipped: false,
            index_root: TamePathBuf::new(),
        };
        let config = Config {
            min_publish_age_seconds: 0,
            registry_min_publish_age: RegistryMinPublishAgeConfig {
                crates_io_seconds: None,
                registries: vec![RegistryMinPublishAgeOverride {
                    name: "registry-that-does-not-exist".to_string(),
                    index: None,
                    min_publish_age_seconds: Some(24 * 60 * 60),
                    name_from_env: true,
                }],
            },
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
        };

        let err = config
            .min_publish_age_seconds_for(&context, "serde")
            .unwrap_err();
        assert!(
            format!("{err:#}").contains(
                "failed to evaluate min-publish-age override for [registries.registry-that-does-not-exist]"
            ),
            "{err:#}"
        );
    }

    #[test]
    fn normalize_registry_identifier_handles_known_prefixes() {
        assert_eq!(
            normalize_registry_identifier("registry+https://mirror.example/index").unwrap(),
            "https://mirror.example/index"
        );
        assert_eq!(
            normalize_registry_identifier("sparse+https://mirror.example/index").unwrap(),
            "sparse+https://mirror.example/index"
        );
        assert_eq!(
            normalize_registry_identifier("CrAtEs-IO").unwrap(),
            "crates-io"
        );
    }

    #[test]
    fn registry_cache_fingerprint_uses_stable_hash() {
        assert_eq!(
            registry_cache_fingerprint("https://mirror.example/index"),
            "6575af8469572506"
        );
    }

    #[test]
    fn helper_checks_surface_meaningful_registry_errors() {
        let context = RegistryContext {
            logical_name: "mirror".to_string(),
            source_id: "sparse+https://mirror.example/index".to_string(),
            effective_index_url: "https://mirror.example/index".to_string(),
            api: None,
            cache_fingerprint: "test".to_string(),
            skipped: false,
            index_root: TamePathBuf::new(),
        };
        let timeline = ReleaseTimeline::default();

        let timeline_err = ensure_timeline_available(&context, "demo", &timeline).unwrap_err();
        assert!(
            timeline_err
                .to_string()
                .contains("does not provide cached metadata")
        );

        let release = Release {
            version: "1.0.0".to_string(),
            published_at: None,
            yanked: false,
            source: ReleaseSource::Index,
        };
        let timestamp_err = assert_has_timestamp(&context, "demo", &release).unwrap_err();
        assert!(
            timestamp_err
                .to_string()
                .contains("missing release timestamp")
        );

        let missing_release =
            require_release(&ReleaseTimeline::default(), &context, "demo", "1.0.0").unwrap_err();
        assert!(
            missing_release
                .to_string()
                .contains("does not contain metadata")
        );
    }

    #[test]
    fn load_index_api_uses_crates_io_default_when_config_is_missing() {
        let dir = tempdir().unwrap();
        let root = TamePathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();

        let api = load_index_api(&root, true).unwrap();
        assert_eq!(api.unwrap().as_str(), "https://crates.io/");

        let non_crates_api = load_index_api(&root, false).unwrap();
        assert!(non_crates_api.is_none());
    }

    #[test]
    fn loads_sparse_timeline_from_local_cache() {
        let dir = tempdir().unwrap();
        let index_root = TamePathBuf::from_path_buf(dir.path().join("registry-index")).unwrap();
        let effective_index_url = "sparse+https://index.example.test/".to_string();

        fs::create_dir_all(&index_root).unwrap();
        fs::write(
            index_root.join("config.json"),
            r#"{"dl":"https://index.example.test/api/v1/crates","api":"https://index.example.test"}"#,
        )
        .unwrap();

        let mut version = IndexVersion::fake("demo", "1.0.0");
        version.pubtime = Some("2026-01-01T00:00:00Z".into());
        let krate = IndexKrate {
            versions: vec![version],
        };
        let lock = FileLock::unlocked();
        IndexCache::at_path(index_root.clone())
            .write_to_cache(&krate, "etag: test", &lock)
            .unwrap();

        let context = RegistryContext {
            logical_name: "https://index.example.test/".to_string(),
            source_id: "sparse+https://index.example.test/".to_string(),
            effective_index_url,
            api: Some(Url::parse("https://index.example.test").unwrap()),
            cache_fingerprint: "test".to_string(),
            skipped: false,
            index_root,
        };

        let timeline = load_local_timeline(&context, "demo").unwrap().unwrap();
        assert_eq!(timeline.releases.len(), 1);
        assert_eq!(timeline.releases[0].version, "1.0.0");
        assert_eq!(
            timeline.releases[0].published_at,
            Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap())
        );
    }
}
