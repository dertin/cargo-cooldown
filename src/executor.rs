use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cargo_metadata::PackageId;
use chrono::Utc;
use semver::{Op, Version, VersionReq};
use tracing::{debug, info, warn};

use crate::allowlist::Allowlist;
use crate::cache::Cache;
use crate::config::Config;
use crate::metadata::read_metadata;
use crate::registry::{RegistryClient, VersionMeta};
use crate::resolver::{PinOutcome, filter_candidates, try_pin_precise};

pub async fn run_pinning_flow(config: &Config) -> Result<()> {
    ensure_lockfile()?;

    let allowlist = Allowlist::load(config.allowlist_path.clone())?;
    let per_crate_minutes = allowlist.per_crate_minutes();
    let global_minutes = allowlist.global_minutes();
    let cache = if let Some(ref root) = config.cache_dir {
        Cache::with_root(root.clone(), Duration::from_secs(config.ttl_seconds))?
    } else {
        Cache::new(config.ttl_seconds)?
    };
    let client = RegistryClient::new(config)?;

    let mut visited_failures: HashSet<String> = HashSet::new();

    'outer: loop {
        let metadata = read_metadata()?;
        let resolve = metadata
            .resolve
            .clone()
            .context("cargo metadata output did not include a resolved dependency graph")?;
        let packages: HashMap<PackageId, cargo_metadata::Package> = metadata
            .packages
            .into_iter()
            .map(|pkg| (pkg.id.clone(), pkg))
            .collect();

        let mut name_version_to_id: HashMap<(String, String), PackageId> = HashMap::new();
        for (id, pkg) in &packages {
            name_version_to_id.insert((pkg.name.to_string(), pkg.version.to_string()), id.clone());
        }

        let now = Utc::now();
        let mut crate_states: HashMap<PackageId, CrateState> = HashMap::new();
        let mut fresh_entries: Vec<FreshCrate> = Vec::new();
        let mut equality_dependents: HashMap<PackageId, Vec<PackageId>> = HashMap::new();
        let mut requirement_origins: HashMap<PackageId, Vec<RequirementOrigin>> = HashMap::new();
        let mut version_requirements: HashMap<PackageId, Vec<VersionReq>> = HashMap::new();
        let mut seen: HashSet<PackageId> = HashSet::new();

        for node in &resolve.nodes {
            if !seen.insert(node.id.clone()) {
                continue;
            }
            let Some(pkg) = packages.get(&node.id) else {
                continue;
            };
            let Some(source) = pkg.source.as_ref() else {
                continue;
            };
            if !config.is_registry_allowed(&source.repr) {
                debug!(crate = %pkg.name, source = %source.repr, "skipping non-crates.io registry dependency");
                continue;
            }

            let current_version = pkg.version.to_string();
            let mut minimum_minutes = config.cooldown_minutes;
            if let Some(global) = global_minutes {
                minimum_minutes = minimum_minutes.min(global);
            }
            if let Some(&minutes) = per_crate_minutes.get(pkg.name.as_str()) {
                minimum_minutes = minimum_minutes.min(minutes);
            }

            let exact_allowed = allowlist.is_exact_allowed(pkg.name.as_str(), &current_version);
            crate_states.insert(
                node.id.clone(),
                CrateState {
                    name: pkg.name.to_string(),
                    current_version: current_version.clone(),
                    minimum_minutes,
                    exact_allowed,
                },
            );

            for dep in &node.deps {
                let Some(dep_pkg) = packages.get(&dep.pkg) else {
                    continue;
                };
                if !dep_pkg
                    .source
                    .as_ref()
                    .map(|src| config.is_registry_allowed(&src.repr))
                    .unwrap_or(false)
                {
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
                    if !origins.iter().any(|origin| {
                        origin.parent_id == node.id && origin.requirement == manifest_dep.req
                    }) {
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

            if exact_allowed || minimum_minutes == 0 {
                continue;
            }

            match fetch_version_meta(&client, &cache, pkg.name.as_str(), &current_version).await {
                Ok(meta) => {
                    let age_minutes = (now - meta.created_at).num_minutes();
                    debug!(
                        crate = %pkg.name,
                        %age_minutes,
                        %minimum_minutes,
                        created_at = %meta.created_at,
                        "crate age inspected"
                    );
                    if age_minutes < minimum_minutes as i64 {
                        fresh_entries.push(FreshCrate {
                            package_id: node.id.clone(),
                            name: pkg.name.to_string(),
                            current_version: current_version.clone(),
                            minimum_minutes,
                        });
                    }
                }
                Err(err) => {
                    if config.offline_ok {
                        warn!(crate = %pkg.name, error = %err, "skipping metadata fetch due to offline mode");
                    } else {
                        return Err(err);
                    }
                }
            }
        }

        if fresh_entries.is_empty() {
            info!("dependency graph cooled down; continuing with Cargo command");
            break;
        }

        let fresh_ids: HashSet<PackageId> =
            fresh_entries.iter().map(|f| f.package_id.clone()).collect();
        fresh_entries.sort_by_key(|entry| {
            equality_dependents
                .get(&entry.package_id)
                .map(|dependents| {
                    dependents
                        .iter()
                        .filter(|id| fresh_ids.contains(*id))
                        .count()
                })
                .unwrap_or(0)
        });

        let mut queue: VecDeque<FreshCrate> = fresh_entries.into();

        'queue_loop: while let Some(fresh) = queue.pop_front() {
            let key = format!("{}@{}", fresh.name, fresh.current_version);
            if visited_failures.contains(&key) {
                bail!(
                    "no acceptable version found for {} (cooldown {} minutes). Consider waiting for the cooldown window, temporarily downgrading, or applying a [patch.crates-io] override.",
                    fresh.name,
                    fresh.minimum_minutes
                );
            }

            let candidate_list = match fetch_version_list(&client, &cache, &fresh.name).await {
                Ok(list) => list,
                Err(err) => {
                    if config.offline_ok {
                        warn!(crate = %fresh.name, error = %err, "skipping candidate discovery due to offline mode");
                        queue.push_back(fresh);
                        continue;
                    } else {
                        return Err(err);
                    }
                }
            };

            let mut candidates = filter_candidates(candidate_list, fresh.minimum_minutes, now);
            let requirements = version_requirements
                .get(&fresh.package_id)
                .cloned()
                .unwrap_or_default();
            if !requirements.is_empty() {
                candidates
                    .retain(|candidate| satisfies_requirements(&candidate.version, &requirements));
            }

            if let Ok(current_semver) = Version::parse(&fresh.current_version) {
                candidates.retain(|candidate| {
                    Version::parse(&candidate.version)
                        .map(|version| version < current_semver)
                        .unwrap_or(true)
                });
            }

            if candidates.is_empty() {
                debug!(crate = %fresh.name, requirements = ?requirements, "no candidates satisfied semver requirements after cooldown filter");
                let mut queued_parent = false;
                if let Some(origins) = requirement_origins.get(&fresh.package_id) {
                    debug!(crate = %fresh.name, parents = ?origins, "enqueuing parents due to unsatisfied requirements");
                    for origin in origins {
                        if let Some(state) = crate_states.get(&origin.parent_id) {
                            if state.exact_allowed || state.minimum_minutes == 0 {
                                continue;
                            }
                            queue.push_front(FreshCrate {
                                package_id: origin.parent_id.clone(),
                                name: origin.parent_name.clone(),
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

                visited_failures.insert(key.clone());
                bail!(
                    "crate {} lacks versions older than {} minutes that satisfy the semver constraint. Options: wait for the cooldown to elapse, relax the dependency requirement, or pin explicitly via [patch.crates-io].",
                    fresh.name,
                    fresh.minimum_minutes
                );
            }

            for candidate in candidates {
                if candidate.version == fresh.current_version {
                    continue;
                }
                info!(crate = %fresh.name, current = %fresh.current_version, candidate = %candidate.version, "attempting pin");
                match try_pin_precise(&fresh.name, &fresh.current_version, &candidate.version) {
                    Ok(PinOutcome::Applied) => {
                        info!(crate = %fresh.name, pinned = %candidate.version, "pin applied");
                        continue 'outer;
                    }
                    Ok(PinOutcome::Rejected { stdout, stderr }) => {
                        let blockers = parse_blockers(&stdout, &stderr);
                        if blockers.is_empty() {
                            debug!(crate = %fresh.name, candidate = %candidate.version, "cargo update rejected candidate");
                            continue;
                        }
                        for blocker in blockers {
                            let blocker_id = blocker
                                .version
                                .as_ref()
                                .and_then(|ver| {
                                    name_version_to_id.get(&(blocker.name.clone(), ver.clone()))
                                })
                                .cloned()
                                .or_else(|| {
                                    crate_states
                                        .iter()
                                        .find(|(_, state)| state.name == blocker.name)
                                        .map(|(id, _)| id.clone())
                                });

                            if let Some(id) = blocker_id
                                && let Some(state) = crate_states.get(&id) {
                                    if state.exact_allowed || state.minimum_minutes == 0 {
                                        debug!(crate = %state.name, "blocking crate is exempt from cooldown; skipping downgrade");
                                        continue;
                                    }
                                    queue.push_front(FreshCrate {
                                        package_id: id,
                                        name: state.name.clone(),
                                        current_version: state.current_version.clone(),
                                        minimum_minutes: state.minimum_minutes,
                                    });
                                }
                        }
                        queue.push_back(fresh.clone());
                        continue 'queue_loop;
                    }
                    Err(err) => {
                        if config.offline_ok {
                            warn!(crate = %fresh.name, candidate = %candidate.version, error = %err, "pin attempt failed in offline mode");
                            queue.push_back(fresh.clone());
                            continue 'queue_loop;
                        } else {
                            return Err(err);
                        }
                    }
                }
            }

            visited_failures.insert(key.clone());
            bail!(
                "unable to pin crate {} to an older compatible release within the cooldown window ({} minutes). Try waiting or adding a manual override.",
                fresh.name,
                fresh.minimum_minutes
            );
        }

        bail!(
            "reached a fixed point without resolving all fresh dependencies; aborting to avoid endless loop"
        );
    }

    Ok(())
}

fn ensure_lockfile() -> Result<()> {
    if Path::new("Cargo.lock").exists() {
        return Ok(());
    }
    let status = Command::new("cargo").args(["generate-lockfile"]).status()?;
    if !status.success() {
        bail!("failed to generate Cargo.lock via `cargo generate-lockfile`");
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct FreshCrate {
    package_id: PackageId,
    name: String,
    current_version: String,
    minimum_minutes: u64,
}

struct CrateState {
    name: String,
    current_version: String,
    minimum_minutes: u64,
    exact_allowed: bool,
}

#[derive(Clone, Debug)]
struct RequirementOrigin {
    parent_id: PackageId,
    parent_name: String,
    requirement: VersionReq,
}

async fn fetch_version_meta(
    client: &RegistryClient,
    cache: &Cache,
    name: &str,
    version: &str,
) -> Result<VersionMeta> {
    let key = format!("{name}/{version}");
    if let Some(meta) = cache.get::<VersionMeta>(&key)? {
        return Ok(meta);
    }
    let meta = client.fetch_version(name, version).await?;
    cache.put(&key, &meta)?;
    Ok(meta)
}

async fn fetch_version_list(
    client: &RegistryClient,
    cache: &Cache,
    name: &str,
) -> Result<Vec<VersionMeta>> {
    let key = format!("{name}/_list");
    if let Some(list) = cache.get::<Vec<VersionMeta>>(&key)? {
        return Ok(list);
    }
    let list = client.list_versions(name).await?;
    cache.put(&key, &list)?;
    Ok(list)
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
            .map(|rename| rename == dep_name)
            .unwrap_or(false)
            || candidate.name == dep_name
            || candidate.name == package_name
    })
}

fn parse_blockers(stdout: &str, stderr: &str) -> Vec<Blocker> {
    let mut blockers = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("required by package `")
            && let Some(end) = rest.find('`') {
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

fn satisfies_requirements(version: &str, requirements: &[VersionReq]) -> bool {
    if requirements.is_empty() {
        return true;
    }
    match Version::parse(version) {
        Ok(parsed) => requirements.iter().all(|req| req.matches(&parsed)),
        Err(_) => false,
    }
}
