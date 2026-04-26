//! Cooldown execution loop, lockfile pinning, and Cargo validation.
//!
//! This module receives the already parsed Cargo command context and turns the
//! current lockfile into a cooled, Cargo-valid lockfile. It repeatedly reads
//! Cargo metadata, identifies registry packages that are too fresh, selects older
//! compatible releases, rewrites the lockfile in bounded batches, and asks Cargo
//! to validate every proposed assignment before keeping it.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use cargo_metadata::{Metadata, PackageId};
use chrono::{DateTime, SecondsFormat, Utc};
use semver::{Comparator, Op, Version, VersionReq};
use tracing::{debug, trace};

use crate::config::{CargoCompatibleAccept, Config, Enforcement};
use crate::lockfile::LockfileSnapshot;
use crate::metadata::{read_metadata, read_metadata_locked};
use crate::registry::{RegistryStore, ensure_timeline_available};
use crate::resolution_state::{
    CargoSnapshot, CrateState, FreshCrate, ReleaseInspection, ReleaseInspectionKey,
    RequirementOrigin, build_resolution_state, crate_failure_key, inspect_current_release,
};
use crate::resolver::{select_candidate, select_candidates};
use crate::ui::{StatusKind, UserOutput, format_status_line};
use clap_cargo::{Features, Manifest, Workspace};

const MAX_COORDINATED_COMPONENT_SIZE: usize = 8;
const MAX_COORDINATED_CANDIDATES: usize = 4;
const MAX_COORDINATED_ASSIGNMENTS: usize = 64;
const COOLDOWN_BATCH_VALIDATION_ATTEMPT_CEILING: usize = 12;
const COOLDOWN_LOCAL_SEED_CANDIDATES: usize = 8;
const COOLDOWN_LOCAL_REQUIRED_CANDIDATES_PER_REQUIREMENT: usize = 2;
const COOLDOWN_LOCAL_ASSIGNMENT_CEILING: usize = 250_000;

/// Log the cost of expensive resolver phases without changing their return type.
macro_rules! timed_debug {
    ($message:literal, $block:block) => {{
        let started = Instant::now();
        let result = $block;
        debug!(
            target: "cargo_cooldown::timing",
            elapsed_ms = started.elapsed().as_millis(),
            concat!("cooldown timing: ", $message)
        );
        result
    }};
}

/// Capture the user-visible `Cargo.lock` before cooldown mutates anything.
///
/// The snapshot stores the file contents and an index of registry package
/// versions. Later phases use it to restore the exact starting state on failure
/// and, under the default `floor` baseline, to avoid downgrading versions that
/// were already locked before this command started.
pub fn capture_initial_lockfile(config: &Config, manifest: &Manifest) -> Result<LockfileSnapshot> {
    let mut registry_store = RegistryStore::new(config)?;
    let lockfile_path = workspace_lockfile_path(manifest)?;
    // Capture the user-visible starting lockfile before any Cargo command is allowed
    // to generate or rewrite it during this cooldown run.
    LockfileSnapshot::capture(&lockfile_path, &mut registry_store)
}

/// Restore a lockfile snapshot into the active workspace.
pub fn restore_lockfile_snapshot(snapshot: &LockfileSnapshot, manifest: &Manifest) -> Result<()> {
    snapshot.restore(&workspace_lockfile_path(manifest)?)
}

/// Run the full cooldown resolver from an already captured baseline.
///
/// This is the main execution loop. It receives the immutable command context,
/// the initial lockfile snapshot, and the success message to show at the end.
/// Each pass reads Cargo metadata, marks fresh registry packages, tries a batch
/// lockfile assignment, validates that assignment with Cargo, and repeats if the
/// graph changed. It returns `Ok(())` after emitting the final user summary, or
/// restores the initial lockfile before returning an error.
pub fn run_pinning_flow_with_snapshot(
    config: &Config,
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
    initial_lockfile: LockfileSnapshot,
    success_message: &str,
) -> Result<()> {
    let mut registry_store = RegistryStore::new(config)?;
    let lockfile_path = workspace_lockfile_path(manifest)?;
    let mut ui = UserOutput::new(config.verbose);
    let result = (|| {
        // Missing lockfiles are created only after the initial snapshot exists, so the
        // default baseline always compares against the pre-run lockfile state.
        ui.set_phase("Preparing cooldown scan...");
        ensure_lockfile(manifest, &lockfile_path)?;
        let now = config.now_override.unwrap_or_else(Utc::now);
        let mut cargo_compatible_skips: HashMap<String, String> = HashMap::new();
        let mut constraint_edges: HashMap<String, HashSet<String>> = HashMap::new();
        let mut inspection_cache: HashMap<ReleaseInspectionKey, ReleaseInspection> = HashMap::new();
        ui.set_phase("Capturing cooldown baseline...");
        let cooldown_start_lockfile =
            LockfileSnapshot::capture(&lockfile_path, &mut registry_store)?;
        let mut pass = 0usize;
        let mut next_metadata = None;

        // Each successful pin can change Cargo's resolved graph, so the loop always
        // restarts from metadata after Cargo accepts a new lockfile assignment.
        'outer: loop {
            pass += 1;
            let metadata = if let Some(metadata) = next_metadata.take() {
                debug!(
                    target: "cargo_cooldown::timing",
                    elapsed_ms = 0,
                    "cooldown timing: reuse validated cargo metadata"
                );
                metadata
            } else {
                if pass == 1 {
                    ui.set_phase("Reading Cargo metadata...");
                }
                timed_debug!("cargo metadata", { read_metadata(manifest, features) })?
            };
            if pass == 1 {
                ui.set_phase("Scanning dependency graph...");
            }
            let snapshot = timed_debug!("snapshot from metadata", {
                CargoSnapshot::from_metadata(metadata, workspace)
            })?;
            if pass == 1 {
                ui.set_phase("Inspecting release ages...");
            }
            let state = timed_debug!("build resolution state", {
                build_resolution_state(
                    &snapshot,
                    config,
                    &initial_lockfile,
                    &mut registry_store,
                    &mut inspection_cache,
                    &cargo_compatible_skips,
                    now,
                )
            })?;

            // Keep cargo-compatible decisions only for crate versions that are still fresh in the
            // current lockfile. If a successful pin changes a crate version, the next pass
            // should reconsider the new version instead of inheriting stale skip state.
            cargo_compatible_skips.retain(|key, _| state.fresh_keys_present.contains(key));
            refresh_constraint_edges(
                &mut constraint_edges,
                &state.crate_states,
                &state.requirement_origins,
                &state.fresh_keys_present,
            );

            debug!(
                "cooldown: scan_summary registry_packages={} inspected={} fresh={} baseline_exempt={} cargo_compatible_skipped={} skipped_registries={} exact_allowed={} zero_minutes={}",
                state.scan_summary.registry_packages,
                state.scan_summary.inspected,
                state.scan_summary.fresh,
                state.scan_summary.baseline_exempt,
                state.scan_summary.cargo_compatible_skipped,
                state.scan_summary.skipped,
                state.scan_summary.exact_allowed,
                state.scan_summary.zero_minutes,
            );
            ui.update_resolver_progress(
                pass,
                state.scan_summary.registry_packages,
                state.scan_summary.inspected,
                state.scan_summary.fresh + state.scan_summary.cargo_compatible_skipped,
            );

            let mut fresh_entries = state.fresh_entries_vec();
            let cargo_compatible_fresh_entries = state.cargo_compatible_entries_vec();
            let crate_states = &state.crate_states;
            let equality_dependents = &state.equality_dependents;
            let requirement_origins = &state.requirement_origins;
            let version_requirements = &state.version_requirements;

            if fresh_entries.is_empty() {
                // Single-package pins cannot handle every exact-version bundle.
                // Try one small coordinated pass before deciding whether enforcement should fail.
                if !cargo_compatible_fresh_entries.is_empty()
                    && attempt_coordinated_bundle_resolution(
                        &CoordinatedResolutionCtx {
                            manifest,
                            workspace,
                            features,
                            config,
                            lockfile_path: &lockfile_path,
                            initial_lockfile: &initial_lockfile,
                            requirement_origins,
                            now,
                        },
                        &mut registry_store,
                        &cargo_compatible_fresh_entries,
                        &constraint_edges,
                    )?
                {
                    continue 'outer;
                }
                ui.finish_progress();
                let final_lockfile =
                    LockfileSnapshot::capture(&lockfile_path, &mut registry_store)?;
                emit_final_run_summary(&mut FinalRunSummaryCtx {
                    ui: &ui,
                    config,
                    initial_lockfile: &initial_lockfile,
                    cooldown_start_lockfile: &cooldown_start_lockfile,
                    final_lockfile: &final_lockfile,
                    crate_states,
                    registry_store: &mut registry_store,
                    inspection_cache: &mut inspection_cache,
                    cargo_compatible_skips: &cargo_compatible_skips,
                    now,
                    success_message,
                })?;
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

            if let Some(metadata) = attempt_cooldown_batch_solver(
                &CooldownBatchSolverCtx {
                    manifest,
                    workspace,
                    features,
                    config,
                    lockfile_path: &lockfile_path,
                    initial_lockfile: &initial_lockfile,
                    crate_states,
                    version_requirements,
                    requirement_origins,
                    now,
                },
                &mut registry_store,
                &fresh_entries,
            )? {
                next_metadata = Some(metadata);
                continue 'outer;
            }

            let mut recorded_batch_limit = false;
            for fresh in fresh_entries {
                let key = crate_failure_key(&fresh.source_id, &fresh.name, &fresh.current_version);
                let reason =
                    "the cooldown batch solver could not find a Cargo-valid older assignment"
                        .to_string();
                recorded_batch_limit |=
                    record_cargo_compatible_skip(&mut cargo_compatible_skips, &key, reason.clone());
                debug!(
                    crate = %fresh.name,
                    registry = %fresh.source_id,
                    current = %fresh.current_version,
                    reason,
                    "leaving fresh package unresolved after cooldown batch planning"
                );
            }

            if recorded_batch_limit {
                continue 'outer;
            }

            bail!(
                "cooldown batch solver reached a fixed point without resolving all fresh dependencies"
            );
        }

        Ok(())
    })();

    ui.finish_progress();

    if let Err(err) = result {
        if let Err(restore_err) = initial_lockfile.restore(&lockfile_path) {
            return Err(restore_err.context(format!("original cooldown error: {err:#}")));
        }
        return Err(err);
    }

    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct FreshVersionNotice {
    name: String,
    version: String,
    registry: String,
    published_at: DateTime<Utc>,
}

#[derive(Debug)]
pub struct CargoCompatibleFreshVersionsNotAccepted {
    message: String,
}

impl CargoCompatibleFreshVersionsNotAccepted {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CargoCompatibleFreshVersionsNotAccepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CargoCompatibleFreshVersionsNotAccepted {}

pub fn is_cargo_compatible_fresh_versions_not_accepted(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CargoCompatibleFreshVersionsNotAccepted>()
        .is_some()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct CooledVersionNotice {
    action: CooledVersionAction,
    name: String,
    from_version: Option<String>,
    to_version: Option<String>,
    latest_version: Option<String>,
    registry: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum CooledVersionAction {
    Adding,
    Cooling,
    Downgrading,
    Keeping,
    Removing,
    Updating,
}

impl CooledVersionAction {
    fn status_kind(&self) -> StatusKind {
        match self {
            Self::Adding => StatusKind::Adding,
            Self::Cooling | Self::Updating => StatusKind::Updating,
            Self::Downgrading => StatusKind::Downgrading,
            Self::Keeping => StatusKind::Keeping,
            Self::Removing => StatusKind::Removing,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct InventoryKey {
    name: String,
    registry_id: String,
    registry: String,
}

type VersionInventory = BTreeMap<InventoryKey, Vec<String>>;

#[derive(Default)]
struct FinalFreshReport {
    baseline_fresh: Vec<FreshVersionNotice>,
    resolver_constrained_fresh: Vec<FreshVersionNotice>,
}

struct FinalRunSummaryCtx<'a> {
    ui: &'a UserOutput,
    config: &'a Config,
    initial_lockfile: &'a LockfileSnapshot,
    cooldown_start_lockfile: &'a LockfileSnapshot,
    final_lockfile: &'a LockfileSnapshot,
    crate_states: &'a HashMap<PackageId, CrateState>,
    registry_store: &'a mut RegistryStore,
    inspection_cache: &'a mut HashMap<ReleaseInspectionKey, ReleaseInspection>,
    cargo_compatible_skips: &'a HashMap<String, String>,
    now: DateTime<Utc>,
    success_message: &'a str,
}

fn emit_final_run_summary(ctx: &mut FinalRunSummaryCtx<'_>) -> Result<()> {
    let baseline_inventory = collect_snapshot_inventory(ctx.initial_lockfile);
    let cooldown_start_inventory = collect_snapshot_inventory(ctx.cooldown_start_lockfile);
    let final_inventory = collect_snapshot_inventory(ctx.final_lockfile);
    let cooled_versions = collect_cooled_versions(
        &baseline_inventory,
        &cooldown_start_inventory,
        &final_inventory,
    );
    let mut report = FinalFreshReport::default();
    let mut baseline_seen = HashSet::new();
    let mut resolver_seen = HashSet::new();

    for state in ctx.crate_states.values() {
        let key = crate_failure_key(&state.source_id, &state.name, &state.current_version);
        let baseline_candidate = state.baseline_exempt;
        let resolver_candidate = ctx.cargo_compatible_skips.contains_key(&key);
        if !baseline_candidate && !resolver_candidate {
            continue;
        }

        let context = match ctx.registry_store.context_for_source(&state.source_id) {
            Ok(context) => context.clone(),
            Err(err) => {
                debug!(
                    crate = %state.name,
                    version = %state.current_version,
                    registry = %state.source_id,
                    error = %err,
                    "skipping final fresh-version classification because registry context could not be resolved"
                );
                continue;
            }
        };
        let (inspection, _) = match inspect_current_release(
            ctx.registry_store,
            ctx.inspection_cache,
            &context,
            state,
            ctx.now,
        ) {
            Ok(result) => result,
            Err(err) => {
                debug!(
                    crate = %state.name,
                    version = %state.current_version,
                    registry = %context.effective_index_url,
                    error = %err,
                    "skipping final fresh-version classification because release metadata could not be inspected"
                );
                continue;
            }
        };
        if !inspection.fresh {
            continue;
        }

        let notice = FreshVersionNotice {
            name: state.name.clone(),
            version: state.current_version.clone(),
            registry: context.logical_name,
            published_at: inspection.published_at,
        };

        if baseline_candidate && baseline_seen.insert(notice.clone()) {
            report.baseline_fresh.push(notice.clone());
        }
        if resolver_candidate && resolver_seen.insert(notice.clone()) {
            report.resolver_constrained_fresh.push(notice);
        }
    }

    report.baseline_fresh.sort();
    report.resolver_constrained_fresh.sort();

    enforce_final_report_policy(ctx.config.enforcement, &report)?;
    confirm_cargo_compatible_fresh_versions(ctx.config, &report, ctx.ui.use_color())?;

    emit_final_summary(ctx.ui, &cooled_versions, &report, ctx.success_message);
    Ok(())
}

fn enforce_final_report_policy(enforcement: Enforcement, report: &FinalFreshReport) -> Result<()> {
    if matches!(enforcement, Enforcement::Strict) && !report.resolver_constrained_fresh.is_empty() {
        bail!(
            "strict enforcement blocked fresh versions that could not be cooled further:\n{}\n\n\
             These versions are still inside the configured cooldown window after \
             cargo-cooldown tried older Cargo-valid lockfile assignments. Strict enforcement \
             restores the original Cargo.lock instead of keeping them.\n\n\
             To keep Cargo's resolved lockfile and report these as warnings, use \
             `COOLDOWN_ENFORCEMENT=cargo_compatible` or `enforcement = \"cargo_compatible\"`. To accept specific \
             fresh releases intentionally, add `allow.package` or `allow.exact` rules.",
            format_fresh_notice_list(&report.resolver_constrained_fresh).join("\n")
        );
    }

    Ok(())
}

fn confirm_cargo_compatible_fresh_versions(
    config: &Config,
    report: &FinalFreshReport,
    use_color: bool,
) -> Result<()> {
    if !matches!(config.enforcement, Enforcement::CargoCompatible)
        || report.resolver_constrained_fresh.is_empty()
        || matches!(config.cargo_compatible_accept, CargoCompatibleAccept::Auto)
    {
        return Ok(());
    }

    eprintln!(
        "{}",
        format_cargo_compatible_acceptance_prompt(report, use_color)
    );

    if !io::stdin().is_terminal() {
        return Err(anyhow::Error::new(
            CargoCompatibleFreshVersionsNotAccepted::new(
                "cargo_compatible enforcement requires confirmation for fresh versions that could not be cooled, but stdin is not interactive. Set `cargo_compatible_accept = \"auto\"` or `COOLDOWN_CARGO_COMPATIBLE_ACCEPT=auto` to accept them without prompting.",
            ),
        ));
    }

    eprint!("Accept these fresh versions and continue? [y/N] ");
    io::stderr().flush().map_err(|err| {
        anyhow::Error::new(CargoCompatibleFreshVersionsNotAccepted::new(format!(
            "failed to prompt for cargo-compatible fresh version acceptance: {err}"
        )))
    })?;

    let mut answer = String::new();
    io::stdin().read_line(&mut answer).map_err(|err| {
        anyhow::Error::new(CargoCompatibleFreshVersionsNotAccepted::new(format!(
            "failed to read cargo-compatible fresh version acceptance: {err}"
        )))
    })?;

    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(anyhow::Error::new(
            CargoCompatibleFreshVersionsNotAccepted::new(
                "fresh versions that could not be cooled were not accepted; restored the original Cargo.lock",
            ),
        )),
    }
}

fn format_cargo_compatible_acceptance_prompt(report: &FinalFreshReport, use_color: bool) -> String {
    let mut lines = vec![format_status_line(
        StatusKind::Warning,
        "Cargo requires fresh versions that cooldown could not replace.",
        use_color,
    )];
    lines.push(
        "These versions will remain in or be added to Cargo.lock; if they are not already cached, the next Cargo command may download them:"
            .to_string(),
    );
    lines.extend(format_fresh_notice_list(&report.resolver_constrained_fresh));
    lines.push(
        "Review before accepting: freshly published crates are the supply-chain risk cooldown is designed to reduce."
            .to_string(),
    );
    lines.join("\n")
}

fn emit_final_summary(
    ui: &UserOutput,
    cooled_versions: &[CooledVersionNotice],
    report: &FinalFreshReport,
    success_message: &str,
) {
    eprintln!(
        "{}",
        format_final_user_summary(cooled_versions, report, success_message, ui.use_color())
    );
}

fn format_final_user_summary(
    cooled_versions: &[CooledVersionNotice],
    report: &FinalFreshReport,
    success_message: &str,
    use_color: bool,
) -> String {
    let mut lines = Vec::new();

    if !cooled_versions.is_empty() {
        lines.extend(
            cooled_versions
                .iter()
                .map(|entry| format_cooled_version_notice(entry, use_color)),
        );
    }

    lines.extend(format_final_fresh_warning(report, use_color));
    lines.push(format_status_line(
        StatusKind::Finished,
        success_message,
        use_color,
    ));
    lines.join("\n")
}

fn format_final_fresh_warning(report: &FinalFreshReport, use_color: bool) -> Vec<String> {
    if report.resolver_constrained_fresh.is_empty() {
        return Vec::new();
    }

    let mut lines = vec![format_status_line(
        StatusKind::Warning,
        "cooldown finished with fresh versions remaining.",
        use_color,
    )];

    lines.push(
        "resolver-constrained versions that could not be cooled further (review these):"
            .to_string(),
    );
    lines.extend(format_fresh_notice_list(&report.resolver_constrained_fresh));

    lines
}

fn format_fresh_notice_list(entries: &[FreshVersionNotice]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{} {}{} (published: {})",
                entry.name,
                entry.version,
                registry_suffix(&entry.registry),
                format_published_at(entry.published_at)
            )
        })
        .map(|entry| format!("      - {entry}"))
        .collect()
}

fn format_published_at(published_at: DateTime<Utc>) -> String {
    published_at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn collect_snapshot_inventory(snapshot: &LockfileSnapshot) -> VersionInventory {
    let mut inventory = VersionInventory::new();

    for ((name, registry_id), mut versions) in snapshot.baseline().version_inventory() {
        versions.sort_by(compare_versions_desc);
        inventory.insert(
            InventoryKey {
                name,
                registry: default_registry_display_name(&registry_id),
                registry_id,
            },
            versions,
        );
    }

    inventory
}

fn collect_cooled_versions(
    baseline_inventory: &VersionInventory,
    start_inventory: &VersionInventory,
    end_inventory: &VersionInventory,
) -> Vec<CooledVersionNotice> {
    let mut notices = Vec::new();
    let mut keys = baseline_inventory
        .keys()
        .chain(start_inventory.keys())
        .chain(end_inventory.keys())
        .cloned()
        .collect::<Vec<_>>();
    keys.sort();
    keys.dedup();

    for key in keys {
        let baseline_versions = baseline_inventory.get(&key);
        let start_versions = start_inventory.get(&key);
        let end_versions = end_inventory.get(&key);

        if inventory_has_multiple_versions(baseline_versions)
            || inventory_has_multiple_versions(start_versions)
            || inventory_has_multiple_versions(end_versions)
        {
            notices.extend(collect_multi_version_notices(
                &key,
                baseline_versions,
                start_versions,
                end_versions,
            ));
            continue;
        }

        let baseline_version = singleton_inventory_version(baseline_versions);
        let start_version = singleton_inventory_version(start_versions);
        let end_version = singleton_inventory_version(end_versions);

        match (baseline_version, end_version) {
            (Some(from_version), Some(to_version)) => {
                let latest_annotation = start_version.filter(|version| version != &to_version);
                let action = classify_cooled_version_action(&from_version, &to_version);
                if action == CooledVersionAction::Keeping && latest_annotation.is_none() {
                    continue;
                }
                notices.push(CooledVersionNotice {
                    action,
                    name: key.name.clone(),
                    from_version: Some(from_version),
                    to_version: Some(to_version),
                    latest_version: latest_annotation,
                    registry: key.registry.clone(),
                });
            }
            (None, Some(to_version)) => notices.push(CooledVersionNotice {
                action: CooledVersionAction::Adding,
                name: key.name.clone(),
                from_version: None,
                to_version: Some(to_version.clone()),
                latest_version: start_version.filter(|version| version != &to_version),
                registry: key.registry.clone(),
            }),
            (Some(from_version), None) => notices.push(CooledVersionNotice {
                action: CooledVersionAction::Removing,
                name: key.name.clone(),
                from_version: Some(from_version),
                to_version: None,
                latest_version: None,
                registry: key.registry.clone(),
            }),
            (None, None) => {}
        }
    }

    notices.sort_by(compare_cooled_version_notice);
    notices
}

fn format_cooled_version_notice(entry: &CooledVersionNotice, use_color: bool) -> String {
    let mut line = match entry.action {
        CooledVersionAction::Adding => format!(
            "{} {}",
            entry.name,
            prefixed_version(
                entry
                    .to_version
                    .as_deref()
                    .expect("adding notice should include a target version"),
            )
        ),
        CooledVersionAction::Keeping => format!(
            "{} {}",
            entry.name,
            entry
                .to_version
                .as_deref()
                .expect("keeping notice should include a target version")
        ),
        CooledVersionAction::Removing => format!(
            "{} {}",
            entry.name,
            prefixed_version(
                entry
                    .from_version
                    .as_deref()
                    .expect("removing notice should include a source version"),
            )
        ),
        _ => format!(
            "{} {} -> {}",
            entry.name,
            prefixed_version(
                entry
                    .from_version
                    .as_deref()
                    .expect("changing notice should include a source version"),
            ),
            prefixed_version(
                entry
                    .to_version
                    .as_deref()
                    .expect("changing notice should include a target version"),
            )
        ),
    };

    if let Some(latest_version) = &entry.latest_version {
        let _ = write!(line, " (latest: {})", prefixed_version(latest_version));
    }

    line.push_str(&registry_suffix(&entry.registry));
    format_status_line(entry.action.status_kind(), &line, use_color)
}

fn collect_multi_version_notices(
    key: &InventoryKey,
    baseline_inventory: Option<&Vec<String>>,
    start_inventory: Option<&Vec<String>>,
    end_inventory: Option<&Vec<String>>,
) -> Vec<CooledVersionNotice> {
    let mut notices = Vec::new();

    let mut removed = inventory_difference(baseline_inventory, end_inventory);
    removed.sort_by(compare_versions_asc);
    let has_removed_versions = !removed.is_empty();
    notices.extend(removed.into_iter().map(|from_version| CooledVersionNotice {
        action: CooledVersionAction::Removing,
        name: key.name.clone(),
        from_version: Some(from_version),
        to_version: None,
        latest_version: None,
        registry: key.registry.clone(),
    }));

    let mut added = inventory_difference(end_inventory, baseline_inventory);
    added.sort_by(compare_versions_asc);
    notices.extend(added.into_iter().map(|to_version| CooledVersionNotice {
        action: CooledVersionAction::Adding,
        name: key.name.clone(),
        from_version: None,
        to_version: Some(to_version),
        latest_version: None,
        registry: key.registry.clone(),
    }));

    let mut latest_removed = inventory_difference(start_inventory, end_inventory);
    latest_removed.sort_by(compare_versions_asc);
    let mut latest_by_lane: BTreeMap<(u64, u64, u64), VecDeque<String>> = BTreeMap::new();
    for version in latest_removed {
        if let Some(lane) = parsed_version_lane(&version) {
            latest_by_lane.entry(lane).or_default().push_back(version);
        }
    }

    let mut preserved = inventory_preserved_counts(baseline_inventory, end_inventory);
    let mut preserved_versions = end_inventory
        .into_iter()
        .flatten()
        .filter_map(|version| take_preserved_version(&mut preserved, version))
        .collect::<Vec<_>>();
    preserved_versions.sort_by(compare_versions_asc);

    for to_version in preserved_versions {
        let Some(lane) = parsed_version_lane(&to_version) else {
            continue;
        };
        let latest_version = latest_by_lane
            .get_mut(&lane)
            .and_then(VecDeque::pop_front)
            .filter(|version| version != &to_version);
        if has_removed_versions || latest_version.is_some() {
            notices.push(CooledVersionNotice {
                action: CooledVersionAction::Keeping,
                name: key.name.clone(),
                from_version: Some(to_version.clone()),
                to_version: Some(to_version),
                latest_version,
                registry: key.registry.clone(),
            });
        }
    }

    notices
}

fn parsed_version_lane(version: &str) -> Option<(u64, u64, u64)> {
    let parsed = Version::parse(version).ok()?;
    Some(version_lane_key(&parsed))
}

fn singleton_inventory_version(inventory: Option<&Vec<String>>) -> Option<String> {
    let mut iter = inventory.into_iter().flatten();
    let version = iter.next()?.clone();
    if iter.next().is_some() {
        return None;
    }
    Some(version)
}

fn inventory_has_multiple_versions(inventory: Option<&Vec<String>>) -> bool {
    inventory.is_some_and(|versions| versions.len() > 1)
}

fn version_lane_key(version: &Version) -> (u64, u64, u64) {
    match (version.major, version.minor) {
        (major, _) if major > 0 => (major, 0, 0),
        (0, minor) if minor > 0 => (0, minor, 0),
        (0, 0) => (0, 0, version.patch),
        _ => (version.major, version.minor, version.patch),
    }
}

fn inventory_preserved_counts(
    left: Option<&Vec<String>>,
    right: Option<&Vec<String>>,
) -> HashMap<String, usize> {
    let mut left_counts = HashMap::new();
    for version in left.into_iter().flatten() {
        *left_counts.entry(version.clone()).or_insert(0) += 1;
    }

    let mut right_counts = HashMap::new();
    for version in right.into_iter().flatten() {
        *right_counts.entry(version.clone()).or_insert(0) += 1;
    }

    let mut preserved = HashMap::new();
    for (version, left_count) in left_counts {
        if let Some(right_count) = right_counts.get(&version) {
            let count = left_count.min(*right_count);
            if count > 0 {
                preserved.insert(version, count);
            }
        }
    }

    preserved
}

fn take_preserved_version(
    preserved_counts: &mut HashMap<String, usize>,
    version: &str,
) -> Option<String> {
    let remaining = preserved_counts.get_mut(version)?;
    if *remaining == 0 {
        return None;
    }
    *remaining -= 1;
    Some(version.to_string())
}

fn inventory_difference(primary: Option<&Vec<String>>, other: Option<&Vec<String>>) -> Vec<String> {
    let mut other_counts: HashMap<&str, usize> = HashMap::new();
    for version in other.into_iter().flatten() {
        *other_counts.entry(version.as_str()).or_default() += 1;
    }

    let mut difference = Vec::new();
    for version in primary.into_iter().flatten() {
        let remaining = other_counts.entry(version.as_str()).or_default();
        if *remaining > 0 {
            *remaining -= 1;
        } else {
            difference.push(version.clone());
        }
    }

    difference
}

fn classify_cooled_version_action(from_version: &str, to_version: &str) -> CooledVersionAction {
    if from_version == to_version {
        return CooledVersionAction::Keeping;
    }

    match (Version::parse(from_version), Version::parse(to_version)) {
        (Ok(from_version), Ok(to_version)) if to_version > from_version => {
            CooledVersionAction::Updating
        }
        (Ok(_), Ok(_)) => CooledVersionAction::Downgrading,
        _ => CooledVersionAction::Cooling,
    }
}

fn default_registry_display_name(registry_id: &str) -> String {
    if registry_id.contains("crates.io-index") || registry_id.contains("index.crates.io") {
        "crates-io".to_string()
    } else {
        registry_id.to_string()
    }
}

fn registry_suffix(registry: &str) -> String {
    if registry == "crates-io" {
        String::new()
    } else {
        format!(" @ {registry}")
    }
}

fn compare_versions_desc(left: &String, right: &String) -> std::cmp::Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => right.cmp(&left),
        _ => right.cmp(left),
    }
}

fn compare_versions_asc(left: &String, right: &String) -> std::cmp::Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

fn prefixed_version(version: &str) -> String {
    format!("v{version}")
}

fn compare_cooled_version_notice(
    left: &CooledVersionNotice,
    right: &CooledVersionNotice,
) -> std::cmp::Ordering {
    left.name
        .cmp(&right.name)
        .then_with(|| left.registry.cmp(&right.registry))
        .then_with(|| {
            cooled_version_action_rank(&left.action).cmp(&cooled_version_action_rank(&right.action))
        })
        .then_with(|| {
            let left_version = left
                .from_version
                .as_ref()
                .or(left.to_version.as_ref())
                .expect("summary notices should always contain a version");
            let right_version = right
                .from_version
                .as_ref()
                .or(right.to_version.as_ref())
                .expect("summary notices should always contain a version");
            compare_versions_asc(left_version, right_version)
        })
}

fn cooled_version_action_rank(action: &CooledVersionAction) -> u8 {
    match action {
        CooledVersionAction::Removing => 0,
        CooledVersionAction::Adding => 1,
        CooledVersionAction::Updating => 2,
        CooledVersionAction::Downgrading => 3,
        CooledVersionAction::Cooling => 4,
        CooledVersionAction::Keeping => 5,
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

fn record_cargo_compatible_skip(
    cargo_compatible_skips: &mut HashMap<String, String>,
    key: &str,
    reason: String,
) -> bool {
    if cargo_compatible_skips
        .get(key)
        .is_some_and(|existing| existing == &reason)
    {
        return false;
    }

    cargo_compatible_skips.insert(key.to_string(), reason);
    true
}

fn refresh_constraint_edges(
    constraint_edges: &mut HashMap<String, HashSet<String>>,
    crate_states: &HashMap<PackageId, CrateState>,
    requirement_origins: &HashMap<PackageId, Vec<RequirementOrigin>>,
    fresh_keys_present: &HashSet<String>,
) {
    constraint_edges.clear();

    // Coordinated bundle solving is intentionally narrow: only exact-version
    // relationships are grouped, which avoids turning broad semver constraints
    // into large expensive components.
    let fresh_key_by_id = crate_states
        .iter()
        .filter_map(|(package_id, state)| {
            let key = crate_failure_key(&state.source_id, &state.name, &state.current_version);
            fresh_keys_present
                .contains(&key)
                .then(|| (package_id.clone(), key))
        })
        .collect::<HashMap<_, _>>();

    for (child_id, origins) in requirement_origins {
        let Some(child_key) = fresh_key_by_id.get(child_id) else {
            continue;
        };

        for origin in origins {
            if !is_exact_requirement_text(&origin.requirement) {
                continue;
            }
            let Some(parent_key) = fresh_key_by_id.get(&origin.parent_id) else {
                continue;
            };
            if child_key == parent_key {
                continue;
            }
            constraint_edges
                .entry(child_key.clone())
                .or_default()
                .insert(parent_key.clone());
            constraint_edges
                .entry(parent_key.clone())
                .or_default()
                .insert(child_key.clone());
        }
    }
}

fn is_exact_requirement_text(requirement: &str) -> bool {
    VersionReq::parse(requirement).is_ok_and(|req| is_exact_requirement(&req))
}

fn is_exact_requirement(req: &VersionReq) -> bool {
    req.comparators.len() == 1 && matches!(req.comparators[0].op, Op::Exact)
}

#[derive(Clone, Debug)]
struct LockfilePin {
    root_names: BTreeSet<String>,
    name: String,
    source_id: String,
    current_version: String,
    target_version: String,
}

#[derive(Clone, Debug)]
struct LockedPackageInfo {
    package_id: PackageId,
    name: String,
    source_id: String,
    current_version: String,
    minimum_minutes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct LocalPackageKey {
    source_id: String,
    name: String,
    current_version: String,
}

impl LocalPackageKey {
    fn new(source_id: String, name: String, current_version: String) -> Self {
        Self {
            source_id,
            name,
            current_version,
        }
    }

    fn from_package(package: &LockedPackageInfo) -> Self {
        Self::new(
            package.source_id.clone(),
            package.name.clone(),
            package.current_version.clone(),
        )
    }

    fn source_name(&self) -> (String, String) {
        (self.source_id.clone(), self.name.clone())
    }
}

#[derive(Clone, Debug)]
struct LocalCandidateDependency {
    key: LocalPackageKey,
    requirement: VersionReq,
}

#[derive(Clone, Debug)]
struct LocalCandidate {
    version: String,
    parsed_version: Version,
    pinned: bool,
    dependencies: Vec<LocalCandidateDependency>,
}

#[derive(Clone, Debug)]
struct LocalSolverPlan {
    package: LockedPackageInfo,
    root_names: BTreeSet<String>,
    candidates: Vec<LocalCandidate>,
}

struct LocalDependencyResolver {
    package_keys_by_id: HashMap<PackageId, LocalPackageKey>,
    dependency_keys_by_parent_and_name: HashMap<(PackageId, String), Vec<LocalPackageKey>>,
    package_keys_by_source_and_name: HashMap<(String, String), Vec<LocalPackageKey>>,
}

impl LocalDependencyResolver {
    fn new(
        ctx: &CooldownBatchSolverCtx<'_>,
        locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
    ) -> Self {
        let package_keys_by_id = locked_packages
            .iter()
            .map(|(key, package)| (package.package_id.clone(), key.clone()))
            .collect::<HashMap<_, _>>();
        let mut package_keys_by_source_and_name: HashMap<(String, String), Vec<LocalPackageKey>> =
            HashMap::new();

        // Keep both PackageId and source/name indexes. PackageId is precise for
        // the current graph, while source/name is the fallback for index metadata
        // that does not carry Cargo's resolved IDs.
        for key in locked_packages.keys() {
            package_keys_by_source_and_name
                .entry(key.source_name())
                .or_default()
                .push(key.clone());
        }
        for keys in package_keys_by_source_and_name.values_mut() {
            keys.sort();
            keys.dedup();
        }

        let mut dependency_keys_by_parent_and_name: HashMap<
            (PackageId, String),
            Vec<LocalPackageKey>,
        > = HashMap::new();
        for (child_id, origins) in ctx.requirement_origins {
            let Some(child_key) = package_keys_by_id.get(child_id) else {
                continue;
            };
            for origin in origins {
                dependency_keys_by_parent_and_name
                    .entry((origin.parent_id.clone(), child_key.name.clone()))
                    .or_default()
                    .push(child_key.clone());
            }
        }
        for keys in dependency_keys_by_parent_and_name.values_mut() {
            keys.sort();
            keys.dedup();
        }

        Self {
            package_keys_by_id,
            dependency_keys_by_parent_and_name,
            package_keys_by_source_and_name,
        }
    }

    fn key_for_package_id(&self, package_id: &PackageId) -> Option<&LocalPackageKey> {
        self.package_keys_by_id.get(package_id)
    }

    fn has_active_dependency(&self, parent_id: &PackageId, crate_name: &str) -> bool {
        self.dependency_keys_by_parent_and_name
            .contains_key(&(parent_id.clone(), crate_name.to_string()))
    }

    fn resolve_dependency(
        &self,
        parent_id: &PackageId,
        source_id: &str,
        crate_name: &str,
        requirement: &VersionReq,
    ) -> Option<LocalPackageKey> {
        if let Some(graph_keys) = self
            .dependency_keys_by_parent_and_name
            .get(&(parent_id.clone(), crate_name.to_string()))
        {
            if let Some(key) = single_key_matching_current_version(graph_keys, requirement) {
                return Some(key);
            }
            if let Some(key) =
                self.single_source_name_key_matching(source_id, crate_name, requirement)
            {
                return Some(key);
            }
            if graph_keys.len() == 1 {
                return graph_keys.first().cloned();
            }
        }

        self.single_source_name_key_matching(source_id, crate_name, requirement)
            .or_else(|| {
                let keys = self
                    .package_keys_by_source_and_name
                    .get(&(source_id.to_string(), crate_name.to_string()))?;
                (keys.len() == 1).then(|| keys[0].clone())
            })
    }

    fn single_source_name_key_matching(
        &self,
        source_id: &str,
        crate_name: &str,
        requirement: &VersionReq,
    ) -> Option<LocalPackageKey> {
        let keys = self
            .package_keys_by_source_and_name
            .get(&(source_id.to_string(), crate_name.to_string()))?;
        single_key_matching_current_version(keys, requirement)
    }
}

fn single_key_matching_current_version(
    keys: &[LocalPackageKey],
    requirement: &VersionReq,
) -> Option<LocalPackageKey> {
    let mut matches = keys
        .iter()
        .filter(|key| requirement_matches_version(requirement, &key.current_version));
    let key = matches.next()?;
    matches.next().is_none().then(|| key.clone())
}

#[derive(Clone, Copy, Debug)]
struct LocalSearchBudget {
    assignment_visits: usize,
    estimated_space: usize,
}

/// Result of asking Cargo whether a rewritten lockfile assignment is valid.
enum BatchPinOutcome {
    Applied { metadata: Box<Metadata> },
    Rejected { error: String },
}

/// Read-only inputs shared by the batch solver.
///
/// The solver does not own CLI state or configuration. It borrows the same
/// manifest/workspace/features that Cargo will use, plus the current graph state
/// and initial lockfile baseline needed to choose safe candidate versions.
struct CooldownBatchSolverCtx<'a> {
    manifest: &'a Manifest,
    workspace: &'a Workspace,
    features: &'a Features,
    config: &'a Config,
    lockfile_path: &'a Path,
    initial_lockfile: &'a LockfileSnapshot,
    crate_states: &'a HashMap<PackageId, CrateState>,
    version_requirements: &'a HashMap<PackageId, Vec<VersionReq>>,
    requirement_origins: &'a HashMap<PackageId, Vec<RequirementOrigin>>,
    now: DateTime<Utc>,
}

/// Try to cool many fresh crates with one validated lockfile rewrite.
///
/// For each fresh crate this builds a candidate pin from registry metadata,
/// applies any baseline floor required by `lockfile_baseline = "floor"`, asks
/// the local dependency solver to add coupled transitive pins, and then validates
/// the whole batch with Cargo. It returns fresh `cargo metadata` when Cargo
/// accepted the assignment, or `None` when this pass should fall back to smaller
/// later strategies.
fn attempt_cooldown_batch_solver(
    ctx: &CooldownBatchSolverCtx<'_>,
    registry_store: &mut RegistryStore,
    fresh_entries: &[FreshCrate],
) -> Result<Option<Metadata>> {
    let pins = timed_debug!("cooldown batch candidate selection", {
        let mut pins = Vec::new();

        for fresh in fresh_entries {
            let context = registry_store.context_for_source(&fresh.source_id)?.clone();
            let timeline = registry_store.timeline_for(&fresh.source_id, &fresh.name)?;
            ensure_timeline_available(&context, &fresh.name, &timeline)?;
            let requirements = ctx
                .version_requirements
                .get(&fresh.package_id)
                .cloned()
                .unwrap_or_default();
            let mut requirements = requirements;
            if let Some(requirement) = baseline_floor_requirement(
                ctx.initial_lockfile,
                ctx.config,
                &context.effective_index_url,
                &fresh.name,
                &fresh.current_version,
            ) {
                requirements.push(requirement);
            }
            let Some(candidate) = select_candidate(
                &timeline,
                &fresh.current_version,
                &requirements,
                fresh.minimum_minutes,
                ctx.now,
                |version| {
                    baseline_allows_candidate(
                        ctx.initial_lockfile,
                        ctx.config,
                        &context.effective_index_url,
                        &fresh.name,
                        version,
                    )
                },
            ) else {
                continue;
            };

            if registry_store
                .local_release_checksum(&fresh.source_id, &fresh.name, &candidate.version)?
                .is_none()
            {
                debug!(
                    crate = %fresh.name,
                    version = %candidate.version,
                    registry = %context.effective_index_url,
                    "skipping bulk lockfile pin because the local index does not expose a checksum"
                );
                continue;
            }

            pins.push(LockfilePin {
                root_names: BTreeSet::from([fresh.name.clone()]),
                name: fresh.name.clone(),
                source_id: fresh.source_id.clone(),
                current_version: fresh.current_version.clone(),
                target_version: candidate.version.clone(),
            });
        }

        Ok::<_, anyhow::Error>(pins)
    })?;
    let pins = timed_debug!("cooldown batch local dependency solver", {
        solve_cooldown_batch_locally(ctx, registry_store, pins)
    })?;

    if pins.is_empty() {
        return Ok(None);
    }

    apply_cooldown_batch_with_blocker_pruning(ctx, registry_store, pins)
}

/// Validate a batch assignment and retry after removing Cargo-reported blockers.
///
/// The input pins are optimistic: they may contain crates whose constraints make
/// the batch impossible. Cargo's resolver diagnostics are used to remove those
/// blockers and retry while progress stays meaningful. The returned metadata is
/// the Cargo-accepted graph after a successful rewrite.
fn apply_cooldown_batch_with_blocker_pruning(
    ctx: &CooldownBatchSolverCtx<'_>,
    registry_store: &mut RegistryStore,
    mut pins: Vec<LockfilePin>,
) -> Result<Option<Metadata>> {
    let attempt_budget = cooldown_batch_validation_attempt_budget(pins.len());
    let mut seen_blocker_signatures = HashSet::new();
    for attempt in 1..=attempt_budget {
        if pins.is_empty() {
            return Ok(None);
        }

        debug!(
            target: "cargo_cooldown::timing",
            pins = pins.len(),
            attempt,
            "attempting cooldown batch solver lockfile assignment"
        );
        match apply_lockfile_pin_assignment_detailed(
            ctx.manifest,
            ctx.workspace,
            ctx.features,
            ctx.lockfile_path,
            registry_store,
            &pins,
        )? {
            BatchPinOutcome::Applied { metadata } => return Ok(Some(*metadata)),
            BatchPinOutcome::Rejected { error } => {
                let blockers = parse_batch_conflict_packages(&error);
                let blocker_names = blockers
                    .iter()
                    .map(|blocker| blocker.name.as_str())
                    .collect::<HashSet<_>>();
                if blocker_names.is_empty() {
                    return Ok(None);
                }
                // Cargo's resolver diagnostics identify packages that made the
                // batch impossible. Prune those pins and retry the rest as one
                // batch instead of falling back to one Cargo call per crate.
                let blocker_signature =
                    blockers.iter().map(Blocker::label).collect::<BTreeSet<_>>();
                if !seen_blocker_signatures.insert(blocker_signature) {
                    debug!(
                        target: "cargo_cooldown::timing",
                        attempt,
                        remaining = pins.len(),
                        "stopping cooldown batch validation because Cargo repeated the same blocker set"
                    );
                    return Ok(None);
                }

                let original_len = pins.len();
                pins.retain(|pin| !blocker_names.contains(pin.name.as_str()));
                let removed = original_len - pins.len();
                debug!(
                    target: "cargo_cooldown::timing",
                    attempt,
                    removed,
                    remaining = pins.len(),
                    blockers = %blockers.iter().map(Blocker::label).collect::<Vec<_>>().join(", "),
                    "pruned cooldown batch blockers"
                );
                if removed == 0 {
                    return Ok(None);
                }
                if batch_pruning_progress_is_too_low(original_len, removed, attempt_budget) {
                    debug!(
                        target: "cargo_cooldown::timing",
                        attempt,
                        removed,
                        original = original_len,
                        remaining = pins.len(),
                        "stopping cooldown batch validation because blocker pruning is too sparse for this batch size"
                    );
                    return Ok(None);
                }
            }
        }
    }

    debug!(
        target: "cargo_cooldown::timing",
        attempts = attempt_budget,
        remaining = pins.len(),
        "stopping cooldown batch validation after retry budget"
    );
    Ok(None)
}

fn cooldown_batch_validation_attempt_budget(pin_count: usize) -> usize {
    ceil_log2(pin_count.saturating_add(1)).clamp(2, COOLDOWN_BATCH_VALIDATION_ATTEMPT_CEILING)
}

fn ceil_log2(value: usize) -> usize {
    if value <= 1 {
        0
    } else {
        usize::BITS as usize - (value - 1).leading_zeros() as usize
    }
}

fn batch_pruning_progress_is_too_low(
    original_len: usize,
    removed: usize,
    attempt_budget: usize,
) -> bool {
    if removed == 0 || attempt_budget < 2 {
        return false;
    }

    let broad_batch = original_len > attempt_budget.saturating_mul(attempt_budget);
    let expected_progress = original_len.div_ceil(attempt_budget);
    broad_batch && removed < expected_progress
}

/// Expand root pins into locally consistent dependency components.
///
/// The batch starts with crates that are fresh in the current lockfile. This
/// helper reads local registry dependency metadata and pulls in any locked
/// package that must move with those roots. Components are solved in memory first
/// so Cargo only sees assignments that are likely to be coherent.
fn solve_cooldown_batch_locally(
    ctx: &CooldownBatchSolverCtx<'_>,
    registry_store: &mut RegistryStore,
    pins: Vec<LockfilePin>,
) -> Result<Vec<LockfilePin>> {
    let locked_packages = locked_packages_by_identity(ctx.crate_states);
    let dependency_resolver = LocalDependencyResolver::new(ctx, &locked_packages);
    let mut root_names_by_key: HashMap<LocalPackageKey, BTreeSet<String>> = HashMap::new();
    for pin in pins {
        root_names_by_key
            .entry(LocalPackageKey::new(
                pin.source_id,
                pin.name,
                pin.current_version,
            ))
            .or_default()
            .extend(pin.root_names);
    }
    let plans = build_local_solver_plans(
        ctx,
        registry_store,
        &locked_packages,
        &dependency_resolver,
        &mut root_names_by_key,
    )?;
    let mut solved_pins = Vec::new();

    for component in local_solver_components(&plans) {
        let budget = local_component_search_budget(&component, &plans);
        let Some(assignment) = solve_local_component(&component, &plans, &locked_packages, budget)
        else {
            let roots = component_root_names(&component, &plans);
            debug!(
                target: "cargo_cooldown::timing",
                size = component.len(),
                budget = budget.assignment_visits,
                estimated_space = budget.estimated_space,
                roots = %roots.iter().cloned().collect::<Vec<_>>().join(", "),
                "skipping locally incompatible cooldown batch component"
            );
            continue;
        };

        solved_pins.extend(local_assignment_pins(&assignment, &plans));
    }

    solved_pins.sort_by(|left, right| {
        left.source_id
            .cmp(&right.source_id)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.current_version.cmp(&right.current_version))
    });
    Ok(solved_pins)
}

fn locked_packages_by_identity(
    crate_states: &HashMap<PackageId, CrateState>,
) -> HashMap<LocalPackageKey, LockedPackageInfo> {
    let mut packages = HashMap::new();
    for (package_id, state) in crate_states {
        let info = LockedPackageInfo {
            package_id: package_id.clone(),
            name: state.name.clone(),
            source_id: state.source_id.clone(),
            current_version: state.current_version.clone(),
            minimum_minutes: state.minimum_minutes,
        };
        packages
            .entry(LocalPackageKey::from_package(&info))
            .or_insert(info);
    }
    packages
}

/// Build the search plans used by the local dependency solver.
///
/// The queue starts with root pins selected by cooldown. While planning their
/// candidate versions, any dependency or reverse dependency that could be broken
/// is added to the same planning set. The result is a map of package keys to the
/// candidate versions the local search may try.
fn build_local_solver_plans(
    ctx: &CooldownBatchSolverCtx<'_>,
    registry_store: &mut RegistryStore,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
    dependency_resolver: &LocalDependencyResolver,
    root_names_by_key: &mut HashMap<LocalPackageKey, BTreeSet<String>>,
) -> Result<HashMap<LocalPackageKey, LocalSolverPlan>> {
    let root_keys = root_names_by_key.keys().cloned().collect::<HashSet<_>>();
    let mut required_requirements_by_key: HashMap<LocalPackageKey, Vec<VersionReq>> =
        HashMap::new();
    let mut plans = HashMap::new();
    let mut queue = root_names_by_key.keys().cloned().collect::<VecDeque<_>>();

    while let Some(key) = queue.pop_front() {
        if plans.contains_key(&key) {
            continue;
        }
        let Some(locked) = locked_packages.get(&key) else {
            debug!(
                target: "cargo_cooldown::timing",
                crate = %key.name,
                version = %key.current_version,
                "skipping local cooldown planning because the package is not present in the current lockfile state"
            );
            continue;
        };
        let is_root = root_keys.contains(&key);
        let Some(plan) = build_local_solver_plan(
            ctx,
            registry_store,
            locked,
            root_names_by_key.get(&key).cloned().unwrap_or_default(),
            is_root,
            required_requirements_by_key
                .get(&key)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            dependency_resolver,
        )?
        else {
            continue;
        };

        for candidate in &plan.candidates {
            for dependency in &candidate.dependencies {
                let Some(dependency_package) = locked_packages.get(&dependency.key) else {
                    continue;
                };
                let current_satisfies = requirement_matches_version(
                    &dependency.requirement,
                    &dependency_package.current_version,
                );
                if !root_names_by_key.contains_key(&dependency.key) && current_satisfies {
                    continue;
                }
                // If a candidate would require changing a dependency that was not
                // part of the original pin set, pull that dependency into the same
                // local component so Cargo sees a coherent assignment.
                let requirement_added = record_local_required_requirement(
                    &mut required_requirements_by_key,
                    dependency.key.clone(),
                    dependency.requirement.clone(),
                );
                let dependency_roots = root_names_by_key.entry(dependency.key.clone()).or_default();
                let before = dependency_roots.len();
                dependency_roots.extend(plan.root_names.iter().cloned());
                let should_requeue = (requirement_added && plans.remove(&dependency.key).is_some())
                    || before != dependency_roots.len()
                    || !plans.contains_key(&dependency.key);
                if should_requeue {
                    queue.push_back(dependency.key.clone());
                }
            }

            for origin in ctx
                .requirement_origins
                .get(&plan.package.package_id)
                .into_iter()
                .flatten()
            {
                let requirement = origin.requirement_req();
                if requirement.matches(&candidate.parsed_version) {
                    continue;
                }
                // Reverse constraints matter too: lowering this candidate can break
                // already-selected parents, so those parents must be planned with it.
                let Some(parent_key) = dependency_resolver.key_for_package_id(&origin.parent_id)
                else {
                    continue;
                };
                if parent_key == &key {
                    continue;
                }
                let parent_roots = root_names_by_key.entry(parent_key.clone()).or_default();
                let before = parent_roots.len();
                parent_roots.extend(plan.root_names.iter().cloned());
                if before != parent_roots.len() || !plans.contains_key(parent_key) {
                    queue.push_back(parent_key.clone());
                }
            }
        }

        plans.insert(key, plan);
    }

    for (key, plan) in &mut plans {
        if let Some(root_names) = root_names_by_key.get(key) {
            plan.root_names = root_names.clone();
        }
    }

    Ok(plans)
}

fn record_local_required_requirement(
    required_requirements_by_key: &mut HashMap<LocalPackageKey, Vec<VersionReq>>,
    key: LocalPackageKey,
    requirement: VersionReq,
) -> bool {
    let requirements = required_requirements_by_key.entry(key).or_default();
    let label = requirement.to_string();
    if requirements
        .iter()
        .any(|existing| existing.to_string() == label)
    {
        return false;
    }
    requirements.push(requirement);
    true
}

/// Build candidate versions for one locked package in a local solver component.
///
/// Root packages must move to an older cooldown-safe version. Non-root packages
/// may stay where they are if their current version still satisfies all external
/// requirements. The returned plan is `None` when registry metadata is missing or
/// no candidate can satisfy the known constraints.
fn build_local_solver_plan(
    ctx: &CooldownBatchSolverCtx<'_>,
    registry_store: &mut RegistryStore,
    locked: &LockedPackageInfo,
    root_names: BTreeSet<String>,
    is_root: bool,
    required_requirements: &[VersionReq],
    dependency_resolver: &LocalDependencyResolver,
) -> Result<Option<LocalSolverPlan>> {
    let context = registry_store
        .context_for_source(&locked.source_id)?
        .clone();
    let timeline = registry_store.timeline_for(&locked.source_id, &locked.name)?;
    ensure_timeline_available(&context, &locked.name, &timeline)?;
    let mut external_requirements = local_candidate_requirements(ctx, locked, is_root);
    if let Some(requirement) = baseline_floor_requirement(
        ctx.initial_lockfile,
        ctx.config,
        &context.effective_index_url,
        &locked.name,
        &locked.current_version,
    ) {
        external_requirements.push(requirement);
    }
    let mut candidates = Vec::new();
    let mut seen_versions = HashSet::new();
    let mut current_candidate_available = false;

    // Non-root packages may stay at their current version when that version still
    // satisfies the external requirements. Root packages must pick an older pin.
    if !is_root
        && version_matches_requirements(&locked.current_version, &external_requirements)
        && let Ok(parsed_version) = Version::parse(&locked.current_version)
    {
        let dependencies = local_candidate_dependencies(
            registry_store,
            dependency_resolver,
            locked,
            &locked.current_version,
        )?
        .unwrap_or_default();
        candidates.push(LocalCandidate {
            version: locked.current_version.clone(),
            parsed_version,
            pinned: false,
            dependencies,
        });
        seen_versions.insert(locked.current_version.clone());
        current_candidate_available = true;
    }

    if is_root || !required_requirements.is_empty() || !current_candidate_available {
        append_local_release_candidates(
            registry_store,
            dependency_resolver,
            locked,
            select_candidates(
                &timeline,
                &locked.current_version,
                &external_requirements,
                locked.minimum_minutes,
                ctx.now,
                |version| {
                    baseline_allows_candidate(
                        ctx.initial_lockfile,
                        ctx.config,
                        &context.effective_index_url,
                        &locked.name,
                        version,
                    )
                },
                COOLDOWN_LOCAL_SEED_CANDIDATES,
            ),
            &mut seen_versions,
            &mut candidates,
        )?;
    }

    for required_requirement in required_requirements {
        let mut requirements = external_requirements.clone();
        requirements.push(required_requirement.clone());
        append_local_release_candidates(
            registry_store,
            dependency_resolver,
            locked,
            select_candidates(
                &timeline,
                &locked.current_version,
                &requirements,
                locked.minimum_minutes,
                ctx.now,
                |version| {
                    baseline_allows_candidate(
                        ctx.initial_lockfile,
                        ctx.config,
                        &context.effective_index_url,
                        &locked.name,
                        version,
                    )
                },
                COOLDOWN_LOCAL_REQUIRED_CANDIDATES_PER_REQUIREMENT,
            ),
            &mut seen_versions,
            &mut candidates,
        )?;
    }

    if candidates.is_empty() {
        debug!(
            target: "cargo_cooldown::timing",
            crate = %locked.name,
            current = %locked.current_version,
            "skipping local cooldown package because no candidate versions are available"
        );
        return Ok(None);
    }

    Ok(Some(LocalSolverPlan {
        package: locked.clone(),
        root_names,
        candidates,
    }))
}

fn append_local_release_candidates(
    registry_store: &mut RegistryStore,
    dependency_resolver: &LocalDependencyResolver,
    locked: &LockedPackageInfo,
    releases: Vec<&crate::registry::Release>,
    seen_versions: &mut HashSet<String>,
    candidates: &mut Vec<LocalCandidate>,
) -> Result<()> {
    for release in releases {
        if !seen_versions.insert(release.version.clone()) {
            continue;
        }
        let Some(candidate) = build_local_release_candidate(
            registry_store,
            dependency_resolver,
            locked,
            &release.version,
            true,
        )?
        else {
            continue;
        };
        candidates.push(candidate);
    }
    Ok(())
}

fn build_local_release_candidate(
    registry_store: &mut RegistryStore,
    dependency_resolver: &LocalDependencyResolver,
    locked: &LockedPackageInfo,
    version: &str,
    pinned: bool,
) -> Result<Option<LocalCandidate>> {
    if pinned
        && registry_store
            .local_release_checksum(&locked.source_id, &locked.name, version)?
            .is_none()
    {
        debug!(
            target: "cargo_cooldown::timing",
            crate = %locked.name,
            version,
            "skipping local cooldown candidate because the local index does not expose a checksum"
        );
        return Ok(None);
    }
    let Ok(parsed_version) = Version::parse(version) else {
        return Ok(None);
    };
    let Some(dependencies) =
        local_candidate_dependencies(registry_store, dependency_resolver, locked, version)?
    else {
        return Ok(None);
    };

    Ok(Some(LocalCandidate {
        version: version.to_string(),
        parsed_version,
        pinned,
        dependencies,
    }))
}

fn local_candidate_dependencies(
    registry_store: &mut RegistryStore,
    dependency_resolver: &LocalDependencyResolver,
    locked: &LockedPackageInfo,
    version: &str,
) -> Result<Option<Vec<LocalCandidateDependency>>> {
    let Some(dependencies) =
        registry_store.local_release_dependencies(&locked.source_id, &locked.name, version)?
    else {
        debug!(
            target: "cargo_cooldown::timing",
            crate = %locked.name,
            version,
            "skipping local cooldown candidate because dependency metadata is unavailable"
        );
        return Ok(None);
    };

    Ok(Some(
        dependencies
            .into_iter()
            .filter_map(|dependency| {
                if (dependency.optional || dependency.target_specific)
                    && !dependency_resolver
                        .has_active_dependency(&locked.package_id, &dependency.crate_name)
                {
                    return None;
                }
                let Some(key) = dependency_resolver.resolve_dependency(
                    &locked.package_id,
                    &locked.source_id,
                    &dependency.crate_name,
                    &dependency.requirement,
                ) else {
                    trace!(
                        target: "cargo_cooldown::solver",
                        crate = %locked.name,
                        version,
                        dependency = %dependency.crate_name,
                        requirement = %dependency.requirement,
                        "leaving local cooldown dependency to Cargo validation because no unambiguous locked package matches"
                    );
                    return None;
                };
                Some(LocalCandidateDependency {
                    key,
                    requirement: dependency.requirement,
                })
            })
            .collect(),
    ))
}

fn local_candidate_requirements(
    ctx: &CooldownBatchSolverCtx<'_>,
    locked: &LockedPackageInfo,
    is_root: bool,
) -> Vec<VersionReq> {
    let requirements = ctx
        .version_requirements
        .get(&locked.package_id)
        .cloned()
        .unwrap_or_default();
    if is_root {
        return requirements;
    }

    Vec::new()
}

fn local_solver_components(
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
) -> Vec<Vec<LocalPackageKey>> {
    let mut edges: HashMap<LocalPackageKey, BTreeSet<LocalPackageKey>> = plans
        .keys()
        .cloned()
        .map(|key| (key, BTreeSet::new()))
        .collect();

    for (key, plan) in plans {
        for candidate in &plan.candidates {
            for dependency in &candidate.dependencies {
                if !plans.contains_key(&dependency.key) {
                    continue;
                }
                edges
                    .entry(key.clone())
                    .or_default()
                    .insert(dependency.key.clone());
                edges
                    .entry(dependency.key.clone())
                    .or_default()
                    .insert(key.clone());
            }
        }
    }

    let mut remaining = edges.keys().cloned().collect::<BTreeSet<_>>();
    let mut components = Vec::new();
    while let Some(start) = remaining.iter().next().cloned() {
        let mut queue = VecDeque::from([start]);
        let mut component = Vec::new();
        while let Some(key) = queue.pop_front() {
            if !remaining.remove(&key) {
                continue;
            }
            component.push(key.clone());
            if let Some(neighbors) = edges.get(&key) {
                for neighbor in neighbors {
                    if remaining.contains(neighbor) {
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }
        component.sort();
        components.push(component);
    }

    components.sort_by_key(|component| std::cmp::Reverse(component.len()));
    components
}

fn solve_local_component(
    component: &[LocalPackageKey],
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
    budget: LocalSearchBudget,
) -> Option<BTreeMap<LocalPackageKey, LocalCandidate>> {
    let mut ordered = component.to_vec();
    ordered.sort_by_key(|key| {
        plans
            .get(key)
            .map(|plan| {
                (
                    plan.candidates.len(),
                    std::cmp::Reverse(plan.root_names.len()),
                )
            })
            .unwrap_or((usize::MAX, std::cmp::Reverse(0)))
    });
    let mut assignment = BTreeMap::new();
    let mut remaining_visits = budget.assignment_visits;

    if search_local_component(
        &ordered,
        0,
        &mut assignment,
        &mut remaining_visits,
        plans,
        locked_packages,
    ) {
        debug!(
            target: "cargo_cooldown::timing",
            size = component.len(),
            budget = budget.assignment_visits,
            estimated_space = budget.estimated_space,
            attempts = budget.assignment_visits - remaining_visits,
            "solved local cooldown batch component"
        );
        return Some(assignment);
    }

    debug!(
        target: "cargo_cooldown::timing",
        size = component.len(),
        budget = budget.assignment_visits,
        estimated_space = budget.estimated_space,
        attempts = budget.assignment_visits - remaining_visits,
        exhausted = remaining_visits == 0,
        reason = %local_component_failure_reason(&ordered, plans, locked_packages)
            .unwrap_or_else(|| "no locally compatible assignment".to_string()),
        "failed to solve local cooldown batch component"
    );
    None
}

fn local_component_search_budget(
    component: &[LocalPackageKey],
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
) -> LocalSearchBudget {
    // Exhaustive search is cheap for small components and bounded for large ones.
    // The floor avoids starving wide but still manageable dependency components.
    let estimated_space = capped_local_assignment_space(
        component,
        plans,
        COOLDOWN_LOCAL_ASSIGNMENT_CEILING.saturating_add(1),
    );
    let branchy_packages = component
        .iter()
        .filter(|key| {
            plans
                .get(*key)
                .is_some_and(|plan| plan.candidates.len() > 1)
        })
        .count()
        .max(1);
    let widest_candidate_set = component
        .iter()
        .filter_map(|key| plans.get(key).map(|plan| plan.candidates.len()))
        .max()
        .unwrap_or(1)
        .max(1);
    let floor = component
        .len()
        .max(1)
        .saturating_mul(branchy_packages)
        .saturating_mul(widest_candidate_set)
        .saturating_mul(8);
    let assignment_visits = if estimated_space <= COOLDOWN_LOCAL_ASSIGNMENT_CEILING {
        estimated_space.max(floor)
    } else {
        floor
            .saturating_mul(component.len().max(1))
            .min(COOLDOWN_LOCAL_ASSIGNMENT_CEILING)
    }
    .max(1);

    LocalSearchBudget {
        assignment_visits,
        estimated_space,
    }
}

fn capped_local_assignment_space(
    component: &[LocalPackageKey],
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    cap: usize,
) -> usize {
    let mut product = 1usize;
    for key in component {
        let candidates = plans
            .get(key)
            .map(|plan| plan.candidates.len().max(1))
            .unwrap_or(1);
        product = product.saturating_mul(candidates);
        if product >= cap {
            return cap;
        }
    }
    product
}

fn search_local_component(
    ordered: &[LocalPackageKey],
    index: usize,
    assignment: &mut BTreeMap<LocalPackageKey, LocalCandidate>,
    budget: &mut usize,
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
) -> bool {
    if index >= ordered.len() {
        return true;
    }
    let key = &ordered[index];
    let Some(plan) = plans.get(key) else {
        return false;
    };

    for candidate in &plan.candidates {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if !local_candidate_matches_assignment(key, candidate, assignment, plans, locked_packages) {
            continue;
        }
        assignment.insert(key.clone(), candidate.clone());
        if remaining_local_candidates_are_viable(
            ordered,
            index + 1,
            assignment,
            plans,
            locked_packages,
        ) && search_local_component(
            ordered,
            index + 1,
            assignment,
            budget,
            plans,
            locked_packages,
        ) {
            return true;
        }
        assignment.remove(key);
    }

    false
}

fn remaining_local_candidates_are_viable(
    ordered: &[LocalPackageKey],
    start: usize,
    assignment: &BTreeMap<LocalPackageKey, LocalCandidate>,
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
) -> bool {
    ordered[start..].iter().all(|key| {
        plans.get(key).is_some_and(|plan| {
            plan.candidates.iter().any(|candidate| {
                local_candidate_matches_assignment(
                    key,
                    candidate,
                    assignment,
                    plans,
                    locked_packages,
                )
            })
        })
    })
}

fn local_candidate_matches_assignment(
    key: &LocalPackageKey,
    candidate: &LocalCandidate,
    assignment: &BTreeMap<LocalPackageKey, LocalCandidate>,
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
) -> bool {
    for dependency in &candidate.dependencies {
        if let Some(assigned_dependency) = assignment.get(&dependency.key) {
            if !dependency
                .requirement
                .matches(&assigned_dependency.parsed_version)
            {
                return false;
            }
        } else if plans.contains_key(&dependency.key) {
            continue;
        } else if let Some(locked_dependency) = locked_packages.get(&dependency.key)
            && !requirement_matches_version(
                &dependency.requirement,
                &locked_dependency.current_version,
            )
        {
            return false;
        }
    }

    for (other_key, other_candidate) in assignment {
        if other_key == key {
            return false;
        }
        for dependency in &other_candidate.dependencies {
            if &dependency.key == key && !dependency.requirement.matches(&candidate.parsed_version)
            {
                return false;
            }
        }
    }

    true
}

fn local_component_failure_reason(
    ordered: &[LocalPackageKey],
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
) -> Option<String> {
    let mut assignment = BTreeMap::new();
    for (index, key) in ordered.iter().enumerate() {
        let plan = plans.get(key)?;
        let Some(candidate) = plan.candidates.iter().find(|candidate| {
            local_candidate_matches_assignment(key, candidate, &assignment, plans, locked_packages)
        }) else {
            let details = plan
                .candidates
                .iter()
                .filter_map(|candidate| {
                    local_candidate_direct_rejection(
                        key,
                        candidate,
                        &assignment,
                        plans,
                        locked_packages,
                    )
                    .map(|reason| format!("{}: {reason}", candidate.version))
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Some(format!(
                "{} {} has no viable candidates after assigning {} package(s) ({details})",
                plan.package.name, plan.package.current_version, index
            ));
        };
        assignment.insert(key.clone(), candidate.clone());
        if ordered[index + 1..].iter().all(|remaining_key| {
            plans.get(remaining_key).is_some_and(|remaining_plan| {
                remaining_plan.candidates.iter().any(|remaining_candidate| {
                    local_candidate_matches_assignment(
                        remaining_key,
                        remaining_candidate,
                        &assignment,
                        plans,
                        locked_packages,
                    )
                })
            })
        }) {
            continue;
        }
        if let Some(blocked_key) = ordered[index + 1..].iter().find(|remaining_key| {
            plans.get(*remaining_key).is_some_and(|remaining_plan| {
                !remaining_plan.candidates.iter().any(|remaining_candidate| {
                    local_candidate_matches_assignment(
                        remaining_key,
                        remaining_candidate,
                        &assignment,
                        plans,
                        locked_packages,
                    )
                })
            })
        }) {
            let blocked_plan = plans.get(blocked_key)?;
            let details = blocked_plan
                .candidates
                .iter()
                .filter_map(|candidate| {
                    local_candidate_direct_rejection(
                        blocked_key,
                        candidate,
                        &assignment,
                        plans,
                        locked_packages,
                    )
                    .map(|reason| format!("{}: {reason}", candidate.version))
                })
                .collect::<Vec<_>>()
                .join("; ");
            return Some(format!(
                "after assigning {} {} -> {}, {} {} has no viable candidates ({details})",
                plan.package.name,
                plan.package.current_version,
                candidate.version,
                blocked_plan.package.name,
                blocked_plan.package.current_version
            ));
        }
    }
    None
}

fn local_candidate_direct_rejection(
    key: &LocalPackageKey,
    candidate: &LocalCandidate,
    assignment: &BTreeMap<LocalPackageKey, LocalCandidate>,
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
    locked_packages: &HashMap<LocalPackageKey, LockedPackageInfo>,
) -> Option<String> {
    for dependency in &candidate.dependencies {
        if let Some(assigned_dependency) = assignment.get(&dependency.key)
            && !dependency
                .requirement
                .matches(&assigned_dependency.parsed_version)
        {
            return Some(format!(
                "{} requires {} {}, but assigned {}",
                candidate.version,
                dependency.key.name,
                dependency.requirement,
                assigned_dependency.version
            ));
        }
        if plans.contains_key(&dependency.key) {
            continue;
        }
        if let Some(locked_dependency) = locked_packages.get(&dependency.key)
            && !requirement_matches_version(
                &dependency.requirement,
                &locked_dependency.current_version,
            )
        {
            return Some(format!(
                "{} requires {} {}, but locked {} is outside the local component",
                candidate.version,
                locked_dependency.name,
                dependency.requirement,
                locked_dependency.current_version
            ));
        }
    }

    for (other_key, other_candidate) in assignment {
        for dependency in &other_candidate.dependencies {
            if &dependency.key == key && !dependency.requirement.matches(&candidate.parsed_version)
            {
                return Some(format!(
                    "{} requires {} {}, but candidate is {}",
                    other_key.name, key.name, dependency.requirement, candidate.version
                ));
            }
        }
    }

    Some(format!(
        "{} is rejected only after considering assigned neighbors for {}",
        candidate.version, key.name
    ))
}

fn local_assignment_pins(
    assignment: &BTreeMap<LocalPackageKey, LocalCandidate>,
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
) -> Vec<LockfilePin> {
    assignment
        .iter()
        .filter_map(|(key, candidate)| {
            if !candidate.pinned {
                return None;
            }
            let plan = plans.get(key)?;
            Some(LockfilePin {
                root_names: plan.root_names.clone(),
                name: plan.package.name.clone(),
                source_id: plan.package.source_id.clone(),
                current_version: plan.package.current_version.clone(),
                target_version: candidate.version.clone(),
            })
        })
        .collect()
}

fn component_root_names(
    component: &[LocalPackageKey],
    plans: &HashMap<LocalPackageKey, LocalSolverPlan>,
) -> BTreeSet<String> {
    component
        .iter()
        .filter_map(|key| plans.get(key))
        .flat_map(|plan| plan.root_names.iter().cloned())
        .collect()
}

fn version_matches_requirements(version: &str, requirements: &[VersionReq]) -> bool {
    let Ok(parsed) = Version::parse(version) else {
        return false;
    };
    requirements
        .iter()
        .all(|requirement| requirement.matches(&parsed))
}

/// Convert the initial lockfile version into a semver floor when baseline mode requires it.
///
/// Under `lockfile_baseline = "floor"`, an already locked version becomes the
/// minimum acceptable candidate for the same crate and registry. Returning
/// `None` means the current baseline mode allows normal cooldown downgrades.
fn baseline_floor_requirement(
    initial_lockfile: &LockfileSnapshot,
    config: &Config,
    registry: &str,
    name: &str,
    current_version: &str,
) -> Option<VersionReq> {
    if !config.lockfile_baseline.uses_initial_lockfile_floor() {
        return None;
    }

    // Default baseline treats the pre-run lockfile as the minimum effective
    // version, preventing accidental downgrades of already locked packages.
    let floor =
        initial_lockfile
            .baseline()
            .newest_version_at_or_below(name, registry, current_version)?;

    Some(VersionReq {
        comparators: vec![Comparator {
            op: Op::GreaterEq,
            major: floor.major,
            minor: Some(floor.minor),
            patch: Some(floor.patch),
            pre: floor.pre,
        }],
    })
}

fn baseline_allows_candidate(
    initial_lockfile: &LockfileSnapshot,
    config: &Config,
    registry: &str,
    name: &str,
    version: &str,
) -> bool {
    config.lockfile_baseline.uses_initial_lockfile_floor()
        && initial_lockfile
            .baseline()
            .contains_registry_version(name, registry, version)
}

fn requirement_matches_version(requirement: &VersionReq, version: &str) -> bool {
    Version::parse(version).is_ok_and(|parsed| requirement.matches(&parsed))
}

#[cfg(test)]
fn exact_requirement_version(requirement: &VersionReq) -> Option<String> {
    if requirement.comparators.len() != 1 {
        return None;
    }
    let comparator = &requirement.comparators[0];
    if !matches!(comparator.op, semver::Op::Exact) {
        return None;
    }
    let minor = comparator.minor?;
    let patch = comparator.patch?;
    let mut version = format!("{}.{}.{}", comparator.major, minor, patch);
    if !comparator.pre.is_empty() {
        version.push('-');
        version.push_str(comparator.pre.as_str());
    }
    Some(version)
}

#[derive(Clone)]
struct BundleCandidate {
    version: String,
    parsed_version: Version,
    internal_requirements: BTreeMap<String, VersionReq>,
}

#[derive(Clone)]
struct BundleMemberPlan {
    fresh: FreshCrate,
    candidates: Vec<BundleCandidate>,
}

struct CoordinatedResolutionCtx<'a> {
    manifest: &'a Manifest,
    workspace: &'a Workspace,
    features: &'a Features,
    config: &'a Config,
    lockfile_path: &'a Path,
    initial_lockfile: &'a LockfileSnapshot,
    requirement_origins: &'a HashMap<PackageId, Vec<RequirementOrigin>>,
    now: DateTime<Utc>,
}

/// Try one bounded solve for fresh crates tied together by exact requirements.
///
/// Some packages cannot be cooled one at a time because they depend on each
/// other with exact `=x.y.z` requirements. This receives the unresolved
/// cargo-compatible entries, groups connected exact-requirement components, searches a
/// small compatible assignment, and applies it as one lockfile update. It returns
/// `true` only when Cargo accepted a bundle and the outer loop should rescan.
fn attempt_coordinated_bundle_resolution(
    ctx: &CoordinatedResolutionCtx<'_>,
    registry_store: &mut RegistryStore,
    cargo_compatible_entries: &[FreshCrate],
    constraint_edges: &HashMap<String, HashSet<String>>,
) -> Result<bool> {
    let components = cargo_compatible_components(cargo_compatible_entries, constraint_edges);

    for component in components {
        if component.len() > MAX_COORDINATED_COMPONENT_SIZE {
            debug!(
                component_size = component.len(),
                "skipping coordinated bundle attempt because the unresolved component is too large"
            );
            continue;
        }
        if has_duplicate_crate_names(&component) {
            debug!(
                component = %format_component(&component),
                "skipping coordinated bundle attempt because the unresolved component contains duplicate crate names"
            );
            continue;
        }

        let Some(assignment) = find_coordinated_assignment(
            registry_store,
            ctx.requirement_origins,
            ctx.initial_lockfile,
            ctx.config,
            &component,
            ctx.now,
        )?
        else {
            continue;
        };

        debug!(
            component = %format_component(&component),
            assignment = %format_assignment(&assignment),
            "attempting coordinated bundle resolution for resolver-constrained crates"
        );

        if apply_coordinated_assignment(
            ctx.manifest,
            ctx.workspace,
            ctx.features,
            ctx.lockfile_path,
            registry_store,
            &component,
            &assignment,
        )? {
            debug!(
                component = %format_component(&component),
                assignment = %format_assignment(&assignment),
                "coordinated bundle resolution succeeded"
            );
            return Ok(true);
        }
    }

    Ok(false)
}

fn cargo_compatible_components(
    cargo_compatible_entries: &[FreshCrate],
    constraint_edges: &HashMap<String, HashSet<String>>,
) -> Vec<Vec<FreshCrate>> {
    let entries_by_key: HashMap<String, FreshCrate> = cargo_compatible_entries
        .iter()
        .cloned()
        .map(|entry| {
            (
                crate_failure_key(&entry.source_id, &entry.name, &entry.current_version),
                entry,
            )
        })
        .collect();
    let mut remaining: HashSet<String> = entries_by_key.keys().cloned().collect();
    let mut components = Vec::new();

    while let Some(start) = remaining.iter().next().cloned() {
        let mut queue = VecDeque::from([start.clone()]);
        let mut component_keys = Vec::new();

        while let Some(key) = queue.pop_front() {
            if !remaining.remove(&key) {
                continue;
            }
            component_keys.push(key.clone());
            if let Some(neighbors) = constraint_edges.get(&key) {
                for neighbor in neighbors {
                    if remaining.contains(neighbor) {
                        queue.push_back(neighbor.clone());
                    }
                }
            }
        }

        component_keys.sort();
        let component = component_keys
            .into_iter()
            .filter_map(|key| entries_by_key.get(&key).cloned())
            .collect::<Vec<_>>();
        components.push(component);
    }

    components.sort_by_key(|component| std::cmp::Reverse(component.len()));
    components
}

fn has_duplicate_crate_names(component: &[FreshCrate]) -> bool {
    let mut seen = HashSet::new();
    component
        .iter()
        .any(|entry| !seen.insert(entry.name.clone()))
}

/// Search compatible versions for one exact-requirement bundle.
///
/// The input component is already small and connected. For each member we collect
/// cooldown-safe candidates, separate external constraints from internal bundle
/// constraints, and backtrack over a bounded candidate space. The returned map is
/// crate name to target version, or `None` when no coherent assignment is found.
fn find_coordinated_assignment(
    registry_store: &mut RegistryStore,
    requirement_origins: &HashMap<PackageId, Vec<RequirementOrigin>>,
    initial_lockfile: &LockfileSnapshot,
    config: &Config,
    component: &[FreshCrate],
    now: DateTime<Utc>,
) -> Result<Option<BTreeMap<String, String>>> {
    let component_ids: HashSet<PackageId> = component
        .iter()
        .map(|entry| entry.package_id.clone())
        .collect();
    let component_names: HashSet<String> =
        component.iter().map(|entry| entry.name.clone()).collect();
    let mut plans = Vec::with_capacity(component.len());

    for fresh in component {
        // Requirements from inside the bundle are solved together; requirements
        // from outside the bundle still constrain each member independently.
        let external_requirements = requirement_origins
            .get(&fresh.package_id)
            .map(|origins| {
                origins
                    .iter()
                    .filter(|origin| !component_ids.contains(&origin.parent_id))
                    .map(RequirementOrigin::requirement_req)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let context = registry_store.context_for_source(&fresh.source_id)?.clone();
        let timeline = registry_store.timeline_for(&fresh.source_id, &fresh.name)?;
        let mut external_requirements = external_requirements;
        if let Some(requirement) = baseline_floor_requirement(
            initial_lockfile,
            config,
            &context.effective_index_url,
            &fresh.name,
            &fresh.current_version,
        ) {
            external_requirements.push(requirement);
        }
        let candidates = select_candidates(
            &timeline,
            &fresh.current_version,
            &external_requirements,
            fresh.minimum_minutes,
            now,
            |version| {
                baseline_allows_candidate(
                    initial_lockfile,
                    config,
                    &context.effective_index_url,
                    &fresh.name,
                    version,
                )
            },
            MAX_COORDINATED_CANDIDATES,
        );

        let mut bundle_candidates = Vec::new();
        for candidate in candidates {
            let Some(dependencies) = registry_store.local_release_dependencies(
                &fresh.source_id,
                &fresh.name,
                &candidate.version,
            )?
            else {
                return Ok(None);
            };
            let internal_requirements = dependencies
                .into_iter()
                .filter(|dependency| component_names.contains(&dependency.crate_name))
                .map(|dependency| (dependency.crate_name, dependency.requirement))
                .collect::<BTreeMap<_, _>>();
            let Ok(parsed_version) = Version::parse(&candidate.version) else {
                continue;
            };
            bundle_candidates.push(BundleCandidate {
                version: candidate.version.clone(),
                parsed_version,
                internal_requirements,
            });
        }

        if bundle_candidates.is_empty() {
            return Ok(None);
        }

        plans.push(BundleMemberPlan {
            fresh: fresh.clone(),
            candidates: bundle_candidates,
        });
    }

    plans.sort_by_key(|plan| plan.candidates.len());
    let mut assignment = BTreeMap::new();
    let mut budget = MAX_COORDINATED_ASSIGNMENTS;
    if search_coordinated_assignment(&plans, 0, &mut assignment, &mut budget) {
        return Ok(Some(
            assignment
                .into_iter()
                .map(|(name, candidate)| (name, candidate.version))
                .collect(),
        ));
    }

    Ok(None)
}

fn search_coordinated_assignment(
    plans: &[BundleMemberPlan],
    index: usize,
    assignment: &mut BTreeMap<String, BundleCandidate>,
    budget: &mut usize,
) -> bool {
    if index >= plans.len() {
        return true;
    }

    let plan = &plans[index];
    for candidate in &plan.candidates {
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        if !candidate_matches_assignment(&plan.fresh.name, candidate, assignment) {
            continue;
        }

        assignment.insert(plan.fresh.name.clone(), candidate.clone());
        if search_coordinated_assignment(plans, index + 1, assignment, budget) {
            return true;
        }
        assignment.remove(&plan.fresh.name);
    }

    false
}

fn candidate_matches_assignment(
    crate_name: &str,
    candidate: &BundleCandidate,
    assignment: &BTreeMap<String, BundleCandidate>,
) -> bool {
    for (dependency_name, requirement) in &candidate.internal_requirements {
        if let Some(dependency_candidate) = assignment.get(dependency_name)
            && !requirement.matches(&dependency_candidate.parsed_version)
        {
            return false;
        }
    }

    for (other_name, other_candidate) in assignment {
        if let Some(requirement) = other_candidate.internal_requirements.get(crate_name)
            && !requirement.matches(&candidate.parsed_version)
        {
            return false;
        }
        if other_name == crate_name {
            return false;
        }
    }

    true
}

fn apply_coordinated_assignment(
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
    lockfile_path: &Path,
    registry_store: &mut RegistryStore,
    component: &[FreshCrate],
    assignment: &BTreeMap<String, String>,
) -> Result<bool> {
    let pins = component
        .iter()
        .filter_map(|fresh| {
            assignment
                .get(&fresh.name)
                .map(|target_version| LockfilePin {
                    root_names: BTreeSet::from([fresh.name.clone()]),
                    name: fresh.name.clone(),
                    source_id: fresh.source_id.clone(),
                    current_version: fresh.current_version.clone(),
                    target_version: target_version.clone(),
                })
        })
        .collect::<Vec<_>>();

    apply_lockfile_pin_assignment(
        manifest,
        workspace,
        features,
        lockfile_path,
        registry_store,
        &pins,
    )
}

fn apply_lockfile_pin_assignment(
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
    lockfile_path: &Path,
    registry_store: &mut RegistryStore,
    pins: &[LockfilePin],
) -> Result<bool> {
    Ok(matches!(
        apply_lockfile_pin_assignment_detailed(
            manifest,
            workspace,
            features,
            lockfile_path,
            registry_store,
            pins,
        )?,
        BatchPinOutcome::Applied { .. }
    ))
}

/// Write a proposed pin assignment and ask Cargo to validate the result.
///
/// The current lockfile is captured first. After rewriting package versions and
/// checksums, Cargo is run with `--locked`; if Cargo needs to refresh derived
/// lockfile data, one unlocked metadata pass is allowed and then rechecked with
/// `--locked`. Any rejection restores the captured lockfile and reports why the
/// batch failed.
fn apply_lockfile_pin_assignment_detailed(
    manifest: &Manifest,
    workspace: &Workspace,
    features: &Features,
    lockfile_path: &Path,
    registry_store: &mut RegistryStore,
    pins: &[LockfilePin],
) -> Result<BatchPinOutcome> {
    let lockfile_snapshot = timed_debug!("capture lockfile before batch assignment", {
        LockfileSnapshot::capture(lockfile_path, registry_store)
    })?;
    timed_debug!("write batch lockfile assignment", {
        write_lockfile_pin_assignment(lockfile_path, registry_store, pins)
    })?;

    let locked_metadata = match timed_debug!("batch validation cargo metadata --locked", {
        read_metadata_locked(manifest, features)
    }) {
        Ok(metadata) => metadata,
        Err(err) => {
            let error = err.to_string();
            if !parse_batch_conflict_packages(&error).is_empty() {
                debug!(
                    target: "cargo_cooldown::timing",
                    error = %error,
                    "cooldown batch solver rejected by cargo metadata --locked"
                );
                lockfile_snapshot.restore(lockfile_path)?;
                return Ok(BatchPinOutcome::Rejected { error });
            }

            debug!(
                target: "cargo_cooldown::timing",
                error = %error,
                "cooldown batch solver lockfile assignment requires Cargo to refresh the lockfile"
            );

            if let Err(unlocked_err) = timed_debug!("batch validation cargo metadata", {
                read_metadata(manifest, features)
            }) {
                let error = unlocked_err.to_string();
                debug!(
                    target: "cargo_cooldown::timing",
                    error = %error,
                    "cooldown batch solver rejected by cargo metadata"
                );
                lockfile_snapshot.restore(lockfile_path)?;
                return Ok(BatchPinOutcome::Rejected { error });
            }

            match timed_debug!("batch validation cargo metadata --locked after update", {
                read_metadata_locked(manifest, features)
            }) {
                Ok(metadata) => metadata,
                Err(err) => {
                    let error = err.to_string();
                    debug!(
                        target: "cargo_cooldown::timing",
                        error = %error,
                        "cooldown batch solver rejected by cargo metadata --locked after update"
                    );
                    lockfile_snapshot.restore(lockfile_path)?;
                    return Ok(BatchPinOutcome::Rejected { error });
                }
            }
        }
    };

    if timed_debug!("check batch progress", {
        lockfile_pin_assignment_made_progress(&locked_metadata, workspace, pins)
    }) {
        return Ok(BatchPinOutcome::Applied {
            metadata: Box::new(locked_metadata),
        });
    }

    lockfile_snapshot.restore(lockfile_path)?;
    Ok(BatchPinOutcome::Rejected {
        error: "batch lockfile assignment made no progress".to_string(),
    })
}

/// Rewrite only the package entries targeted by the selected pins.
///
/// The function receives exact current-version identities, so duplicate package
/// names in the lockfile remain distinguishable. It updates version/checksum
/// fields from local registry metadata and deduplicates entries that collapse to
/// the same name/version/source after the rewrite.
fn write_lockfile_pin_assignment(
    lockfile_path: &Path,
    registry_store: &mut RegistryStore,
    pins: &[LockfilePin],
) -> Result<()> {
    // TODO: Replace this lockfile rewrite with a Cargo-native multi-pin once Cargo exposes a
    // public API for resolving several precise package versions in one operation.
    let contents = fs::read_to_string(lockfile_path)
        .with_context(|| format!("failed to read lockfile {}", lockfile_path.display()))?;
    let mut document = toml::from_str::<toml::Value>(&contents)
        .with_context(|| format!("failed to parse lockfile {}", lockfile_path.display()))?;
    let packages = document
        .get_mut("package")
        .and_then(toml::Value::as_array_mut)
        .context("lockfile package list should be a TOML array")?;
    let pins_by_locked_version = pins
        .iter()
        .map(|pin| {
            (
                (
                    pin.name.clone(),
                    pin.source_id.clone(),
                    pin.current_version.clone(),
                ),
                pin,
            )
        })
        .collect::<HashMap<_, _>>();

    for package in packages.iter_mut() {
        let Some(table) = package.as_table_mut() else {
            continue;
        };
        let Some(name) = table.get("name").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some(version) = table.get("version").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some(source) = table.get("source").and_then(toml::Value::as_str) else {
            continue;
        };
        let Some(pin) = pins_by_locked_version.get(&(
            name.to_string(),
            source.to_string(),
            version.to_string(),
        )) else {
            continue;
        };
        let Some(checksum) = registry_store.local_release_checksum(
            &pin.source_id,
            &pin.name,
            &pin.target_version,
        )?
        else {
            bail!(
                "registry {} did not expose a local checksum for lockfile pin candidate {}@{}",
                pin.source_id,
                pin.name,
                pin.target_version
            );
        };

        table.insert(
            "version".to_string(),
            toml::Value::String(pin.target_version.clone()),
        );
        table.insert("checksum".to_string(), toml::Value::String(checksum));
    }

    let mut seen = HashSet::new();
    packages.retain(|package| {
        let Some(table) = package.as_table() else {
            return true;
        };
        let Some(name) = table.get("name").and_then(toml::Value::as_str) else {
            return true;
        };
        let Some(version) = table.get("version").and_then(toml::Value::as_str) else {
            return true;
        };
        let Some(source) = table.get("source").and_then(toml::Value::as_str) else {
            return true;
        };
        seen.insert((name.to_string(), version.to_string(), source.to_string()))
    });

    fs::write(lockfile_path, toml::to_string(&document)?)
        .with_context(|| format!("failed to write lockfile {}", lockfile_path.display()))
}

fn lockfile_pin_assignment_made_progress(
    metadata: &cargo_metadata::Metadata,
    workspace: &Workspace,
    pins: &[LockfilePin],
) -> bool {
    let Some(resolved_versions) = reachable_registry_package_versions(metadata, workspace) else {
        return false;
    };

    pins.iter().any(|pin| {
        pin.target_version != pin.current_version
            && resolved_versions.contains(&pin_version_identity(pin, &pin.target_version))
    })
}

fn pin_version_identity(pin: &LockfilePin, version: &str) -> (String, String, String) {
    (pin.name.clone(), pin.source_id.clone(), version.to_string())
}

fn reachable_registry_package_versions(
    metadata: &cargo_metadata::Metadata,
    workspace: &Workspace,
) -> Option<BTreeSet<(String, String, String)>> {
    let resolve = metadata.resolve.as_ref()?;
    let selected_root_ids = selected_package_ids(metadata, workspace);
    let reachable_ids = reachable_package_ids(resolve, &selected_root_ids);
    let packages: HashMap<PackageId, &cargo_metadata::Package> = metadata
        .packages
        .iter()
        .map(|package| (package.id.clone(), package))
        .collect();
    let mut versions = BTreeSet::new();

    for id in reachable_ids {
        let Some(package) = packages.get(&id) else {
            continue;
        };
        let Some(source) = package.source.as_ref() else {
            continue;
        };
        versions.insert((
            package.name.to_string(),
            source.repr.clone(),
            package.version.to_string(),
        ));
    }

    Some(versions)
}

fn format_component(component: &[FreshCrate]) -> String {
    component
        .iter()
        .map(|entry| format!("{} {}", entry.name, entry.current_version))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_assignment(assignment: &BTreeMap<String, String>) -> String {
    assignment
        .iter()
        .map(|(crate_name, version)| format!("{crate_name} {version}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_blockers(stdout: &str, stderr: &str) -> Vec<Blocker> {
    let mut blockers = Vec::new();
    for line in stdout.lines().chain(stderr.lines()) {
        let trimmed = line.trim();
        if let Some(blocker) = parse_blocker_line(trimmed)
            && !blockers
                .iter()
                .any(|existing: &Blocker| existing == &blocker)
        {
            blockers.push(blocker);
        }
    }
    blockers
}

fn parse_batch_conflict_packages(error: &str) -> Vec<Blocker> {
    let mut blockers = parse_blockers("", error);
    for line in error.lines() {
        let mut rest = line;
        while let Some(start) = rest.find("package `") {
            let after_marker = &rest[start + "package `".len()..];
            let Some(end) = after_marker.find('`') else {
                break;
            };
            let blocker = parse_blocker_inner(&after_marker[..end]);
            if !blockers.iter().any(|existing| existing == &blocker) {
                blockers.push(blocker);
            }
            rest = &after_marker[end + 1..];
        }
    }
    blockers
}

fn parse_blocker_line(line: &str) -> Option<Blocker> {
    for marker in ["required by package `", "previously selected package `"] {
        let Some(start) = line.find(marker) else {
            continue;
        };
        let rest = &line[start + marker.len()..];
        let end = rest.find('`')?;
        return Some(parse_blocker_inner(&rest[..end]));
    }

    None
}

fn parse_blocker_inner(inner: &str) -> Blocker {
    if let Some((name, version)) = inner.rsplit_once(' ') {
        Blocker {
            name: name.to_string(),
            version: Some(version.trim_start_matches('v').to_string()),
        }
    } else {
        Blocker {
            name: inner.to_string(),
            version: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
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

#[cfg(test)]
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
        if !crate::registry::is_registry_source(&source.repr) {
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
            let requirement = manifest_dep.req.to_string();
            if !origins
                .iter()
                .any(|origin| origin.parent_id == node.id && origin.requirement == requirement)
            {
                origins.push(RequirementOrigin {
                    parent_id: node.id.clone(),
                    parent_name: pkg.name.to_string(),
                    requirement: requirement.clone(),
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

/// Unit tests for cooldown execution helpers and summary formatting.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::allow_rules::AllowRules;
    use crate::config::{CargoCompatibleAccept, Config, Enforcement, LockfileBaselineMode};
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

    fn fresh_notice(name: &str, version: &str) -> FreshVersionNotice {
        FreshVersionNotice {
            name: name.to_string(),
            version: version.to_string(),
            registry: "crates-io".to_string(),
            published_at: DateTime::parse_from_rfc3339("2026-04-03T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        }
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
    fn exact_requirement_version_extracts_full_version() {
        assert_eq!(
            exact_requirement_version(&VersionReq::parse("=1.2.3").unwrap()).as_deref(),
            Some("1.2.3")
        );
        assert_eq!(
            exact_requirement_version(&VersionReq::parse("=1.2.3-alpha.1").unwrap()).as_deref(),
            Some("1.2.3-alpha.1")
        );
        assert!(exact_requirement_version(&VersionReq::parse("^1.2.3").unwrap()).is_none());
    }

    #[test]
    fn parse_blockers_handles_cargo_conflict_diagnostics() {
        let stderr = r#"error: failed to select a version for `js-sys`.
    ... required by package `web-sys v0.3.94`
    ... which satisfies dependency `web-sys = "^0.3.66"` (locked to 0.3.94) of package `plotters v0.3.7`
  previously selected package `js-sys v0.3.92`
    ... which satisfies dependency `js-sys = "=0.3.92"` of package `wasm-bindgen-futures v0.4.65`"#;

        let blockers = parse_blockers("", stderr);

        assert!(blockers.contains(&Blocker {
            name: "web-sys".to_string(),
            version: Some("0.3.94".to_string()),
        }));
        assert!(blockers.contains(&Blocker {
            name: "js-sys".to_string(),
            version: Some("0.3.92".to_string()),
        }));
    }

    #[test]
    fn parse_batch_conflict_packages_includes_dependency_chain() {
        let error = r#"error: failed to select a version for `icu_collections`.
    ... required by package `icu_properties v2.1.2`
    ... which satisfies dependency `icu_properties = "^2"` of package `idna_adapter v1.2.1`
    ... which satisfies dependency `idna_adapter = "^1"` of package `idna v1.1.0`
  previously selected package `icu_collections v2.0.0`
    ... which satisfies dependency `icu_collections = "~2.0.0"` of package `icu_normalizer v2.0.1`"#;

        let blockers = parse_batch_conflict_packages(error);
        let labels = blockers.iter().map(Blocker::label).collect::<Vec<_>>();

        assert!(labels.contains(&"icu_properties 2.1.2".to_string()));
        assert!(labels.contains(&"idna_adapter 1.2.1".to_string()));
        assert!(labels.contains(&"idna 1.1.0".to_string()));
        assert!(labels.contains(&"icu_collections 2.0.0".to_string()));
        assert!(labels.contains(&"icu_normalizer 2.0.1".to_string()));
    }

    #[test]
    fn cooldown_batch_validation_budget_scales_with_batch_size() {
        assert_eq!(cooldown_batch_validation_attempt_budget(1), 2);
        assert_eq!(cooldown_batch_validation_attempt_budget(8), 4);
        assert_eq!(cooldown_batch_validation_attempt_budget(67), 7);
        assert_eq!(cooldown_batch_validation_attempt_budget(10_000), 12);
    }

    #[test]
    fn sparse_batch_pruning_only_stops_broad_batches() {
        assert!(!batch_pruning_progress_is_too_low(
            5,
            1,
            cooldown_batch_validation_attempt_budget(5)
        ));
        assert!(!batch_pruning_progress_is_too_low(
            67,
            10,
            cooldown_batch_validation_attempt_budget(67)
        ));
        assert!(batch_pruning_progress_is_too_low(
            67,
            2,
            cooldown_batch_validation_attempt_budget(67)
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
            "manifest_path": "/tmp/crates-io-smoke-workspace/Cargo.toml",
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
    fn record_cargo_compatible_skip_only_reports_new_entries() {
        let mut cargo_compatible_skips = HashMap::new();
        let key = "registry+https://github.com/rust-lang/crates.io-index::demo@1.0.1";

        assert!(record_cargo_compatible_skip(
            &mut cargo_compatible_skips,
            key,
            "reason".to_string()
        ));
        assert!(!record_cargo_compatible_skip(
            &mut cargo_compatible_skips,
            key,
            "reason".to_string()
        ));
        assert!(record_cargo_compatible_skip(
            &mut cargo_compatible_skips,
            key,
            "different reason".to_string()
        ));
    }

    #[test]
    fn refresh_constraint_edges_links_only_current_fresh_exact_dependencies() {
        let source_id = "registry+https://github.com/rust-lang/crates.io-index";
        let parent_id = PackageId {
            repr: format!("{source_id}#parent@1.0.1"),
        };
        let child_id = PackageId {
            repr: format!("{source_id}#child@1.0.1"),
        };
        let unrelated_id = PackageId {
            repr: format!("{source_id}#unrelated@1.0.1"),
        };
        let state = |name: &str| CrateState {
            name: name.to_string(),
            source_id: source_id.to_string(),
            current_version: "1.0.1".to_string(),
            minimum_minutes: 60,
            exact_allowed: false,
            skipped: false,
            baseline_exempt: false,
        };
        let crate_states = HashMap::from([
            (parent_id.clone(), state("parent")),
            (child_id.clone(), state("child")),
            (unrelated_id.clone(), state("unrelated")),
        ]);
        let parent_key = crate_failure_key(source_id, "parent", "1.0.1");
        let child_key = crate_failure_key(source_id, "child", "1.0.1");
        let unrelated_key = crate_failure_key(source_id, "unrelated", "1.0.1");
        let requirement_origins = HashMap::from([(
            child_id.clone(),
            vec![
                RequirementOrigin {
                    parent_id: parent_id.clone(),
                    parent_name: "parent".to_string(),
                    requirement: "=1.0.1".to_string(),
                },
                RequirementOrigin {
                    parent_id: unrelated_id,
                    parent_name: "unrelated".to_string(),
                    requirement: "^1".to_string(),
                },
            ],
        )]);
        let mut constraint_edges =
            HashMap::from([(unrelated_key.clone(), HashSet::from([parent_key.clone()]))]);

        refresh_constraint_edges(
            &mut constraint_edges,
            &crate_states,
            &requirement_origins,
            &HashSet::from([parent_key.clone(), child_key.clone()]),
        );

        assert_eq!(
            constraint_edges.get(&parent_key),
            Some(&HashSet::from([child_key.clone()]))
        );
        assert_eq!(
            constraint_edges.get(&child_key),
            Some(&HashSet::from([parent_key.clone()]))
        );
        assert!(!constraint_edges.contains_key(&unrelated_key));

        refresh_constraint_edges(
            &mut constraint_edges,
            &crate_states,
            &requirement_origins,
            &HashSet::from([child_key]),
        );

        assert!(constraint_edges.is_empty());
    }

    #[test]
    fn format_final_fresh_warning_only_reports_resolver_constrained_versions() {
        let warning = format_final_fresh_warning(
            &FinalFreshReport {
                baseline_fresh: vec![fresh_notice("serde", "1.0.218")],
                resolver_constrained_fresh: vec![fresh_notice("web-sys", "0.3.94")],
            },
            false,
        );

        assert_eq!(
            warning,
            vec![
                "     Warning cooldown finished with fresh versions remaining.".to_string(),
                "resolver-constrained versions that could not be cooled further (review these):"
                    .to_string(),
                "      - web-sys 0.3.94 (published: 2026-04-03T00:00:00Z)".to_string(),
            ]
        );
    }

    #[test]
    fn strict_enforcement_rejects_remaining_resolver_constrained_versions() {
        let report = FinalFreshReport {
            baseline_fresh: vec![fresh_notice("serde", "1.0.218")],
            resolver_constrained_fresh: vec![fresh_notice("web-sys", "0.3.94")],
        };

        let err = enforce_final_report_policy(Enforcement::Strict, &report).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("strict enforcement blocked fresh versions"),
            "{message}"
        );
        assert!(message.contains("web-sys 0.3.94"), "{message}");
        assert!(!message.contains("serde 1.0.218"), "{message}");
    }

    #[test]
    fn cargo_compatible_enforcement_allows_remaining_resolver_constrained_versions() {
        let report = FinalFreshReport {
            baseline_fresh: Vec::new(),
            resolver_constrained_fresh: vec![fresh_notice("web-sys", "0.3.94")],
        };

        assert!(enforce_final_report_policy(Enforcement::CargoCompatible, &report).is_ok());
    }

    #[test]
    fn format_cargo_compatible_acceptance_prompt_includes_publish_dates() {
        let prompt = format_cargo_compatible_acceptance_prompt(
            &FinalFreshReport {
                baseline_fresh: Vec::new(),
                resolver_constrained_fresh: vec![fresh_notice("web-sys", "0.3.94")],
            },
            false,
        );

        assert!(
            prompt.contains("Cargo requires fresh versions that cooldown could not replace."),
            "{prompt}"
        );
        assert!(
            prompt.contains("- web-sys 0.3.94 (published: 2026-04-03T00:00:00Z)"),
            "{prompt}"
        );
    }

    #[test]
    fn format_final_user_summary_lists_cooled_versions_and_remaining_fresh_entries() {
        let summary = format_final_user_summary(
            &[CooledVersionNotice {
                action: CooledVersionAction::Keeping,
                name: "cooldowndep".to_string(),
                from_version: Some("1.0.0".to_string()),
                to_version: Some("1.0.0".to_string()),
                latest_version: Some("1.0.1".to_string()),
                registry: "cool-reg".to_string(),
            }],
            &FinalFreshReport {
                baseline_fresh: vec![fresh_notice("serde", "1.0.218")],
                resolver_constrained_fresh: Vec::new(),
            },
            "dependency graph updated and cooled down",
            false,
        );

        assert!(summary.contains("     Keeping cooldowndep 1.0.0 (latest: v1.0.1) @ cool-reg"));
        assert!(!summary.contains("cooldown finished with fresh versions remaining."));
        assert!(summary.ends_with("    Finished dependency graph updated and cooled down"));
    }

    #[test]
    fn collect_cooled_versions_includes_plain_update_results() {
        let key = InventoryKey {
            name: "demo".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline = BTreeMap::from([(key.clone(), vec!["1.0.0".to_string()])]);
        let start = BTreeMap::from([(key.clone(), vec!["1.1.0".to_string()])]);
        let end = BTreeMap::from([(key, vec!["1.1.0".to_string()])]);

        let notices = collect_cooled_versions(&baseline, &start, &end);

        assert_eq!(
            notices,
            vec![CooledVersionNotice {
                action: CooledVersionAction::Updating,
                name: "demo".to_string(),
                from_version: Some("1.0.0".to_string()),
                to_version: Some("1.1.0".to_string()),
                latest_version: None,
                registry: "crates-io".to_string(),
            }]
        );
    }

    #[test]
    fn collect_cooled_versions_reports_multiple_versions_with_adding_and_removing() {
        let key = InventoryKey {
            name: "redox_syscall".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline =
            BTreeMap::from([(key.clone(), vec!["0.7.0".to_string(), "0.5.13".to_string()])]);
        let start =
            BTreeMap::from([(key.clone(), vec!["0.7.3".to_string(), "0.5.18".to_string()])]);
        let end = BTreeMap::from([(key, vec!["0.7.3".to_string(), "0.5.18".to_string()])]);

        let notices = collect_cooled_versions(&baseline, &start, &end);

        assert_eq!(
            notices,
            vec![
                CooledVersionNotice {
                    action: CooledVersionAction::Removing,
                    name: "redox_syscall".to_string(),
                    from_version: Some("0.5.13".to_string()),
                    to_version: None,
                    latest_version: None,
                    registry: "crates-io".to_string(),
                },
                CooledVersionNotice {
                    action: CooledVersionAction::Removing,
                    name: "redox_syscall".to_string(),
                    from_version: Some("0.7.0".to_string()),
                    to_version: None,
                    latest_version: None,
                    registry: "crates-io".to_string(),
                },
                CooledVersionNotice {
                    action: CooledVersionAction::Adding,
                    name: "redox_syscall".to_string(),
                    from_version: None,
                    to_version: Some("0.5.18".to_string()),
                    latest_version: None,
                    registry: "crates-io".to_string(),
                },
                CooledVersionNotice {
                    action: CooledVersionAction::Adding,
                    name: "redox_syscall".to_string(),
                    from_version: None,
                    to_version: Some("0.7.3".to_string()),
                    latest_version: None,
                    registry: "crates-io".to_string(),
                },
            ]
        );
    }

    #[test]
    fn collect_cooled_versions_matches_plain_update_and_multi_version_changes_together() {
        let addr2line = InventoryKey {
            name: "addr2line".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let redox = InventoryKey {
            name: "redox_syscall".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline = BTreeMap::from([
            (addr2line.clone(), vec!["0.24.2".to_string()]),
            (
                redox.clone(),
                vec!["0.7.0".to_string(), "0.5.13".to_string()],
            ),
        ]);
        let start = BTreeMap::from([
            (addr2line.clone(), vec!["0.25.1".to_string()]),
            (
                redox.clone(),
                vec!["0.7.3".to_string(), "0.5.18".to_string()],
            ),
        ]);
        let end = BTreeMap::from([
            (addr2line, vec!["0.25.1".to_string()]),
            (redox, vec!["0.7.3".to_string(), "0.5.18".to_string()]),
        ]);

        let notices = collect_cooled_versions(&baseline, &start, &end);
        let rendered = notices
            .iter()
            .map(|entry| format_cooled_version_notice(entry, false))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "    Updating addr2line v0.24.2 -> v0.25.1".to_string(),
                "    Removing redox_syscall v0.5.13".to_string(),
                "    Removing redox_syscall v0.7.0".to_string(),
                "      Adding redox_syscall v0.5.18".to_string(),
                "      Adding redox_syscall v0.7.3".to_string(),
            ]
        );
    }

    #[test]
    fn collect_cooled_versions_can_keep_repeated_versions_with_latest_annotation() {
        let key = InventoryKey {
            name: "redox_syscall".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline =
            BTreeMap::from([(key.clone(), vec!["0.7.0".to_string(), "0.5.13".to_string()])]);
        let start =
            BTreeMap::from([(key.clone(), vec!["0.7.3".to_string(), "0.5.18".to_string()])]);
        let end = BTreeMap::from([(key, vec!["0.7.0".to_string(), "0.5.13".to_string()])]);

        let notices = collect_cooled_versions(&baseline, &start, &end);

        assert_eq!(
            notices,
            vec![
                CooledVersionNotice {
                    action: CooledVersionAction::Keeping,
                    name: "redox_syscall".to_string(),
                    from_version: Some("0.5.13".to_string()),
                    to_version: Some("0.5.13".to_string()),
                    latest_version: Some("0.5.18".to_string()),
                    registry: "crates-io".to_string(),
                },
                CooledVersionNotice {
                    action: CooledVersionAction::Keeping,
                    name: "redox_syscall".to_string(),
                    from_version: Some("0.7.0".to_string()),
                    to_version: Some("0.7.0".to_string()),
                    latest_version: Some("0.7.3".to_string()),
                    registry: "crates-io".to_string(),
                },
            ]
        );
    }

    #[test]
    fn collect_cooled_versions_keeps_preserved_version_after_multi_version_removals() {
        let key = InventoryKey {
            name: "windows-targets".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline = BTreeMap::from([(
            key.clone(),
            vec![
                "0.53.0".to_string(),
                "0.52.6".to_string(),
                "0.48.5".to_string(),
            ],
        )]);
        let start = baseline.clone();
        let end = BTreeMap::from([(key, vec!["0.52.6".to_string()])]);

        let notices = collect_cooled_versions(&baseline, &start, &end);
        let rendered = notices
            .iter()
            .map(|entry| format_cooled_version_notice(entry, false))
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "    Removing windows-targets v0.48.5".to_string(),
                "    Removing windows-targets v0.53.0".to_string(),
                "     Keeping windows-targets 0.52.6".to_string(),
            ]
        );
    }

    #[test]
    fn collect_cooled_versions_uses_direct_transition_for_single_version_crates() {
        let key = InventoryKey {
            name: "demo".to_string(),
            registry_id: "https://github.com/rust-lang/crates.io-index".to_string(),
            registry: "crates-io".to_string(),
        };
        let baseline = BTreeMap::from([(key.clone(), vec!["0.7.0".to_string()])]);
        let start = BTreeMap::from([(key.clone(), vec!["0.5.18".to_string()])]);
        let end = BTreeMap::from([(key, vec!["0.5.18".to_string()])]);

        let notices = collect_cooled_versions(&baseline, &start, &end);

        assert_eq!(
            notices,
            vec![CooledVersionNotice {
                action: CooledVersionAction::Downgrading,
                name: "demo".to_string(),
                from_version: Some("0.7.0".to_string()),
                to_version: Some("0.5.18".to_string()),
                latest_version: None,
                registry: "crates-io".to_string(),
            }]
        );
    }

    #[test]
    fn search_coordinated_assignment_picks_a_consistent_bundle() {
        let shared_id: PackageId = serde_json::from_value(json!(
            "registry+https://github.com/rust-lang/crates.io-index#sharedshim@1.1.0"
        ))
        .unwrap();
        let web_id: PackageId = serde_json::from_value(json!(
            "registry+https://github.com/rust-lang/crates.io-index#webshim@1.1.0"
        ))
        .unwrap();
        let future_id: PackageId = serde_json::from_value(json!(
            "registry+https://github.com/rust-lang/crates.io-index#futureshim@1.1.0"
        ))
        .unwrap();
        let plans = vec![
            BundleMemberPlan {
                fresh: FreshCrate {
                    package_id: web_id,
                    name: "webshim".to_string(),
                    source_id: "registry+https://github.com/rust-lang/crates.io-index".to_string(),
                    current_version: "1.1.0".to_string(),
                    minimum_minutes: 60,
                },
                candidates: vec![
                    BundleCandidate {
                        version: "1.0.0".to_string(),
                        parsed_version: Version::parse("1.0.0").unwrap(),
                        internal_requirements: BTreeMap::from([(
                            "sharedshim".to_string(),
                            VersionReq::parse("=1.0.0").unwrap(),
                        )]),
                    },
                    BundleCandidate {
                        version: "1.0.1".to_string(),
                        parsed_version: Version::parse("1.0.1").unwrap(),
                        internal_requirements: BTreeMap::from([(
                            "sharedshim".to_string(),
                            VersionReq::parse("=1.0.1").unwrap(),
                        )]),
                    },
                ],
            },
            BundleMemberPlan {
                fresh: FreshCrate {
                    package_id: future_id,
                    name: "futureshim".to_string(),
                    source_id: "registry+https://github.com/rust-lang/crates.io-index".to_string(),
                    current_version: "1.1.0".to_string(),
                    minimum_minutes: 60,
                },
                candidates: vec![BundleCandidate {
                    version: "1.0.0".to_string(),
                    parsed_version: Version::parse("1.0.0").unwrap(),
                    internal_requirements: BTreeMap::from([(
                        "sharedshim".to_string(),
                        VersionReq::parse("=1.0.0").unwrap(),
                    )]),
                }],
            },
            BundleMemberPlan {
                fresh: FreshCrate {
                    package_id: shared_id,
                    name: "sharedshim".to_string(),
                    source_id: "registry+https://github.com/rust-lang/crates.io-index".to_string(),
                    current_version: "1.1.0".to_string(),
                    minimum_minutes: 60,
                },
                candidates: vec![
                    BundleCandidate {
                        version: "1.0.0".to_string(),
                        parsed_version: Version::parse("1.0.0").unwrap(),
                        internal_requirements: BTreeMap::new(),
                    },
                    BundleCandidate {
                        version: "1.0.1".to_string(),
                        parsed_version: Version::parse("1.0.1").unwrap(),
                        internal_requirements: BTreeMap::new(),
                    },
                ],
            },
        ];
        let mut assignment = BTreeMap::new();
        let mut budget = MAX_COORDINATED_ASSIGNMENTS;

        assert!(search_coordinated_assignment(
            &plans,
            0,
            &mut assignment,
            &mut budget
        ));
        assert_eq!(assignment["webshim"].version, "1.0.0");
        assert_eq!(assignment["futureshim"].version, "1.0.0");
        assert_eq!(assignment["sharedshim"].version, "1.0.0");
    }

    #[test]
    fn config_fixture_remains_constructible_for_executor_tests() {
        let config = Config {
            cooldown_minutes: 60,
            enforcement: Enforcement::Strict,
            cargo_compatible_accept: CargoCompatibleAccept::Prompt,
            lockfile_baseline: LockfileBaselineMode::Floor,
            now_override: None,
            ttl_seconds: 60,
            cache_dir: None,
            http_retries: 0,
            verbose: false,
            skip_registries: Vec::new(),
            allow_rules: AllowRules::default(),
        };

        assert_eq!(config.cooldown_minutes, 60);
    }
}
