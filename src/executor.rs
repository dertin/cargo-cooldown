use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use cargo_metadata::PackageId;
use chrono::{DateTime, Utc};
use semver::{Op, VersionReq};
use tracing::{debug, info};

use crate::config::Config;
use crate::lockfile::LockfileSnapshot;
use crate::metadata::read_metadata;
use crate::registry::{
    RegistryContext, RegistryStore, ReleaseSource, assert_has_timestamp, ensure_timeline_available,
    is_registry_source, require_release,
};
use crate::resolver::{
    PinOutcome, cutoff_time, is_release_fresh, select_candidate, try_pin_precise,
};
use clap_cargo::{Features, Manifest, Workspace};

pub fn run_pinning_flow(
    config: &Config,
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
) -> Result<()> {
    let initial_lockfile = capture_initial_lockfile(config, manifest)?;
    run_pinning_flow_with_snapshot(
        config,
        manifest,
        workspace,
        features,
        initial_lockfile,
        "dependency graph cooled down; continuing with Cargo command",
    )
}

pub fn capture_initial_lockfile(config: &Config, manifest: &Manifest) -> Result<LockfileSnapshot> {
    let mut registry_store = RegistryStore::new(config)?;
    let lockfile_path = workspace_lockfile_path(manifest)?;
    // Capture the user-visible starting lockfile before any Cargo command is allowed
    // to generate or rewrite it during this cooldown run.
    LockfileSnapshot::capture(&lockfile_path, &mut registry_store)
}

pub fn run_pinning_flow_with_snapshot(
    config: &Config,
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
    initial_lockfile: LockfileSnapshot,
    success_message: &str,
) -> Result<()> {
    let allowlist = &config.allowlist;
    let per_crate_minutes = allowlist.per_crate_minutes();
    let global_minutes = allowlist.global_minutes();
    let mut registry_store = RegistryStore::new(config)?;
    let lockfile_path = workspace_lockfile_path(manifest)?;
    let result = (|| {
        // Missing lockfiles are created only after the initial snapshot exists, so the
        // default policy always compares against the pre-run lockfile state.
        ensure_lockfile(manifest, &lockfile_path)?;
        let now = config.now_override.unwrap_or_else(Utc::now);
        let mut visited_failures: HashSet<String> = HashSet::new();
        let mut inspection_cache: HashMap<ReleaseInspectionKey, ReleaseInspection> = HashMap::new();

        'outer: loop {
            // After each successful pin we rebuild cargo metadata from scratch so every
            // decision in this pass reflects the current lockfile and resolved graph.
            let metadata = read_metadata(manifest, features)?;
            let resolve = metadata
                .resolve
                .clone()
                .context("cargo metadata output did not include a resolved dependency graph")?;
            // When the user targets a package/workspace subset, only enforce cooldown on
            // the dependency closure reachable from those selected roots.
            let selected_root_ids = selected_package_ids(&metadata, workspace);
            let reachable_ids = reachable_package_ids(&resolve, &selected_root_ids);
            let packages: HashMap<PackageId, cargo_metadata::Package> = metadata
                .packages
                .into_iter()
                .map(|pkg| (pkg.id.clone(), pkg))
                .collect();

            let mut name_version_to_ids: HashMap<(String, String), Vec<PackageId>> = HashMap::new();
            for (id, pkg) in &packages {
                if !reachable_ids.contains(id) {
                    continue;
                }
                name_version_to_ids
                    .entry((pkg.name.to_string(), pkg.version.to_string()))
                    .or_default()
                    .push(id.clone());
            }

            // Build the per-package state we need to reason about cooldowns in one pass:
            // current version, effective minimum age, semver requirements from parents,
            // and which locked packages are currently too fresh.
            let mut crate_states: HashMap<PackageId, CrateState> = HashMap::new();
            let mut fresh_entries: Vec<FreshCrate> = Vec::new();
            let mut equality_dependents: HashMap<PackageId, Vec<PackageId>> = HashMap::new();
            let mut requirement_origins: HashMap<PackageId, Vec<RequirementOrigin>> =
                HashMap::new();
            let mut version_requirements: HashMap<PackageId, Vec<VersionReq>> = HashMap::new();
            let mut seen: HashSet<PackageId> = HashSet::new();
            let mut scan_summary = ScanSummary::default();

            for node in &resolve.nodes {
                if !reachable_ids.contains(&node.id) || !seen.insert(node.id.clone()) {
                    continue;
                }
                let Some(pkg) = packages.get(&node.id) else {
                    continue;
                };

                record_dependency_requirements(
                    node,
                    pkg,
                    &packages,
                    &mut version_requirements,
                    &mut requirement_origins,
                    &mut equality_dependents,
                );

                let Some(source) = pkg.source.as_ref() else {
                    continue;
                };
                if !is_registry_source(&source.repr) {
                    continue;
                }

                scan_summary.registry_packages += 1;
                let context = registry_store.context_for_source(&source.repr)?.clone();
                let current_version = pkg.version.to_string();
                let mut minimum_minutes = config.cooldown_minutes;
                if let Some(global) = global_minutes {
                    minimum_minutes = minimum_minutes.min(global);
                }
                if let Some(&minutes) = per_crate_minutes.get(pkg.name.as_str()) {
                    minimum_minutes = minimum_minutes.min(minutes);
                }

                let exact_allowed = allowlist.is_exact_allowed(pkg.name.as_str(), &current_version);
                let baseline_exempt = !config.lockfile_policy.applies_to_existing_lockfile()
                    && initial_lockfile.baseline().contains_registry_version(
                        pkg.name.as_str(),
                        &context.effective_index_url,
                        &current_version,
                    );
                let state = CrateState {
                    name: pkg.name.to_string(),
                    source_id: source.repr.clone(),
                    current_version: current_version.clone(),
                    minimum_minutes,
                    exact_allowed,
                    skipped: context.skipped,
                    baseline_exempt,
                };
                crate_states.insert(node.id.clone(), state.clone());
                scan_summary.observe(&state);

                if state.is_cooldown_exempt() {
                    continue;
                }

                scan_summary.inspected += 1;
                let (inspection, cache_hit) = inspect_current_release(
                    &mut registry_store,
                    &mut inspection_cache,
                    &context,
                    &state,
                    now,
                )?;
                let cutoff = cutoff_time(minimum_minutes, now);
                debug!(
                    crate = %pkg.name,
                    version = %current_version,
                    published_at = %inspection.published_at,
                    release_time_source = inspection.release_time_source.log_label(),
                    cutoff = %cutoff,
                    cache = if cache_hit { "hit" } else { "miss" },
                    registry = %context.effective_index_url,
                    "evaluated release age for locked dependency"
                );
                if config.verbose {
                    eprintln!(
                        "cooldown: {} crate={} version={} registry={} published_at={} cutoff={} release_time_source={} cache={}",
                        if cache_hit { "reused" } else { "inspected" },
                        pkg.name,
                        current_version,
                        context.effective_index_url,
                        inspection.published_at,
                        cutoff,
                        inspection.release_time_source.log_label(),
                        if cache_hit { "hit" } else { "miss" },
                    );
                }

                if inspection.fresh {
                    scan_summary.fresh += 1;
                    fresh_entries.push(FreshCrate {
                        package_id: node.id.clone(),
                        name: pkg.name.to_string(),
                        source_id: source.repr.clone(),
                        current_version,
                        minimum_minutes,
                    });
                }
            }

            if config.verbose {
                eprintln!(
                    "cooldown: scan_summary registry_packages={} inspected={} fresh={} baseline_exempt={} skipped_registries={} exact_allowed={} zero_minutes={}",
                    scan_summary.registry_packages,
                    scan_summary.inspected,
                    scan_summary.fresh,
                    scan_summary.baseline_exempt,
                    scan_summary.skipped,
                    scan_summary.exact_allowed,
                    scan_summary.zero_minutes,
                );
            }

            if fresh_entries.is_empty() {
                info!("{}", success_message);
                break;
            }

            // Try the fresh crates that are most likely to unblock others first.
            // Exact-version dependents are the hardest constraints, so they go earlier.
            let fresh_ids: HashSet<PackageId> = fresh_entries
                .iter()
                .map(|entry| entry.package_id.clone())
                .collect();
            fresh_entries.sort_by_key(|entry| {
                equality_dependents
                    .get(&entry.package_id)
                    .map_or(0, |dependents| {
                        dependents
                            .iter()
                            .filter(|id| fresh_ids.contains(*id))
                            .count()
                    })
            });

            let mut queue: VecDeque<FreshCrate> = fresh_entries.into();

            'queue_loop: while let Some(fresh) = queue.pop_front() {
                // Each queue entry represents one currently locked crate/version pair that
                // still violates the cooldown window and needs an older acceptable version.
                let key = format!(
                    "{}::{}@{}",
                    fresh.source_id, fresh.name, fresh.current_version
                );
                if visited_failures.contains(&key) {
                    bail!(
                        "no acceptable version found for {} from registry {} (cooldown {} minutes). Consider waiting for the cooldown window, relaxing the requirement, or skipping that registry via COOLDOWN_SKIP_REGISTRIES.",
                        fresh.name,
                        fresh.source_id,
                        fresh.minimum_minutes
                    );
                }

                let context = registry_store.context_for_source(&fresh.source_id)?.clone();
                let timeline = registry_store.timeline_for(&fresh.source_id, &fresh.name)?;
                ensure_timeline_available(&context, &fresh.name, &timeline)?;
                let requirements = version_requirements
                    .get(&fresh.package_id)
                    .cloned()
                    .unwrap_or_default();

                let Some(candidate) = select_candidate(
                    &timeline,
                    &fresh.current_version,
                    &requirements,
                    fresh.minimum_minutes,
                    now,
                ) else {
                    // No older compatible version exists for this crate as-is. Requeue any
                    // parent that constrained it so we can try to cool down the parent first
                    // and potentially relax the version chosen for this dependency.
                    let mut queued_parent = false;
                    if let Some(origins) = requirement_origins.get(&fresh.package_id) {
                        for origin in origins {
                            if let Some(state) = crate_states.get(&origin.parent_id) {
                                if state.is_cooldown_exempt() {
                                    continue;
                                }
                                queue.push_front(FreshCrate {
                                    package_id: origin.parent_id.clone(),
                                    name: origin.parent_name.clone(),
                                    source_id: state.source_id.clone(),
                                    current_version: state.current_version.clone(),
                                    minimum_minutes: state.minimum_minutes,
                                });
                                queued_parent = true;
                            }
                        }
                    }
                    if queued_parent {
                        queue.push_back(fresh.clone());
                        continue 'queue_loop;
                    }

                    visited_failures.insert(key);
                    bail!(
                        "crate {} from registry {} lacks versions older than {} minutes that satisfy the semver constraints",
                        fresh.name,
                        context.effective_index_url,
                        fresh.minimum_minutes
                    );
                };

                info!(
                    crate = %fresh.name,
                    registry = %context.effective_index_url,
                    current = %fresh.current_version,
                    candidate = %candidate.version,
                    "attempting pin"
                );

                match try_pin_precise(
                    manifest,
                    &fresh.name,
                    &fresh.current_version,
                    &candidate.version,
                )? {
                    PinOutcome::Applied => {
                        // A successful pin changes the lockfile, so restart from the top and
                        // recompute metadata instead of trying to patch our in-memory graph.
                        info!(
                            crate = %fresh.name,
                            registry = %context.effective_index_url,
                            pinned = %candidate.version,
                            "pin applied"
                        );
                        continue 'outer;
                    }
                    PinOutcome::Rejected { stdout, stderr } => {
                        let blockers = parse_blockers(&stdout, &stderr);
                        if blockers.is_empty() {
                            visited_failures.insert(key);
                            bail!(
                                "cargo rejected pinning {} from registry {} to {} without exposing actionable blockers",
                                fresh.name,
                                context.effective_index_url,
                                candidate.version
                            );
                        }

                        // Cargo can reject a pin because some other locked package still
                        // requires the fresher version. Requeue those blockers first, then
                        // revisit this crate later in the same pass.
                        let blocker_descriptions =
                            blockers.iter().map(Blocker::label).collect::<Vec<_>>();
                        let mut queued_blocker = false;
                        for blocker in blockers {
                            let matches = blocker
                                .version
                                .as_ref()
                                .and_then(|version| {
                                    name_version_to_ids
                                        .get(&(blocker.name.clone(), version.clone()))
                                        .cloned()
                                })
                                .or_else(|| {
                                    Some(
                                        crate_states
                                            .iter()
                                            .filter(|(_, state)| state.name == blocker.name)
                                            .map(|(id, _)| id.clone())
                                            .collect(),
                                    )
                                })
                                .unwrap_or_default();

                            for id in matches {
                                if let Some(state) = crate_states.get(&id) {
                                    if state.is_cooldown_exempt() {
                                        continue;
                                    }
                                    queue.push_front(FreshCrate {
                                        package_id: id,
                                        name: state.name.clone(),
                                        source_id: state.source_id.clone(),
                                        current_version: state.current_version.clone(),
                                        minimum_minutes: state.minimum_minutes,
                                    });
                                    queued_blocker = true;
                                }
                            }
                        }

                        if !queued_blocker {
                            visited_failures.insert(key);
                            bail!(
                                "cargo rejected pinning {} from registry {} to {} due to blockers outside the selected cooldown scope or otherwise ineligible blockers: {}",
                                fresh.name,
                                context.effective_index_url,
                                candidate.version,
                                blocker_descriptions.join(", ")
                            );
                        }

                        queue.push_back(fresh.clone());
                    }
                }
            }

            bail!(
                "reached a fixed point without resolving all fresh dependencies; aborting to avoid endless loop"
            );
        }

        Ok(())
    })();

    if let Err(err) = result {
        if let Err(restore_err) = initial_lockfile.restore(&lockfile_path) {
            return Err(restore_err.context(format!("original cooldown error: {err:#}")));
        }
        return Err(err);
    }

    Ok(())
}

#[derive(Debug, Default)]
struct ScanSummary {
    registry_packages: usize,
    inspected: usize,
    fresh: usize,
    baseline_exempt: usize,
    skipped: usize,
    exact_allowed: usize,
    zero_minutes: usize,
}

impl ScanSummary {
    fn observe(&mut self, state: &CrateState) {
        self.baseline_exempt += usize::from(state.baseline_exempt);
        self.skipped += usize::from(state.skipped);
        self.exact_allowed += usize::from(state.exact_allowed);
        self.zero_minutes += usize::from(state.minimum_minutes == 0);
    }
}

fn selected_package_ids(
    metadata: &cargo_metadata::Metadata,
    workspace: &Workspace,
) -> HashSet<PackageId> {
    workspace
        .partition_packages(metadata)
        .0
        .into_iter()
        .map(|package| package.id.clone())
        .collect()
}

fn reachable_package_ids(
    resolve: &cargo_metadata::Resolve,
    selected_root_ids: &HashSet<PackageId>,
) -> HashSet<PackageId> {
    let nodes_by_id: HashMap<PackageId, &cargo_metadata::Node> = resolve
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

fn ensure_lockfile(manifest: &Manifest, lockfile_path: &Path) -> Result<()> {
    if lockfile_path.exists() {
        return Ok(());
    }

    let mut command = Command::new("cargo");
    command.arg("generate-lockfile");
    if let Some(path) = &manifest.manifest_path {
        command.arg("--manifest-path").arg(path);
    }

    let status = command.status()?;
    if !status.success() {
        bail!("failed to generate Cargo.lock via `cargo generate-lockfile`");
    }
    Ok(())
}

fn workspace_lockfile_path(manifest: &Manifest) -> Result<PathBuf> {
    // Workspace members share the root Cargo.lock, so we ask Cargo for the
    // effective workspace manifest instead of guessing from --manifest-path.
    let mut command = Command::new("cargo");
    command.args(["locate-project", "--workspace", "--message-format", "plain"]);
    if let Some(path) = &manifest.manifest_path {
        command.arg("--manifest-path").arg(path);
    }

    let output = command
        .output()
        .context("failed to run `cargo locate-project --workspace`")?;
    if !output.status.success() {
        bail!(
            "failed to locate workspace manifest via `cargo locate-project --workspace`: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let manifest_path = String::from_utf8(output.stdout)
        .context("`cargo locate-project --workspace` returned non-utf8 output")?;
    let manifest_path = manifest_path.trim();
    let workspace_manifest = PathBuf::from(manifest_path);
    let workspace_root = workspace_manifest.parent().with_context(|| {
        format!(
            "`cargo locate-project --workspace` returned a manifest without a parent directory: {}",
            workspace_manifest.display()
        )
    })?;
    Ok(workspace_root.join("Cargo.lock"))
}

#[derive(Clone, Debug)]
struct FreshCrate {
    package_id: PackageId,
    name: String,
    source_id: String,
    current_version: String,
    minimum_minutes: u64,
}

#[derive(Clone)]
struct CrateState {
    name: String,
    source_id: String,
    current_version: String,
    minimum_minutes: u64,
    exact_allowed: bool,
    skipped: bool,
    baseline_exempt: bool,
}

impl CrateState {
    fn is_cooldown_exempt(&self) -> bool {
        self.exact_allowed || self.minimum_minutes == 0 || self.skipped || self.baseline_exempt
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReleaseInspectionKey {
    source_id: String,
    crate_name: String,
    current_version: String,
    minimum_minutes: u64,
}

#[derive(Clone, Debug)]
struct ReleaseInspection {
    published_at: DateTime<Utc>,
    release_time_source: ReleaseSource,
    fresh: bool,
}

#[derive(Clone, Debug)]
struct RequirementOrigin {
    parent_id: PackageId,
    parent_name: String,
    requirement: VersionReq,
}

fn inspect_current_release(
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

    // A single cooldown run should reason over one stable timeline snapshot
    // instead of re-reading the same release metadata after each successful pin.
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

fn is_exact_requirement(req: &semver::VersionReq) -> bool {
    if req.comparators.len() != 1 {
        return false;
    }
    matches!(req.comparators[0].op, Op::Exact)
}

fn find_manifest_dependency<'a>(
    deps: &'a [cargo_metadata::Dependency],
    dep_name: &str,
    package_name: &str,
) -> Option<&'a cargo_metadata::Dependency> {
    deps.iter().find(|candidate| {
        candidate
            .rename
            .as_deref()
            .is_some_and(|rename| rename == dep_name)
            || candidate.name == dep_name
            || candidate.name == package_name
    })
}

fn record_dependency_requirements(
    node: &cargo_metadata::Node,
    pkg: &cargo_metadata::Package,
    packages: &HashMap<PackageId, cargo_metadata::Package>,
    version_requirements: &mut HashMap<PackageId, Vec<VersionReq>>,
    requirement_origins: &mut HashMap<PackageId, Vec<RequirementOrigin>>,
    equality_dependents: &mut HashMap<PackageId, Vec<PackageId>>,
) {
    for dep in &node.deps {
        let Some(dep_pkg) = packages.get(&dep.pkg) else {
            continue;
        };
        let Some(source) = dep_pkg.source.as_ref() else {
            continue;
        };
        if !is_registry_source(&source.repr) {
            continue;
        }

        if let Some(manifest_dep) =
            find_manifest_dependency(&pkg.dependencies, &dep.name, &dep_pkg.name)
        {
            let requirements = version_requirements.entry(dep.pkg.clone()).or_default();
            if !requirements.iter().any(|req| req == &manifest_dep.req) {
                requirements.push(manifest_dep.req.clone());
            }

            let origins = requirement_origins.entry(dep.pkg.clone()).or_default();
            if !origins
                .iter()
                .any(|origin| origin.parent_id == node.id && origin.requirement == manifest_dep.req)
            {
                origins.push(RequirementOrigin {
                    parent_id: node.id.clone(),
                    parent_name: pkg.name.to_string(),
                    requirement: manifest_dep.req.clone(),
                });
            }

            if is_exact_requirement(&manifest_dep.req) {
                equality_dependents
                    .entry(dep.pkg.clone())
                    .or_default()
                    .push(node.id.clone());
            }
        }
    }
}

fn parse_blockers(stdout: &str, stderr: &str) -> Vec<Blocker> {
    let mut blockers = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("required by package `")
            && let Some(end) = rest.find('`')
        {
            let inner = &rest[..end];
            if let Some((name, version)) = inner.rsplit_once(' ') {
                let version = version.trim_start_matches('v').to_string();
                if !blockers.iter().any(|existing: &Blocker| {
                    existing.name == name && existing.version.as_deref() == Some(&version)
                }) {
                    blockers.push(Blocker {
                        name: name.to_string(),
                        version: Some(version),
                    });
                }
            } else if !blockers
                .iter()
                .any(|existing: &Blocker| existing.name == inner)
            {
                blockers.push(Blocker {
                    name: inner.to_string(),
                    version: None,
                });
            }
        }
    }
    blockers
}

#[derive(Debug)]
struct Blocker {
    name: String,
    version: Option<String>,
}

impl Blocker {
    fn label(&self) -> String {
        self.version.as_ref().map_or_else(
            || self.name.clone(),
            |version| format!("{} {}", self.name, version),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;
    use crate::config::{Config, LockfilePolicy, Mode};
    use serde_json::json;

    fn dependency_with(rename: Option<&str>, req: &str) -> cargo_metadata::Dependency {
        serde_json::from_value(json!({
            "name": "sha2",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "req": req,
            "kind": null,
            "rename": rename,
            "optional": false,
            "uses_default_features": true,
            "features": [],
            "target": null,
            "registry": null
        }))
        .expect("dependency should deserialize")
    }

    #[test]
    fn is_exact_requirement_only_accepts_single_exact_comparator() {
        assert!(is_exact_requirement(&VersionReq::parse("=1.2.3").unwrap()));
        assert!(!is_exact_requirement(&VersionReq::parse("^1.2.3").unwrap()));
        assert!(!is_exact_requirement(
            &VersionReq::parse(">=1.2.3, <2.0.0").unwrap()
        ));
    }

    #[test]
    fn find_manifest_dependency_matches_renamed_dependency() {
        let deps = vec![dependency_with(Some("digest-sha2"), "^0.10")];
        let matched = find_manifest_dependency(&deps, "digest-sha2", "sha2")
            .expect("renamed dependency should match");
        assert_eq!(matched.req, VersionReq::parse("^0.10").unwrap());
    }

    #[test]
    fn local_workspace_members_constrain_registry_candidates() {
        let local_pkg: cargo_metadata::Package = serde_json::from_value(json!({
            "name": "workspace-member-app",
            "version": "0.1.0",
            "id": "path+file:///tmp/workspace-member/app#workspace-member-app@0.1.0",
            "license": null,
            "license_file": null,
            "description": null,
            "source": null,
            "dependencies": [
                {
                    "name": "sha2",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "req": "^0.11",
                    "kind": null,
                    "rename": null,
                    "optional": false,
                    "uses_default_features": true,
                    "features": [],
                    "target": null,
                    "registry": null
                }
            ],
            "targets": [
                {
                    "kind": ["bin"],
                    "crate_types": ["bin"],
                    "name": "workspace-member-app",
                    "src_path": "/tmp/workspace-member/app/src/main.rs",
                    "edition": "2021",
                    "doc": true,
                    "doctest": false,
                    "test": true
                }
            ],
            "features": {},
            "manifest_path": "/tmp/workspace-member/app/Cargo.toml",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2021",
            "links": null,
            "default_run": null,
            "rust_version": null
        }))
        .expect("local package should deserialize");
        let registry_pkg: cargo_metadata::Package = serde_json::from_value(json!({
            "name": "sha2",
            "version": "0.11.0",
            "id": "registry+https://github.com/rust-lang/crates.io-index#sha2@0.11.0",
            "license": "MIT OR Apache-2.0",
            "license_file": null,
            "description": "sha2 test package",
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "dependencies": [],
            "targets": [
                {
                    "kind": ["lib"],
                    "crate_types": ["lib"],
                    "name": "sha2",
                    "src_path": "/tmp/cargo-home/sha2/src/lib.rs",
                    "edition": "2024",
                    "doc": true,
                    "doctest": true,
                    "test": true
                }
            ],
            "features": {},
            "manifest_path": "/tmp/cargo-home/sha2/Cargo.toml",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }))
        .expect("registry package should deserialize");
        let local_node: cargo_metadata::Node = serde_json::from_value(json!({
            "id": "path+file:///tmp/workspace-member/app#workspace-member-app@0.1.0",
            "dependencies": [
                "registry+https://github.com/rust-lang/crates.io-index#sha2@0.11.0"
            ],
            "deps": [
                {
                    "name": "sha2",
                    "pkg": "registry+https://github.com/rust-lang/crates.io-index#sha2@0.11.0",
                    "dep_kinds": [
                        {
                            "kind": null,
                            "target": null
                        }
                    ]
                }
            ],
            "features": []
        }))
        .expect("local node should deserialize");

        let local_id = local_pkg.id.clone();
        let registry_id = registry_pkg.id.clone();
        let packages = HashMap::from([
            (local_id.clone(), local_pkg),
            (registry_id.clone(), registry_pkg),
        ]);
        let mut version_requirements = HashMap::new();
        let mut requirement_origins = HashMap::new();
        let mut equality_dependents = HashMap::new();

        record_dependency_requirements(
            &local_node,
            packages.get(&local_id).expect("local package exists"),
            &packages,
            &mut version_requirements,
            &mut requirement_origins,
            &mut equality_dependents,
        );

        let requirements = version_requirements
            .get(&registry_id)
            .expect("local workspace member should constrain registry dependency");
        assert_eq!(requirements.len(), 1);
        assert_eq!(requirements[0], VersionReq::parse("^0.11").unwrap());

        let origins = requirement_origins
            .get(&registry_id)
            .expect("requirement origin should be tracked");
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].parent_id, local_id);
        assert_eq!(origins[0].parent_name, "workspace-member-app");

        assert!(
            !equality_dependents.contains_key(&registry_id),
            "caret requirements must not be treated as exact blockers"
        );
    }

    #[test]
    fn record_dependency_requirements_deduplicates_exact_requirements() {
        let parent_pkg: cargo_metadata::Package = serde_json::from_value(json!({
            "name": "demo-app",
            "version": "0.1.0",
            "id": "path+file:///tmp/demo#demo-app@0.1.0",
            "license": null,
            "license_file": null,
            "description": null,
            "source": null,
            "dependencies": [
                {
                    "name": "sha2",
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "req": "=1.0.0",
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
            "manifest_path": "/tmp/demo/Cargo.toml",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }))
        .unwrap();
        let registry_pkg: cargo_metadata::Package = serde_json::from_value(json!({
            "name": "sha2",
            "version": "1.0.0",
            "id": "registry+https://github.com/rust-lang/crates.io-index#sha2@1.0.0",
            "license": null,
            "license_file": null,
            "description": null,
            "source": "registry+https://github.com/rust-lang/crates.io-index",
            "dependencies": [],
            "targets": [],
            "features": {},
            "manifest_path": "/tmp/sha2/Cargo.toml",
            "metadata": null,
            "publish": null,
            "authors": [],
            "categories": [],
            "keywords": [],
            "readme": null,
            "repository": null,
            "homepage": null,
            "documentation": null,
            "edition": "2024",
            "links": null,
            "default_run": null,
            "rust_version": null
        }))
        .unwrap();
        let node: cargo_metadata::Node = serde_json::from_value(json!({
            "id": "path+file:///tmp/demo#demo-app@0.1.0",
            "dependencies": [
                "registry+https://github.com/rust-lang/crates.io-index#sha2@1.0.0"
            ],
            "deps": [
                {
                    "name": "sha2",
                    "pkg": "registry+https://github.com/rust-lang/crates.io-index#sha2@1.0.0",
                    "dep_kinds": [{ "kind": null, "target": null }]
                },
                {
                    "name": "sha2",
                    "pkg": "registry+https://github.com/rust-lang/crates.io-index#sha2@1.0.0",
                    "dep_kinds": [{ "kind": null, "target": null }]
                }
            ],
            "features": []
        }))
        .unwrap();

        let parent_id = parent_pkg.id.clone();
        let registry_id = registry_pkg.id.clone();
        let packages = HashMap::from([
            (parent_id.clone(), parent_pkg),
            (registry_id.clone(), registry_pkg),
        ]);
        let mut version_requirements = HashMap::new();
        let mut requirement_origins = HashMap::new();
        let mut equality_dependents = HashMap::new();

        record_dependency_requirements(
            &node,
            packages.get(&parent_id).unwrap(),
            &packages,
            &mut version_requirements,
            &mut requirement_origins,
            &mut equality_dependents,
        );

        assert_eq!(version_requirements.get(&registry_id).unwrap().len(), 1);
        assert_eq!(requirement_origins.get(&registry_id).unwrap().len(), 1);
        assert_eq!(equality_dependents.get(&registry_id).unwrap().len(), 2);
    }

    #[test]
    fn reachable_package_ids_stay_within_selected_workspace_member_closure() {
        let resolve: cargo_metadata::Resolve = serde_json::from_value(json!({
            "nodes": [
                {
                    "id": "path+file:///tmp/ws#targeted@0.1.0",
                    "dependencies": [
                        "registry+https://github.com/rust-lang/crates.io-index#targetdep@1.0.1"
                    ],
                    "deps": [
                        {
                            "name": "targetdep",
                            "pkg": "registry+https://github.com/rust-lang/crates.io-index#targetdep@1.0.1",
                            "dep_kinds": [{ "kind": null, "target": null }]
                        }
                    ],
                    "features": []
                },
                {
                    "id": "path+file:///tmp/ws#unrelated@0.1.0",
                    "dependencies": [
                        "registry+https://github.com/rust-lang/crates.io-index#otherdep@1.0.1"
                    ],
                    "deps": [
                        {
                            "name": "otherdep",
                            "pkg": "registry+https://github.com/rust-lang/crates.io-index#otherdep@1.0.1",
                            "dep_kinds": [{ "kind": null, "target": null }]
                        }
                    ],
                    "features": []
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#targetdep@1.0.1",
                    "dependencies": [],
                    "deps": [],
                    "features": []
                },
                {
                    "id": "registry+https://github.com/rust-lang/crates.io-index#otherdep@1.0.1",
                    "dependencies": [],
                    "deps": [],
                    "features": []
                }
            ],
            "root": null
        }))
        .expect("resolve graph should deserialize");
        let targeted_id: PackageId =
            serde_json::from_value(json!("path+file:///tmp/ws#targeted@0.1.0")).unwrap();
        let unrelated_id: PackageId =
            serde_json::from_value(json!("path+file:///tmp/ws#unrelated@0.1.0")).unwrap();
        let targetdep_id: PackageId = serde_json::from_value(json!(
            "registry+https://github.com/rust-lang/crates.io-index#targetdep@1.0.1"
        ))
        .unwrap();
        let otherdep_id: PackageId = serde_json::from_value(json!(
            "registry+https://github.com/rust-lang/crates.io-index#otherdep@1.0.1"
        ))
        .unwrap();
        let selected = HashSet::from([targeted_id.clone()]);

        let reachable = reachable_package_ids(&resolve, &selected);

        assert!(reachable.contains(&targeted_id));
        assert!(reachable.contains(&targetdep_id));
        assert!(!reachable.contains(&unrelated_id));
        assert!(!reachable.contains(&otherdep_id));
    }

    #[test]
    fn parse_blockers_extracts_unique_packages() {
        let blockers = parse_blockers(
            "",
            "required by package `foo 1.2.3`\nrequired by package `foo 1.2.3`\nrequired by package `bar`",
        );
        assert_eq!(blockers.len(), 2);
        assert_eq!(blockers[0].name, "foo");
        assert_eq!(blockers[0].version.as_deref(), Some("1.2.3"));
        assert_eq!(blockers[1].name, "bar");
        assert!(blockers[1].version.is_none());
    }

    #[test]
    fn baseline_exempt_state_stays_out_of_cooldown() {
        let unchanged = CrateState {
            name: "demo".to_string(),
            source_id: "registry+https://github.com/rust-lang/crates.io-index".to_string(),
            current_version: "1.0.0".to_string(),
            minimum_minutes: 60,
            exact_allowed: false,
            skipped: false,
            baseline_exempt: true,
        };
        let changed = CrateState {
            baseline_exempt: false,
            ..unchanged.clone()
        };

        assert!(unchanged.is_cooldown_exempt());
        assert!(!changed.is_cooldown_exempt());
    }

    #[test]
    fn config_fixture_remains_constructible_for_executor_tests() {
        let config = Config {
            cooldown_minutes: 60,
            mode: Mode::Enforce,
            lockfile_policy: LockfilePolicy::Changed,
            now_override: None,
            ttl_seconds: 60,
            cache_dir: None,
            http_retries: 0,
            verbose: false,
            skip_registries: Vec::new(),
            allowlist: Allowlist::default(),
        };

        assert_eq!(config.cooldown_minutes, 60);
    }
}
