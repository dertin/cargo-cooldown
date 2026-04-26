use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result};
use cargo_metadata::{Dependency, Metadata, Node, Package, PackageId, Resolve};
use chrono::{DateTime, Utc};
use semver::{Op, VersionReq};
use tracing::debug;

use crate::config::Config;
use crate::lockfile::LockfileSnapshot;
use crate::registry::{
    RegistryContext, RegistryStore, ReleaseSource, assert_has_timestamp, ensure_timeline_available,
    is_registry_source, require_release,
};
use crate::resolver::{cutoff_time, is_release_fresh};
use clap_cargo::Workspace;

#[derive(Clone, Debug)]
pub struct FreshCrate {
    pub package_id: PackageId,
    pub name: String,
    pub source_id: String,
    pub current_version: String,
    pub minimum_minutes: u64,
}

#[derive(Clone, Debug)]
pub struct CrateState {
    pub name: String,
    pub source_id: String,
    pub current_version: String,
    pub minimum_minutes: u64,
    pub exact_allowed: bool,
    pub skipped: bool,
    pub baseline_exempt: bool,
}

impl CrateState {
    pub fn is_cooldown_exempt(&self) -> bool {
        self.exact_allowed || self.minimum_minutes == 0 || self.skipped || self.baseline_exempt
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseInspectionKey {
    pub source_id: String,
    pub crate_name: String,
    pub current_version: String,
    pub minimum_minutes: u64,
}

#[derive(Clone, Debug)]
pub struct ReleaseInspection {
    pub published_at: DateTime<Utc>,
    pub release_time_source: ReleaseSource,
    pub fresh: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RequirementOrigin {
    pub parent_id: PackageId,
    pub parent_name: String,
    pub requirement: String,
}

impl RequirementOrigin {
    pub fn requirement_req(&self) -> VersionReq {
        VersionReq::parse(&self.requirement).expect("requirement origins store valid semver")
    }
}

#[derive(Debug, Default, Clone)]
pub struct ScanSummary {
    pub registry_packages: usize,
    pub inspected: usize,
    pub fresh: usize,
    pub baseline_exempt: usize,
    pub best_effort_skipped: usize,
    pub skipped: usize,
    pub exact_allowed: usize,
    pub zero_minutes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ManifestDependencyRecord {
    name: String,
    rename: Option<String>,
    requirement: String,
}

impl ManifestDependencyRecord {
    fn requirement_req(&self) -> VersionReq {
        VersionReq::parse(&self.requirement).expect("manifest dependency stores valid semver")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotPackage {
    id: PackageId,
    name: String,
    version: String,
    source_id: Option<String>,
    dependencies: Vec<ManifestDependencyRecord>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotNodeDep {
    name: String,
    pkg: PackageId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SnapshotNode {
    id: PackageId,
    deps: Vec<SnapshotNodeDep>,
}

#[derive(Clone, Debug)]
pub struct CargoSnapshot {
    packages: HashMap<PackageId, SnapshotPackage>,
    nodes: HashMap<PackageId, SnapshotNode>,
    reachable_order: Vec<PackageId>,
}

impl CargoSnapshot {
    pub fn from_metadata(metadata: Metadata, workspace: &Workspace) -> Result<Self> {
        let resolve = metadata
            .resolve
            .clone()
            .context("cargo metadata output did not include a resolved dependency graph")?;
        let selected_root_ids = selected_package_ids(&metadata, workspace);
        let reachable_ids = reachable_package_ids(&resolve, &selected_root_ids);
        let packages = metadata
            .packages
            .into_iter()
            .map(snapshot_package)
            .map(|pkg| (pkg.id.clone(), pkg))
            .collect::<HashMap<_, _>>();
        let mut nodes = HashMap::new();
        let mut reachable_order = Vec::new();
        let mut seen = HashSet::new();

        for node in resolve.nodes {
            if !reachable_ids.contains(&node.id) || !seen.insert(node.id.clone()) {
                continue;
            }
            let snapshot_node = snapshot_node(&node);
            reachable_order.push(snapshot_node.id.clone());
            nodes.insert(snapshot_node.id.clone(), snapshot_node);
        }

        Ok(Self {
            packages,
            nodes,
            reachable_order,
        })
    }

    pub fn reachable_order(&self) -> &[PackageId] {
        &self.reachable_order
    }

    fn package(&self, package_id: &PackageId) -> Option<&SnapshotPackage> {
        self.packages.get(package_id)
    }

    fn node(&self, package_id: &PackageId) -> Option<&SnapshotNode> {
        self.nodes.get(package_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ConstraintContribution {
    child_id: PackageId,
    parent_id: PackageId,
    parent_name: String,
    requirement: String,
    exact_count: usize,
}

#[derive(Clone, Debug)]
struct PackageScanRecord {
    state: CrateState,
    inspected: bool,
    fresh: bool,
    best_effort_skipped: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ResolutionState {
    pub crate_states: HashMap<PackageId, CrateState>,
    pub version_requirements: HashMap<PackageId, Vec<VersionReq>>,
    pub requirement_origins: HashMap<PackageId, Vec<RequirementOrigin>>,
    pub equality_dependents: HashMap<PackageId, Vec<PackageId>>,
    pub scan_summary: ScanSummary,
    pub fresh_keys_present: HashSet<String>,
    fresh_entries: HashMap<PackageId, FreshCrate>,
    best_effort_entries: HashMap<PackageId, FreshCrate>,
    version_requirement_counts: HashMap<PackageId, HashMap<String, (VersionReq, usize)>>,
    requirement_origin_counts: RequirementOriginCounts,
    equality_dependent_counts: HashMap<PackageId, HashMap<PackageId, usize>>,
}

type RequirementOriginCounts =
    HashMap<PackageId, HashMap<(PackageId, String), (RequirementOrigin, usize)>>;

struct ResolutionScanCtx<'a> {
    snapshot: &'a CargoSnapshot,
    config: &'a Config,
    initial_lockfile: &'a LockfileSnapshot,
    registry_store: &'a mut RegistryStore,
    inspection_cache: &'a mut HashMap<ReleaseInspectionKey, ReleaseInspection>,
    best_effort_skips: &'a HashMap<String, String>,
    now: DateTime<Utc>,
}

impl ResolutionState {
    pub fn fresh_entries_vec(&self) -> Vec<FreshCrate> {
        self.fresh_entries.values().cloned().collect()
    }

    pub fn best_effort_entries_vec(&self) -> Vec<FreshCrate> {
        self.best_effort_entries.values().cloned().collect()
    }

    fn apply_node(
        &mut self,
        package_id: &PackageId,
        ctx: &mut ResolutionScanCtx<'_>,
    ) -> Result<()> {
        let Some(package) = ctx.snapshot.package(package_id) else {
            return Ok(());
        };
        let Some(node) = ctx.snapshot.node(package_id) else {
            return Ok(());
        };

        let contributions = constraint_contributions(node, package, ctx.snapshot);
        for contribution in &contributions {
            self.add_contribution(contribution);
        }

        let Some(source_id) = &package.source_id else {
            return Ok(());
        };
        if !is_registry_source(source_id) {
            return Ok(());
        }

        let context = ctx.registry_store.context_for_source(source_id)?.clone();
        let current_version = package.version.clone();
        let minimum_minutes = effective_minimum_minutes(ctx.config, &package.name);
        let exact_allowed = ctx
            .config
            .allow_rules
            .is_exact_allowed(package.name.as_str(), &current_version);
        let baseline_exempt = !ctx.config.lockfile_policy.applies_to_existing_lockfile()
            && ctx.initial_lockfile.baseline().contains_registry_version(
                package.name.as_str(),
                &context.effective_index_url,
                &current_version,
            );
        let state = CrateState {
            name: package.name.clone(),
            source_id: source_id.clone(),
            current_version: current_version.clone(),
            minimum_minutes,
            exact_allowed,
            skipped: context.skipped,
            baseline_exempt,
        };
        let mut record = PackageScanRecord {
            state: state.clone(),
            inspected: false,
            fresh: false,
            best_effort_skipped: false,
        };
        self.crate_states.insert(package_id.clone(), state.clone());

        if !state.is_cooldown_exempt() {
            record.inspected = true;
            let (inspection, cache_hit) = inspect_current_release(
                ctx.registry_store,
                ctx.inspection_cache,
                &context,
                &state,
                ctx.now,
            )?;
            let cutoff = cutoff_time(minimum_minutes, ctx.now);
            debug!(
                crate = %package.name,
                version = %current_version,
                published_at = %inspection.published_at,
                release_time_source = inspection.release_time_source.log_label(),
                cutoff = %cutoff,
                cache = if cache_hit { "hit" } else { "miss" },
                registry = %context.effective_index_url,
                "evaluated release age for locked dependency"
            );
            debug!(
                "cooldown: {} crate={} version={} registry={} published_at={} cutoff={} release_time_source={} cache={}",
                if cache_hit { "reused" } else { "inspected" },
                package.name,
                current_version,
                context.effective_index_url,
                inspection.published_at,
                cutoff,
                inspection.release_time_source.log_label(),
                if cache_hit { "hit" } else { "miss" },
            );
            record.fresh = inspection.fresh;
            if inspection.fresh {
                let fresh = FreshCrate {
                    package_id: package_id.clone(),
                    name: package.name.clone(),
                    source_id: source_id.clone(),
                    current_version: current_version.clone(),
                    minimum_minutes,
                };
                let key = crate_failure_key(source_id, package.name.as_str(), &current_version);
                if ctx.best_effort_skips.contains_key(&key) {
                    record.best_effort_skipped = true;
                    self.best_effort_entries.insert(package_id.clone(), fresh);
                } else {
                    self.fresh_entries.insert(package_id.clone(), fresh);
                }
            }
        }

        self.add_scan_record(package_id.clone(), record);
        Ok(())
    }

    fn add_contribution(&mut self, contribution: &ConstraintContribution) {
        let child_id = &contribution.child_id;

        let requirements = self
            .version_requirement_counts
            .entry(child_id.clone())
            .or_default();
        let requirement_entry = requirements
            .entry(contribution.requirement.clone())
            .or_insert_with(|| {
                (
                    VersionReq::parse(&contribution.requirement)
                        .expect("constraint contributions store valid semver"),
                    0,
                )
            });
        requirement_entry.1 += 1;
        self.sync_version_requirements(child_id);

        let origin = RequirementOrigin {
            parent_id: contribution.parent_id.clone(),
            parent_name: contribution.parent_name.clone(),
            requirement: contribution.requirement.clone(),
        };
        let origins = self
            .requirement_origin_counts
            .entry(child_id.clone())
            .or_default();
        let origin_key = (origin.parent_id.clone(), origin.requirement.clone());
        let origin_entry = origins.entry(origin_key).or_insert_with(|| (origin, 0));
        origin_entry.1 += 1;
        self.sync_requirement_origins(child_id);

        if contribution.exact_count > 0 {
            *self
                .equality_dependent_counts
                .entry(child_id.clone())
                .or_default()
                .entry(contribution.parent_id.clone())
                .or_default() += contribution.exact_count;
            self.sync_equality_dependents(child_id);
        }
    }

    fn sync_version_requirements(&mut self, child_id: &PackageId) {
        if let Some(requirements) = self.version_requirement_counts.get(child_id) {
            self.version_requirements.insert(
                child_id.clone(),
                requirements.values().map(|(req, _)| req.clone()).collect(),
            );
        } else {
            self.version_requirements.remove(child_id);
        }
    }

    fn sync_requirement_origins(&mut self, child_id: &PackageId) {
        if let Some(origins) = self.requirement_origin_counts.get(child_id) {
            self.requirement_origins.insert(
                child_id.clone(),
                origins.values().map(|(origin, _)| origin.clone()).collect(),
            );
        } else {
            self.requirement_origins.remove(child_id);
        }
    }

    fn sync_equality_dependents(&mut self, child_id: &PackageId) {
        if let Some(parents) = self.equality_dependent_counts.get(child_id) {
            let mut dependents = Vec::new();
            for (parent_id, count) in parents {
                for _ in 0..*count {
                    dependents.push(parent_id.clone());
                }
            }
            self.equality_dependents
                .insert(child_id.clone(), dependents);
        } else {
            self.equality_dependents.remove(child_id);
        }
    }

    fn add_scan_record(&mut self, _package_id: PackageId, record: PackageScanRecord) {
        self.scan_summary.registry_packages += 1;
        self.scan_summary.baseline_exempt += usize::from(record.state.baseline_exempt);
        self.scan_summary.skipped += usize::from(record.state.skipped);
        self.scan_summary.exact_allowed += usize::from(record.state.exact_allowed);
        self.scan_summary.zero_minutes += usize::from(record.state.minimum_minutes == 0);
        self.scan_summary.inspected += usize::from(record.inspected);
        self.scan_summary.fresh += usize::from(record.fresh && !record.best_effort_skipped);
        self.scan_summary.best_effort_skipped += usize::from(record.best_effort_skipped);
        if record.fresh {
            self.fresh_keys_present.insert(crate_failure_key(
                &record.state.source_id,
                &record.state.name,
                &record.state.current_version,
            ));
        }
    }
}

pub fn build_resolution_state(
    snapshot: &CargoSnapshot,
    config: &Config,
    initial_lockfile: &LockfileSnapshot,
    registry_store: &mut RegistryStore,
    inspection_cache: &mut HashMap<ReleaseInspectionKey, ReleaseInspection>,
    best_effort_skips: &HashMap<String, String>,
    now: DateTime<Utc>,
) -> Result<ResolutionState> {
    let mut state = ResolutionState::default();
    let mut ctx = ResolutionScanCtx {
        snapshot,
        config,
        initial_lockfile,
        registry_store,
        inspection_cache,
        best_effort_skips,
        now,
    };
    for package_id in snapshot.reachable_order() {
        state.apply_node(package_id, &mut ctx)?;
    }
    Ok(state)
}

pub fn crate_failure_key(source_id: &str, name: &str, current_version: &str) -> String {
    format!("{source_id}::{name}@{current_version}")
}

pub fn selected_package_ids(metadata: &Metadata, workspace: &Workspace) -> HashSet<PackageId> {
    workspace
        .partition_packages(metadata)
        .0
        .into_iter()
        .map(|package| package.id.clone())
        .collect()
}

pub fn reachable_package_ids(
    resolve: &Resolve,
    selected_root_ids: &HashSet<PackageId>,
) -> HashSet<PackageId> {
    let nodes_by_id: HashMap<PackageId, &Node> = resolve
        .nodes
        .iter()
        .map(|node| (node.id.clone(), node))
        .collect();
    let mut reachable = HashSet::new();
    let mut queue: VecDeque<PackageId> = selected_root_ids.iter().cloned().collect();

    while let Some(package_id) = queue.pop_front() {
        if !reachable.insert(package_id.clone()) {
            continue;
        }
        if let Some(node) = nodes_by_id.get(&package_id) {
            queue.extend(node.deps.iter().map(|dep| dep.pkg.clone()));
        }
    }

    reachable
}

fn snapshot_package(package: Package) -> SnapshotPackage {
    SnapshotPackage {
        id: package.id,
        name: package.name.to_string(),
        version: package.version.to_string(),
        source_id: package.source.map(|source| source.repr),
        dependencies: package
            .dependencies
            .into_iter()
            .map(snapshot_manifest_dependency)
            .collect(),
    }
}

fn snapshot_manifest_dependency(dependency: Dependency) -> ManifestDependencyRecord {
    ManifestDependencyRecord {
        name: dependency.name,
        rename: dependency.rename,
        requirement: dependency.req.to_string(),
    }
}

fn snapshot_node(node: &Node) -> SnapshotNode {
    SnapshotNode {
        id: node.id.clone(),
        deps: node
            .deps
            .iter()
            .map(|dep| SnapshotNodeDep {
                name: dep.name.clone(),
                pkg: dep.pkg.clone(),
            })
            .collect(),
    }
}

fn effective_minimum_minutes(config: &Config, crate_name: &str) -> u64 {
    let mut minutes = config.cooldown_minutes;
    if let Some(global) = config.allow_rules.global_minutes() {
        minutes = minutes.min(global);
    }
    if let Some(&crate_minutes) = config.allow_rules.per_crate_minutes().get(crate_name) {
        minutes = minutes.min(crate_minutes);
    }
    minutes
}

fn constraint_contributions(
    node: &SnapshotNode,
    package: &SnapshotPackage,
    snapshot: &CargoSnapshot,
) -> Vec<ConstraintContribution> {
    let mut contributions: HashMap<(PackageId, String), ConstraintContribution> = HashMap::new();

    for dep in &node.deps {
        let Some(dep_pkg) = snapshot.package(&dep.pkg) else {
            continue;
        };
        let Some(source_id) = dep_pkg.source_id.as_ref() else {
            continue;
        };
        if !is_registry_source(source_id) {
            continue;
        }
        let Some(manifest_dep) = find_manifest_dependency(
            &package.dependencies,
            dep.name.as_str(),
            dep_pkg.name.as_str(),
        ) else {
            continue;
        };
        let requirement = manifest_dep.requirement.clone();
        let entry = contributions
            .entry((dep.pkg.clone(), requirement.clone()))
            .or_insert_with(|| ConstraintContribution {
                child_id: dep.pkg.clone(),
                parent_id: node.id.clone(),
                parent_name: package.name.clone(),
                requirement: requirement.clone(),
                exact_count: 0,
            });
        if is_exact_requirement(&manifest_dep.requirement_req()) {
            entry.exact_count += 1;
        }
    }

    contributions.into_values().collect()
}

pub fn inspect_current_release(
    registry_store: &mut RegistryStore,
    inspection_cache: &mut HashMap<ReleaseInspectionKey, ReleaseInspection>,
    context: &RegistryContext,
    state: &CrateState,
    now: DateTime<Utc>,
) -> Result<(ReleaseInspection, bool)> {
    let key = ReleaseInspectionKey {
        source_id: state.source_id.clone(),
        crate_name: state.name.clone(),
        current_version: state.current_version.clone(),
        minimum_minutes: state.minimum_minutes,
    };
    if let Some(cached) = inspection_cache.get(&key) {
        return Ok((cached.clone(), true));
    }

    let timeline = registry_store.timeline_for(&state.source_id, &state.name)?;
    ensure_timeline_available(context, &state.name, &timeline)?;
    let current_release = require_release(&timeline, context, &state.name, &state.current_version)?;
    let published_at = assert_has_timestamp(context, &state.name, current_release)?;
    let inspection = ReleaseInspection {
        published_at,
        release_time_source: current_release.source,
        fresh: is_release_fresh(current_release, state.minimum_minutes, now) == Some(true),
    };
    inspection_cache.insert(key, inspection.clone());
    Ok((inspection, false))
}

fn is_exact_requirement(req: &VersionReq) -> bool {
    if req.comparators.len() != 1 {
        return false;
    }
    matches!(req.comparators[0].op, Op::Exact)
}

fn find_manifest_dependency<'a>(
    deps: &'a [ManifestDependencyRecord],
    dep_name: &str,
    package_name: &str,
) -> Option<&'a ManifestDependencyRecord> {
    deps.iter().find(|candidate| {
        candidate
            .rename
            .as_deref()
            .is_some_and(|rename| rename == dep_name)
            || candidate.name == dep_name
            || candidate.name == package_name
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allow_rules::AllowRules;
    use crate::config::{Config, LockfilePolicy, Mode};
    use serde_json::json;

    fn config_fixture() -> Config {
        Config {
            cooldown_minutes: 60,
            mode: Mode::Strict,
            lockfile_policy: LockfilePolicy::All,
            now_override: None,
            ttl_seconds: 60,
            cache_dir: None,
            http_retries: 0,
            verbose: false,
            skip_registries: Vec::new(),
            allow_rules: AllowRules::default(),
        }
    }

    fn reachable_snapshot() -> CargoSnapshot {
        let metadata: Metadata = serde_json::from_value(json!({
            "packages": [
                {
                    "name": "app",
                    "version": "0.1.0",
                    "id": "path+file:///tmp/ws#app@0.1.0",
                    "source": null,
                    "dependencies": [
                        {
                            "name": "dep",
                            "source": "registry+https://github.com/rust-lang/crates.io-index",
                            "req": "^1",
                            "kind": null,
                            "rename": null,
                            "optional": false,
                            "uses_default_features": true,
                            "features": [],
                            "target": null,
                            "registry": null
                        }
                    ],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/tmp/ws/Cargo.toml",
                    "metadata": null,
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "readme": null,
                    "repository": null,
                    "homepage": null,
                    "documentation": null,
                    "edition": "2024",
                    "license": null,
                    "license_file": null,
                    "description": null,
                    "publish": null,
                    "links": null,
                    "default_run": null,
                    "rust_version": null
                },
                {
                    "name": "dep",
                    "version": "1.0.0",
                    "id": "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "dependencies": [],
                    "targets": [],
                    "features": {},
                    "manifest_path": "/tmp/dep/Cargo.toml",
                    "metadata": null,
                    "authors": [],
                    "categories": [],
                    "keywords": [],
                    "readme": null,
                    "repository": null,
                    "homepage": null,
                    "documentation": null,
                    "edition": "2024",
                    "license": null,
                    "license_file": null,
                    "description": null,
                    "publish": null,
                    "links": null,
                    "default_run": null,
                    "rust_version": null
                }
            ],
            "workspace_members": ["path+file:///tmp/ws#app@0.1.0"],
            "workspace_default_members": ["path+file:///tmp/ws#app@0.1.0"],
            "workspace_root": "/tmp/ws",
            "target_directory": "/tmp/ws/target",
            "resolve": {
                "nodes": [
                    {
                        "id": "path+file:///tmp/ws#app@0.1.0",
                        "dependencies": [
                            "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0"
                        ],
                        "deps": [
                            {
                                "name": "dep",
                                "pkg": "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0",
                                "dep_kinds": [{ "kind": null, "target": null }]
                            }
                        ],
                        "features": []
                    },
                    {
                        "id": "registry+https://github.com/rust-lang/crates.io-index#dep@1.0.0",
                        "dependencies": [],
                        "deps": [],
                        "features": []
                    }
                ],
                "root": "path+file:///tmp/ws#app@0.1.0"
            },
            "version": 1
        }))
        .expect("metadata should deserialize");

        CargoSnapshot::from_metadata(metadata, &Workspace::default())
            .expect("snapshot should be constructible")
    }

    #[test]
    fn cargo_snapshot_tracks_reachable_registry_packages() {
        let snapshot = reachable_snapshot();
        assert_eq!(snapshot.reachable_order.len(), 2);
        assert!(snapshot.package(&snapshot.reachable_order[1]).is_some());
    }

    #[test]
    fn requirement_origin_exposes_version_req() {
        let origin = RequirementOrigin {
            parent_id: serde_json::from_value(json!("path+file:///tmp/ws#app@0.1.0")).unwrap(),
            parent_name: "app".to_string(),
            requirement: "^1".to_string(),
        };
        assert_eq!(origin.requirement_req(), VersionReq::parse("^1").unwrap());
    }

    #[test]
    fn config_fixture_remains_constructible_for_resolution_state_tests() {
        let config = config_fixture();
        assert_eq!(config.cooldown_minutes, 60);
    }
}
