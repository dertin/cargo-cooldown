//! End-to-end cooldown tests using a deterministic local sparse registry.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::write::GzEncoder;
use sha2::{Digest, Sha256};
use tame_index::KrateName;
use tar::Builder;
use tempfile::{TempDir, tempdir};

const CRATE_NAME: &str = "cooldowndep";
const OLD_VERSION: &str = "1.0.0";
const FRESH_VERSION: &str = "1.0.1";
const FRESHER_VERSION: &str = "1.0.2";
const OLD_PUBTIME: &str = "2026-03-01T00:00:00Z";
const FRESH_PUBTIME: &str = "2026-04-02T12:00:00Z";
const FRESHER_PUBTIME: &str = "2026-04-02T18:00:00Z";
const NOW: &str = "2026-04-03T00:00:00Z";
const COOLDOWN_MINUTES: &str = "1440";
const REGISTRY_NAME: &str = "cool-reg";
const LOCKFILE_BASELINE_IGNORE: (&str, &str) = ("COOLDOWN_LOCKFILE_BASELINE", "ignore");
const CHAIN_A_NAME: &str = "chaina";
const CHAIN_A_OLD_VERSION: &str = "1.2.2";
const CHAIN_A_FRESH_VERSION: &str = "1.2.3";
const CHAIN_A_OLD_PUBTIME: &str = "2026-03-01T00:00:00Z";
const CHAIN_A_FRESH_PUBTIME: &str = "2026-04-02T12:00:00Z";
const CHAIN_B_NAME: &str = "chainb";
const CHAIN_B_OLD_VERSION: &str = "2.3.3";
const CHAIN_B_UPDATED_VERSION: &str = "2.3.4";
const CHAIN_B_OLD_PUBTIME: &str = "2026-03-01T00:00:00Z";
const CHAIN_B_UPDATED_PUBTIME: &str = "2026-03-15T00:00:00Z";
const BUNDLE_A_NAME: &str = "webshim";
const BUNDLE_B_NAME: &str = "futureshim";
const BUNDLE_SHARED_NAME: &str = "sharedshim";
const BUNDLE_OLD_VERSION: &str = "1.0.0";
const BUNDLE_FRESH_VERSION: &str = "1.1.0";
const BUNDLE_OLD_PUBTIME: &str = "2026-03-01T00:00:00Z";
const BUNDLE_FRESH_PUBTIME: &str = "2026-04-02T12:00:00Z";
const BACKTRACK_LEFT_NAME: &str = "backtrackleft";
const BACKTRACK_RIGHT_NAME: &str = "backtrackright";
const BACKTRACK_SHARED_NAME: &str = "backtrackshared";
const BACKTRACK_OLD_VERSION: &str = "1.0.0";
const BACKTRACK_COMPAT_VERSION: &str = "1.1.0";
const BACKTRACK_CONFLICT_VERSION: &str = "1.2.0";
const BACKTRACK_FRESH_VERSION: &str = "1.3.0";
const DUP_ROOT_A_NAME: &str = "dupuserone";
const DUP_ROOT_B_NAME: &str = "dupusertwo";
const DUP_SHARED_NAME: &str = "dupshared";
const DUP_V1_OLD_VERSION: &str = "1.0.0";
const DUP_V1_FRESH_VERSION: &str = "1.0.1";
const DUP_V2_OLD_VERSION: &str = "2.0.0";
const DUP_V2_FRESH_VERSION: &str = "2.0.1";
const DUP_PARENT_OLD_VERSION: &str = "1.0.0";
const DUP_PARENT_FRESH_VERSION: &str = "1.1.0";
const DUP_TRANSITIVE_CURRENT_PUBTIME: &str = "2026-03-15T00:00:00Z";
const BASELINE_FLOOR_NAME: &str = "baselinefloor";
const BASELINE_USER_NAME: &str = "baselineuser";
const SCOPED_CONFLICT_NAME: &str = "scopedfresh";
const SCOPED_MEMBER_A: &str = "member-a";
const SCOPED_MEMBER_B: &str = "member-b";
const BENCHMARK_CRATE_COUNT: usize = 24;

#[test]
fn existing_lockfile_fresh_dependency_is_ignored_by_default() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    harness.server.reset_counts();
    let output = harness.run_cooldown(&[("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "cooldown should leave unchanged baseline dependencies alone: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    assert_eq!(harness.server.api_hits(), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("cooldown: inspected"),
        "default lockfile baseline should not inspect unchanged baseline versions: {stderr}"
    );
    assert!(
        !stderr.contains("cooldown finished with fresh versions remaining."),
        "baseline-fresh versions from the initial lockfile should not trigger a warning: {stderr}"
    );
}

#[test]
fn guard_commands_cool_current_lockfile_when_baseline_ignore_is_enabled() {
    for command in ["check", "build", "test", "run"] {
        let mut harness =
            TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
        harness.generate_lockfile();
        assert_eq!(harness.locked_version(), FRESH_VERSION);

        let output = harness.run_command(&[command], &[LOCKFILE_BASELINE_IGNORE]);
        assert!(
            output.status.success(),
            "cargo cooldown {command} should cool the current lockfile before forwarding to Cargo: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            harness.locked_version(),
            OLD_VERSION,
            "cargo cooldown {command} should leave the consumed lockfile cooled"
        );

        if command == "run" {
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains(OLD_VERSION),
                "run should execute the cooled dependency version: {stdout}"
            );
            assert!(
                !stdout.contains(FRESH_VERSION),
                "run should not execute the fresh dependency version after cooldown: {stdout}"
            );
        }
    }
}

#[test]
fn uses_index_pubtime_without_hitting_api() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    harness.server.reset_counts();
    let output = harness.run_cooldown(&[LOCKFILE_BASELINE_IGNORE, ("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "cooldown should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
    assert_eq!(harness.server.api_hits(), 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stderr}");
    assert!(
        stderr.contains("release_time_source=index_pubtime"),
        "expected verbose logs to show local pubtime usage: {stderr}"
    );
}

#[test]
fn fills_missing_pubtime_via_fallback_api() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeWithApi).expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    harness.server.reset_counts();
    let output = harness.run_cooldown(&[LOCKFILE_BASELINE_IGNORE, ("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "cooldown should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
    assert!(harness.server.api_hits() > 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stderr}");
    assert!(
        stderr.contains("release_time_source=registry_api_fallback"),
        "expected verbose logs to show HTTP fallback usage: {stderr}"
    );
}

#[test]
fn fails_closed_when_registry_lacks_release_time_metadata() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[LOCKFILE_BASELINE_IGNORE]);
    assert!(!output.status.success(), "cooldown should fail closed");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("missing release timestamp"), "{stderr}");
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn cargo_compatible_enforcement_continues_when_registry_lacks_release_time_metadata() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[
        LOCKFILE_BASELINE_IGNORE,
        ("COOLDOWN_ENFORCEMENT", "cargo_compatible"),
        ("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", "auto"),
    ]);
    assert!(
        output.status.success(),
        "cargo_compatible enforcement should continue: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn cargo_compatible_update_keeps_cargo_updated_lockfile_when_metadata_is_missing() {
    let mut harness = TestHarness::new_with_dependency_req(
        RegistryMode::MissingPubtimeNoApi,
        &format!("={OLD_VERSION}"),
    )
    .expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), OLD_VERSION);

    harness.set_dependency_requirement("1");
    let output = harness.run_command(
        &["update"],
        &[
            ("COOLDOWN_ENFORCEMENT", "cargo_compatible"),
            ("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", "auto"),
        ],
    );
    assert!(
        output.status.success(),
        "cargo_compatible update should keep Cargo's updated lockfile when cooldown metadata is missing: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn skips_registry_from_start_by_name() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[
        LOCKFILE_BASELINE_IGNORE,
        ("COOLDOWN_SKIP_REGISTRIES", REGISTRY_NAME),
    ]);
    assert!(
        output.status.success(),
        "skipped registry should be ignored: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn skips_registry_from_start_by_effective_url() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();
    let skip_value = format!("sparse+{}/index/", harness.server.base_url());

    let output = harness.run_cooldown(&[
        LOCKFILE_BASELINE_IGNORE,
        ("COOLDOWN_SKIP_REGISTRIES", &skip_value),
    ]);
    assert!(
        output.status.success(),
        "skipped registry should be ignored: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn enforcement_off_skips_cooldown_checks_entirely() {
    let mut harness =
        TestHarness::new(RegistryMode::MissingPubtimeNoApi).expect("harness should build");
    harness.generate_lockfile();
    harness.server.reset_counts();

    let output = harness.run_cooldown(&[LOCKFILE_BASELINE_IGNORE, ("COOLDOWN_ENFORCEMENT", "off")]);
    assert!(
        output.status.success(),
        "enforcement=off should bypass cooldown: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    assert_eq!(harness.server.api_hits(), 0);
}

#[test]
fn generates_lockfile_before_running_cooldown() {
    let harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    assert!(
        !harness.workspace_dir.join("Cargo.lock").exists(),
        "fixture should start without lockfile"
    );

    let output = harness.run_cooldown(&[]);
    assert!(
        output.status.success(),
        "cooldown should generate a lockfile and continue: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn guard_command_cools_dependency_added_by_manifest_change() {
    let mut harness = TestHarness::new_without_dependencies(RegistryMode::PubtimeOnly)
        .expect("harness should build");
    harness.generate_lockfile();
    let initial_lockfile = fs::read_to_string(harness.workspace_dir.join("Cargo.lock"))
        .expect("initial lockfile should be readable");
    assert!(
        parse_lockfile_version(&initial_lockfile, CRATE_NAME).is_none(),
        "fixture should start with a lockfile that does not contain {CRATE_NAME}"
    );

    harness.set_dependency_requirement("1");
    let output = harness.run_cooldown(&[]);

    assert!(
        output.status.success(),
        "cooldown should cool a dependency introduced by a manifest change before Cargo consumes it: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn failed_fresh_update_restores_initial_lockfile_and_transitive_versions() {
    let mut harness = DependencyChainHarness::new().expect("chain harness should build");
    harness.generate_lockfile();
    let baseline_lockfile = harness.lockfile_contents();
    assert_eq!(harness.locked_version(CHAIN_A_NAME), CHAIN_A_OLD_VERSION);
    assert_eq!(harness.locked_version(CHAIN_B_NAME), CHAIN_B_OLD_VERSION);

    harness.request_exact_version(CHAIN_A_FRESH_VERSION);
    let output = harness.run_cooldown(&[]);
    assert!(
        !output.status.success(),
        "fresh exact updates should fail and restore the baseline lockfile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.lockfile_contents(), baseline_lockfile);
    assert_eq!(harness.locked_version(CHAIN_A_NAME), CHAIN_A_OLD_VERSION);
    assert_eq!(harness.locked_version(CHAIN_B_NAME), CHAIN_B_OLD_VERSION);
}

#[test]
fn honors_manifest_path_in_cargo_style_order_from_external_cwd() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let runner_dir = harness.runner_dir();
    let manifest_path = harness.workspace_dir.join("Cargo.toml");
    let manifest_path = manifest_path.to_string_lossy().to_string();

    let output = harness.run_command_in(
        &runner_dir,
        &["check", "--manifest-path", manifest_path.as_str()],
        &[LOCKFILE_BASELINE_IGNORE],
    );
    assert!(
        output.status.success(),
        "manifest-path invocation should succeed from another cwd: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn honors_manifest_path_before_subcommand_from_external_cwd() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let runner_dir = harness.runner_dir();
    let manifest_path = harness.workspace_dir.join("Cargo.toml");
    let manifest_path = manifest_path.to_string_lossy().to_string();

    let output = harness.run_command_in(
        &runner_dir,
        &["--manifest-path", manifest_path.as_str(), "check"],
        &[LOCKFILE_BASELINE_IGNORE],
    );
    assert!(
        output.status.success(),
        "manifest-path before subcommand should still cool the external project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
}

#[test]
fn workspace_member_manifest_reuses_workspace_root_lockfile() {
    let harness = WorkspaceMemberHarness::new().expect("workspace member harness should build");
    harness.generate_lockfile();
    assert!(harness.workspace_lockfile().exists());
    assert!(
        !harness.member_lockfile().exists(),
        "workspace members should not own a separate lockfile"
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "workspace member invocation should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cargo_log = harness.cargo_log();
    assert!(
        !cargo_log
            .iter()
            .any(|line| line.contains("generate-lockfile")),
        "existing workspace Cargo.lock should prevent redundant cargo generate-lockfile runs: {cargo_log:#?}"
    );
}

#[test]
fn workspace_member_manifest_generates_workspace_root_lockfile_when_missing() {
    let harness = WorkspaceMemberHarness::new().expect("workspace member harness should build");
    assert!(
        !harness.workspace_lockfile().exists(),
        "fixture should start without a workspace lockfile"
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "workspace member invocation should generate the shared lockfile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        harness.workspace_lockfile().exists(),
        "cargo generate-lockfile should create Cargo.lock at the workspace root"
    );

    let cargo_log = harness.cargo_log();
    let generate_count = cargo_log
        .iter()
        .filter(|line| line.contains("generate-lockfile"))
        .count();
    assert_eq!(
        generate_count, 1,
        "missing workspace Cargo.lock should trigger exactly one cargo generate-lockfile run: {cargo_log:#?}"
    );
}

#[test]
fn exact_allow_rule_keeps_fresh_version_pinned() {
    let mut harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    harness.generate_lockfile();
    let config = harness.workspace_dir.join("cooldown.toml");
    fs::write(
        &config,
        format!(
            "cooldown_minutes = 1440\nlockfile_baseline = \"ignore\"\n\n[[allow.exact]]\ncrate = \"{CRATE_NAME}\"\nversion = \"{FRESH_VERSION}\"\n"
        ),
    )
    .expect("config should be writable");

    let output = harness.run_cooldown(&[]);
    assert!(
        output.status.success(),
        "exact allow rule should bypass cooldown for the pinned version: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn cooldown_update_keeps_existing_baseline_versions_with_floor_baseline() {
    let harness = TestHarness::new(RegistryMode::PubtimeOnly).expect("harness should build");
    let mut harness = harness;
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    let output = harness.run_command(&["update"], &[]);

    assert!(
        output.status.success(),
        "cargo cooldown update should keep unchanged baseline versions: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
}

#[test]
fn cooldown_update_repins_new_fresh_versions_against_pre_update_baseline() {
    let mut harness =
        TestHarness::new_with_dependency_req(RegistryMode::PubtimeOnly, &format!("={OLD_VERSION}"))
            .expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), OLD_VERSION);
    harness.set_dependency_requirement("1");

    let output = harness.run_command(&["update"], &[("COOLDOWN_VERBOSE", "true")]);

    assert!(
        output.status.success(),
        "cargo cooldown update should cool the updated lockfile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected_cooldown_line = format!(
        "     Keeping cooldowndep 1.0.0 (latest: v1.0.1) @ sparse+{}/index/",
        harness.server.base_url()
    );
    assert!(
        stderr.contains("cooldown: inspected crate=cooldowndep version=1.0.1"),
        "{stderr}"
    );
    assert!(
        stderr.contains("cooldown: scan_summary registry_packages=1 inspected=1 fresh=1"),
        "{stderr}"
    );
    assert!(stderr.contains(&expected_cooldown_line), "{stderr}");
    assert!(
        stderr.contains("    Finished dependency graph updated and cooled down"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Updating `cool-reg` index"),
        "the initial cargo update output should stay hidden on success: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn cooldown_update_holds_real_lockfile_and_uses_temp_workspace() {
    let mut harness =
        TestHarness::new_with_dependency_req(RegistryMode::PubtimeOnly, &format!("={OLD_VERSION}"))
            .expect("harness should build");
    harness.generate_lockfile();
    harness.set_dependency_requirement("1");

    let wrapper_dir = harness.temp_root.join("hold-wrapper-bin");
    let wrapper_path = wrapper_dir.join(wrapper_binary_name());
    let wrapper_log = harness.temp_root.join("hold-wrapper.log");
    fs::create_dir_all(&wrapper_dir).expect("wrapper dir should exist");
    write_hold_asserting_cargo_wrapper(&wrapper_path, &wrapper_log)
        .expect("wrapper should be writable");
    let path_with_wrapper = prepend_to_path(&wrapper_dir).expect("PATH should be buildable");
    let runner_dir = harness.runner_dir();
    let manifest_path = harness.workspace_dir.join("Cargo.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .args([
            "update",
            "--manifest-path",
            manifest_path.to_string_lossy().as_ref(),
        ])
        .current_dir(&runner_dir)
        .env("CARGO_HOME", &harness.cargo_home)
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .env("COOLDOWN_NOW", NOW)
        .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
        .env("COOLDOWN_HTTP_RETRIES", "0")
        .env("COOLDOWN_VERBOSE", "true")
        .env("COOLDOWN_EXPECT_HELD_WORKSPACE", &harness.workspace_dir)
        .env("PATH", &path_with_wrapper)
        .output()
        .expect("cargo-cooldown should run");

    assert!(
        output.status.success(),
        "isolated update should succeed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(&wrapper_log).unwrap_or_default()
    );
    assert_eq!(harness.locked_version(), OLD_VERSION);

    let wrapper_log = fs::read_to_string(&wrapper_log).expect("wrapper log should exist");
    assert!(
        wrapper_log.contains("update-held-lockfile"),
        "internal cargo update should see the real Cargo.lock held: {wrapper_log}"
    );
    assert!(
        wrapper_log.contains("update-used-temp-workspace"),
        "internal cargo update should run from the temp workspace: {wrapper_log}"
    );
    assert!(
        wrapper_log.contains("update-rewrote-manifest-path"),
        "internal cargo update should receive the temp manifest path: {wrapper_log}"
    );
    assert!(
        fs::read_dir(&harness.workspace_dir)
            .expect("workspace should be readable")
            .all(|entry| !entry
                .expect("entry should be readable")
                .file_name()
                .to_string_lossy()
                .starts_with("Cargo.lock.cooldown-backup.")),
        "lockfile backup should be cleaned after publishing"
    );
}

#[test]
fn cooldown_update_reports_plain_lockfile_updates_when_no_cooling_is_needed() {
    let mut harness =
        TestHarness::new_with_dependency_req(RegistryMode::PubtimeOnly, &format!("={OLD_VERSION}"))
            .expect("harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), OLD_VERSION);
    harness.set_dependency_requirement("1");

    let output = harness.run_command(&["update"], &[("COOLDOWN_MINUTES", "1")]);

    assert!(
        output.status.success(),
        "cargo cooldown update should keep plain update results when nothing is fresh enough to cool: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected_update_line = format!(
        "    Updating cooldowndep v1.0.0 -> v1.0.1 @ sparse+{}/index/",
        harness.server.base_url()
    );
    assert!(stderr.contains(&expected_update_line), "{stderr}");
    assert!(
        stderr.contains("    Finished dependency graph updated and cooled down"),
        "{stderr}"
    );
}

#[test]
fn cooldown_update_can_restore_a_fresh_baseline_version() {
    let temp_dir = tempdir().expect("tempdir should build");
    let temp_root = temp_dir.path().to_path_buf();
    let cargo_home = temp_root.join("cargo-home");
    let workspace_dir = temp_root.join("workspace");
    let server = RegistryServer::with_crates(
        vec![PublishedCrate::new(
            CRATE_NAME,
            vec![
                PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                PackageVersion::new(FRESHER_VERSION, Some(FRESHER_PUBTIME), false),
            ],
        )],
        false,
    )
    .expect("registry should build");

    fs::create_dir_all(&cargo_home).expect("cargo home should exist");
    create_workspace_with_dependency(
        &workspace_dir,
        &server,
        CRATE_NAME,
        &format!("={FRESH_VERSION}"),
    )
    .expect("workspace should build");
    write_registry_config(&cargo_home, &server).expect("registry config should write");

    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&workspace_dir)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .output()
        .expect("cargo generate-lockfile should run");
    assert!(
        output.status.success(),
        "lockfile generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        parse_lockfile_version(
            &fs::read_to_string(workspace_dir.join("Cargo.lock")).expect("lockfile should exist"),
            CRATE_NAME,
        )
        .expect("crate should exist in lockfile"),
        FRESH_VERSION
    );

    write_root_manifest(&workspace_dir, &[(CRATE_NAME, "1")]).expect("manifest should update");
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("update")
        .current_dir(&workspace_dir)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .env("COOLDOWN_NOW", NOW)
        .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
        .env("COOLDOWN_HTTP_RETRIES", "0")
        .output()
        .expect("cargo-cooldown should run");

    assert!(
        output.status.success(),
        "cargo cooldown update should restore the baseline version even when it is still fresh: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        parse_lockfile_version(
            &fs::read_to_string(workspace_dir.join("Cargo.lock")).expect("lockfile should exist"),
            CRATE_NAME,
        )
        .expect("crate should exist in lockfile"),
        FRESH_VERSION
    );
}

#[test]
fn cooldown_update_does_not_downgrade_existing_fresh_lockfile_versions() {
    assert_cooldown_update_with_existing_fresh_lockfile_version(&[], false);
}

#[test]
fn cooldown_update_ignore_baseline_can_cool_existing_lockfile_versions() {
    assert_cooldown_update_with_existing_fresh_lockfile_version(&[LOCKFILE_BASELINE_IGNORE], true);
}

fn assert_cooldown_update_with_existing_fresh_lockfile_version(
    extra_env: &[(&str, &str)],
    expect_success: bool,
) {
    let temp_dir = tempdir().expect("tempdir should build");
    let temp_root = temp_dir.path().to_path_buf();
    let cargo_home = temp_root.join("cargo-home");
    let workspace_dir = temp_root.join("workspace");
    let server = RegistryServer::with_crates(
        vec![
            PublishedCrate::new(
                BASELINE_FLOOR_NAME,
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
            PublishedCrate::new(
                BASELINE_USER_NAME,
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false).with_dependencies(
                        vec![RegistryDependency::exact(BASELINE_FLOOR_NAME, OLD_VERSION)],
                    ),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false)
                        .with_dependencies(vec![RegistryDependency::exact(
                            BASELINE_FLOOR_NAME,
                            FRESH_VERSION,
                        )]),
                ],
            ),
        ],
        false,
    )
    .expect("registry should build");

    fs::create_dir_all(&cargo_home).expect("cargo home should exist");
    create_workspace_with_dependency(
        &workspace_dir,
        &server,
        BASELINE_FLOOR_NAME,
        &format!("={FRESH_VERSION}"),
    )
    .expect("workspace should build");
    write_registry_config(&cargo_home, &server).expect("registry config should write");

    let output = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(&workspace_dir)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .output()
        .expect("cargo generate-lockfile should run");
    assert!(
        output.status.success(),
        "lockfile generation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let baseline_lockfile =
        fs::read_to_string(workspace_dir.join("Cargo.lock")).expect("lockfile should exist");
    assert_eq!(
        parse_lockfile_version(&baseline_lockfile, BASELINE_FLOOR_NAME).as_deref(),
        Some(FRESH_VERSION)
    );
    assert!(
        parse_lockfile_version(&baseline_lockfile, BASELINE_USER_NAME).is_none(),
        "new dependency should not exist in the baseline lockfile"
    );

    write_root_manifest(
        &workspace_dir,
        &[(BASELINE_FLOOR_NAME, "1"), (BASELINE_USER_NAME, "1")],
    )
    .expect("manifest should update");

    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
    command
        .arg("update")
        .current_dir(&workspace_dir)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TERM_PROGRESS_WHEN", "never")
        .env("COOLDOWN_NOW", NOW)
        .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
        .env("COOLDOWN_HTTP_RETRIES", "0")
        .env("COOLDOWN_VERBOSE", "true");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = command.output().expect("cargo-cooldown should run");

    let final_lockfile =
        fs::read_to_string(workspace_dir.join("Cargo.lock")).expect("lockfile should exist");
    if expect_success {
        assert!(
            output.status.success(),
            "ignore baseline should cool both existing and newly introduced fresh versions: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            parse_lockfile_version(&final_lockfile, BASELINE_FLOOR_NAME).as_deref(),
            Some(OLD_VERSION)
        );
        assert_eq!(
            parse_lockfile_version(&final_lockfile, BASELINE_USER_NAME).as_deref(),
            Some(OLD_VERSION)
        );
    } else {
        assert!(
            !output.status.success(),
            "strict cooldown should reject adding a fresh dependency when the only cool candidate downgrades an existing lockfile version: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(final_lockfile, baseline_lockfile);
        assert_eq!(
            parse_lockfile_version(&final_lockfile, BASELINE_FLOOR_NAME).as_deref(),
            Some(FRESH_VERSION)
        );
        assert!(
            parse_lockfile_version(&final_lockfile, BASELINE_USER_NAME).is_none(),
            "failed strict update should restore the pre-update lockfile"
        );
    }
}

#[test]
fn coordinated_bundle_resolution_cools_exactly_coupled_transitives() {
    let mut harness = CoordinatedBundleHarness::new().expect("bundle harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(BUNDLE_A_NAME), BUNDLE_FRESH_VERSION);
    assert_eq!(harness.locked_version(BUNDLE_B_NAME), BUNDLE_FRESH_VERSION);
    assert_eq!(
        harness.locked_version(BUNDLE_SHARED_NAME),
        BUNDLE_FRESH_VERSION
    );

    let output = harness.run_cooldown(&[LOCKFILE_BASELINE_IGNORE, ("COOLDOWN_VERBOSE", "true")]);
    assert!(
        output.status.success(),
        "coordinated bundle resolution should cool the coupled transitive crates: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(BUNDLE_A_NAME), BUNDLE_OLD_VERSION);
    assert_eq!(harness.locked_version(BUNDLE_B_NAME), BUNDLE_OLD_VERSION);
    assert_eq!(
        harness.locked_version(BUNDLE_SHARED_NAME),
        BUNDLE_OLD_VERSION
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("attempting cooldown batch solver")
            || stderr.contains("coordinated bundle resolution succeeded"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("resolver-constrained versions that could not be cooled further"),
        "{stderr}"
    );
}

#[test]
fn batch_solver_cools_independent_crates_without_per_crate_precise_updates() {
    let mut harness = MultiPassBenchmarkHarness::new(BENCHMARK_CRATE_COUNT)
        .expect("benchmark harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[]);
    assert!(
        output.status.success(),
        "batch solver run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lockfile = harness.lockfile_contents();
    for index in 0..BENCHMARK_CRATE_COUNT {
        let crate_name = benchmark_crate_name(index);
        assert_eq!(
            parse_lockfile_version(&lockfile, &crate_name).as_deref(),
            Some(OLD_VERSION),
            "{crate_name} should be cooled in the final lockfile"
        );
    }

    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "independent fresh crates should be cooled by one verified lockfile batch, not one cargo update --precise per crate"
    );
}

#[test]
fn batch_solver_cools_single_crate_without_per_crate_precise_update() {
    let mut harness = MultiPassBenchmarkHarness::new(1).expect("single-crate harness should build");
    harness.generate_lockfile();

    let output = harness.run_cooldown(&[]);
    assert!(
        output.status.success(),
        "batch solver run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let lockfile = harness.lockfile_contents();
    assert_eq!(
        parse_lockfile_version(&lockfile, &benchmark_crate_name(0)).as_deref(),
        Some(OLD_VERSION),
        "single fresh crate should be cooled in the final lockfile"
    );

    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "a single locally valid fresh crate should be cooled by one verified lockfile assignment, not cargo update --precise"
    );
}

#[test]
fn batch_solver_backtracks_internal_dependency_candidates_locally() {
    let mut harness = BacktrackingBundleHarness::new().expect("backtracking harness should build");
    harness.generate_lockfile();
    assert_eq!(
        harness.locked_version(BACKTRACK_LEFT_NAME),
        BACKTRACK_FRESH_VERSION
    );
    assert_eq!(
        harness.locked_version(BACKTRACK_RIGHT_NAME),
        BACKTRACK_FRESH_VERSION
    );
    assert_eq!(
        harness.locked_version(BACKTRACK_SHARED_NAME),
        BACKTRACK_COMPAT_VERSION
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should backtrack internal exact dependency candidates: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.locked_version(BACKTRACK_LEFT_NAME),
        BACKTRACK_COMPAT_VERSION
    );
    assert_eq!(
        harness.locked_version(BACKTRACK_RIGHT_NAME),
        BACKTRACK_CONFLICT_VERSION
    );
    assert_eq!(
        harness.locked_version(BACKTRACK_SHARED_NAME),
        BACKTRACK_OLD_VERSION
    );

    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "compatible local component solving should avoid per-crate cargo update --precise calls"
    );
}

#[test]
fn batch_solver_batches_duplicate_package_names_without_precise_updates() {
    let mut harness =
        DuplicateNameBatchHarness::new().expect("duplicate-name harness should build");
    harness.generate_lockfile();
    assert_eq!(
        sorted_lockfile_versions(&harness.lockfile_contents(), DUP_SHARED_NAME),
        vec![
            DUP_V1_FRESH_VERSION.to_string(),
            DUP_V2_FRESH_VERSION.to_string()
        ]
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool duplicate-name packages via one validated batch: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        sorted_lockfile_versions(&harness.lockfile_contents(), DUP_SHARED_NAME),
        vec![
            DUP_V1_OLD_VERSION.to_string(),
            DUP_V2_OLD_VERSION.to_string()
        ]
    );

    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "duplicate package names are unambiguous in Cargo.lock by current version and should not require per-crate cargo update --precise calls"
    );
}

#[test]
fn batch_solver_resolves_duplicate_transitive_package_names_locally() {
    let mut harness =
        DuplicateTransitiveBatchHarness::new().expect("duplicate-transitive harness should build");
    harness.generate_lockfile();
    assert_eq!(
        harness.locked_version(DUP_ROOT_A_NAME),
        DUP_PARENT_FRESH_VERSION
    );
    assert_eq!(
        harness.locked_version(DUP_ROOT_B_NAME),
        DUP_PARENT_FRESH_VERSION
    );
    assert_eq!(
        sorted_lockfile_versions(&harness.lockfile_contents(), DUP_SHARED_NAME),
        vec![
            DUP_V1_FRESH_VERSION.to_string(),
            DUP_V2_FRESH_VERSION.to_string()
        ]
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool duplicate transitive packages locally: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        harness.locked_version(DUP_ROOT_A_NAME),
        DUP_PARENT_OLD_VERSION
    );
    assert_eq!(
        harness.locked_version(DUP_ROOT_B_NAME),
        DUP_PARENT_OLD_VERSION
    );
    assert_eq!(
        sorted_lockfile_versions(&harness.lockfile_contents(), DUP_SHARED_NAME),
        vec![
            DUP_V1_OLD_VERSION.to_string(),
            DUP_V2_OLD_VERSION.to_string()
        ]
    );

    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "duplicate transitive package names should be solved in the local batch, not by per-crate cargo update --precise calls"
    );
}

#[test]
fn batch_solver_cools_workspace_dev_dependencies() {
    let mut harness = FeatureCoverageHarness::new(
        vec![PublishedCrate::new(
            "devcool",
            vec![
                PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
            ],
        )],
        vec![
            (
                "Cargo.toml",
                format!(
                    r#"[workspace]
members = ["member"]
resolver = "3"

[workspace.dependencies]
devcool = {{ version = "1", registry = "{REGISTRY_NAME}" }}
"#
                ),
            ),
            (
                "member/Cargo.toml",
                r#"[package]
name = "member"
version = "0.1.0"
edition = "2024"

[dev-dependencies]
devcool = { workspace = true }
"#
                .to_string(),
            ),
            ("member/src/lib.rs", String::new()),
        ],
    )
    .expect("feature coverage harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version("devcool"), FRESH_VERSION);

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool workspace dev-dependencies: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version("devcool"), OLD_VERSION);
    assert_no_precise_updates(&harness);
}

#[test]
fn batch_solver_cools_feature_activated_optional_dependency() {
    let mut harness = FeatureCoverageHarness::new(
        vec![
            PublishedCrate::new(
                "featureuser",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)
                        .with_dependencies(vec![
                            RegistryDependency::exact("featuredep", OLD_VERSION).optional(),
                        ])
                        .with_features(vec![("with-dep", vec!["dep:featuredep"])]),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false)
                        .with_dependencies(vec![
                            RegistryDependency::exact("featuredep", FRESH_VERSION).optional(),
                        ])
                        .with_features(vec![("with-dep", vec!["dep:featuredep"])]),
                ],
            ),
            PublishedCrate::new(
                "featuredep",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
        ],
        root_package_files(
            r#"[dependencies]
featureuser = { version = "1", registry = "cool-reg", features = ["with-dep"] }
"#,
        ),
    )
    .expect("feature coverage harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version("featureuser"), FRESH_VERSION);
    assert_eq!(harness.locked_version("featuredep"), FRESH_VERSION);

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool optional dependencies activated by root features: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version("featureuser"), OLD_VERSION);
    assert_eq!(harness.locked_version("featuredep"), OLD_VERSION);
    assert_no_precise_updates(&harness);
}

#[test]
fn batch_solver_handles_candidate_only_optional_dependency() {
    let mut harness = FeatureCoverageHarness::new(
        vec![
            PublishedCrate::new(
                "candidateoptionaluser",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)
                        .with_dependencies(vec![
                            RegistryDependency::new("candidateonlydep", "1").optional(),
                        ])
                        .with_features(vec![("with-extra", vec!["dep:candidateonlydep"])]),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false)
                        .with_features(vec![("with-extra", Vec::new())]),
                ],
            ),
            PublishedCrate::new(
                "candidateonlydep",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
        ],
        root_package_files(
            r#"[dependencies]
candidateoptionaluser = { version = "1", registry = "cool-reg", features = ["with-extra"] }
"#,
        ),
    )
    .expect("feature coverage harness should build");
    harness.generate_lockfile();
    assert_eq!(
        harness.locked_version("candidateoptionaluser"),
        FRESH_VERSION
    );
    assert!(
        parse_lockfile_version(&harness.lockfile_contents(), "candidateonlydep").is_none(),
        "fresh root candidate should not depend on candidateonlydep yet"
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool optional dependencies introduced by the selected candidate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version("candidateoptionaluser"), OLD_VERSION);
    assert_eq!(harness.locked_version("candidateonlydep"), OLD_VERSION);
    assert_no_precise_updates(&harness);
}

#[test]
fn batch_solver_cools_target_specific_dependency() {
    let active_target = "cfg(any(unix, windows))";
    let mut harness = FeatureCoverageHarness::new(
        vec![
            PublishedCrate::new(
                "targetuser",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false).with_dependencies(
                        vec![
                            RegistryDependency::exact("targetdep", OLD_VERSION)
                                .target(active_target),
                        ],
                    ),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false)
                        .with_dependencies(vec![
                            RegistryDependency::exact("targetdep", FRESH_VERSION)
                                .target(active_target),
                        ]),
                ],
            ),
            PublishedCrate::new(
                "targetdep",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
        ],
        root_package_files(
            r#"[dependencies]
targetuser = { version = "1", registry = "cool-reg" }
"#,
        ),
    )
    .expect("feature coverage harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version("targetuser"), FRESH_VERSION);
    assert_eq!(harness.locked_version("targetdep"), FRESH_VERSION);

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool target-specific dependencies active for the current target: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version("targetuser"), OLD_VERSION);
    assert_eq!(harness.locked_version("targetdep"), OLD_VERSION);
    assert_no_precise_updates(&harness);
}

#[test]
fn batch_solver_cools_candidate_introduced_transitive_dependency() {
    let mut harness = FeatureCoverageHarness::new(
        vec![
            PublishedCrate::new(
                "newtransitiveparent",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)
                        .with_dependencies(vec![RegistryDependency::new("introduceddep", "1")]),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
            PublishedCrate::new(
                "introduceddep",
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            ),
        ],
        root_package_files(
            r#"[dependencies]
newtransitiveparent = { version = "1", registry = "cool-reg" }
"#,
        ),
    )
    .expect("feature coverage harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version("newtransitiveparent"), FRESH_VERSION);
    assert!(
        parse_lockfile_version(&harness.lockfile_contents(), "introduceddep").is_none(),
        "fresh parent candidate should not depend on introduceddep yet"
    );

    let output = harness.run_cooldown();
    assert!(
        output.status.success(),
        "batch solver should cool transitive dependencies introduced by the selected candidate: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version("newtransitiveparent"), OLD_VERSION);
    assert_eq!(harness.locked_version("introduceddep"), OLD_VERSION);
    assert_no_precise_updates(&harness);
}

#[test]
#[ignore = "manual benchmark; run with -- --ignored --nocapture"]
fn benchmark_batch_solver() {
    let mut samples = Vec::new();

    for sample in 0..3 {
        let mut harness = MultiPassBenchmarkHarness::new(BENCHMARK_CRATE_COUNT)
            .expect("benchmark harness should build");
        harness.generate_lockfile();
        let started = Instant::now();
        let output = harness.run_cooldown(&[]);
        let elapsed = started.elapsed();
        assert!(
            output.status.success(),
            "batch solver run should succeed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        samples.push(elapsed);

        println!("sample {}: batch_solver={:?}", sample + 1, elapsed);
    }

    let total: f64 = samples.iter().map(Duration::as_secs_f64).sum();
    println!(
        "average batch_solver={:?}",
        Duration::from_secs_f64(total / samples.len() as f64),
    );
}

#[test]
fn cargo_compatible_allows_resolver_constrained_versions_outside_selected_scope() {
    let mut harness = ScopedConflictHarness::new().expect("scoped conflict harness should build");
    harness.generate_lockfile();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    let output = harness.run_cooldown(Some("cargo_compatible"));
    assert!(
        output.status.success(),
        "cargo_compatible should keep the lockfile and warn: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("resolver-constrained versions that could not be cooled further"),
        "{stderr}"
    );
    assert!(stderr.contains("- scopedfresh 1.0.1"), "{stderr}");
    assert!(
        stderr.contains("published: 2026-04-02T12:00:00Z"),
        "{stderr}"
    );
}

#[test]
fn cargo_compatible_requires_prompt_unless_auto_accept_is_configured() {
    let mut harness = ScopedConflictHarness::new().expect("scoped conflict harness should build");
    harness.generate_lockfile();
    let baseline_lockfile = harness.lockfile_contents();

    let output = harness.run_cooldown_requiring_prompt();
    assert!(
        !output.status.success(),
        "non-interactive cargo_compatible should require explicit acceptance: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.lockfile_contents(), baseline_lockfile);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cargo requires fresh versions that cooldown could not replace."),
        "{stderr}"
    );
    assert!(stderr.contains("- scopedfresh 1.0.1 @ "), "{stderr}");
    assert!(
        stderr.contains("published: 2026-04-02T12:00:00Z"),
        "{stderr}"
    );
    assert!(
        stderr.contains("COOLDOWN_CARGO_COMPATIBLE_ACCEPT=auto"),
        "{stderr}"
    );
}

#[test]
fn strict_rejects_resolver_constrained_versions_outside_selected_scope() {
    let mut harness = ScopedConflictHarness::new().expect("scoped conflict harness should build");
    harness.generate_lockfile();
    let baseline_lockfile = harness.lockfile_contents();
    assert_eq!(harness.locked_version(), FRESH_VERSION);

    let output = harness.run_cooldown(None);
    assert!(
        !output.status.success(),
        "strict should fail when fresh resolver-constrained versions remain: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(harness.lockfile_contents(), baseline_lockfile);
    assert_eq!(harness.locked_version(), FRESH_VERSION);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("strict enforcement blocked fresh versions"),
        "{stderr}"
    );
    assert!(stderr.contains("scopedfresh 1.0.1"), "{stderr}");
}

struct TestHarness {
    _temp_dir: TempDir,
    temp_root: PathBuf,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    server: RegistryServer,
}

impl TestHarness {
    fn new(mode: RegistryMode) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_dependency_req(mode, "1")
    }

    fn new_without_dependencies(mode: RegistryMode) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_dependencies(mode, &[])
    }

    fn new_with_dependency_req(
        mode: RegistryMode,
        version_req: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_with_dependencies(mode, &[(CRATE_NAME, version_req)])
    }

    fn new_with_dependencies(
        mode: RegistryMode,
        dependencies: &[(&str, &str)],
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let server = RegistryServer::new(mode)?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");

        fs::create_dir_all(&cargo_home)?;
        create_workspace_with_dependencies(&workspace_dir, &server, dependencies)?;
        write_registry_config(&cargo_home, &server)?;

        Ok(Self {
            _temp_dir: temp_dir,
            temp_root,
            cargo_home,
            workspace_dir,
            server,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self, extra_env: &[(&str, &str)]) -> Output {
        self.run_command(&["check"], extra_env)
    }

    fn run_command(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.run_command_in(&self.workspace_dir, args, extra_env)
    }

    fn run_command_in(
        &self,
        current_dir: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .args(args)
            .current_dir(current_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0");

        for (key, value) in extra_env {
            command.env(key, value);
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn runner_dir(&self) -> PathBuf {
        let path = self.temp_root.join("runner");
        fs::create_dir_all(&path).expect("runner dir should be creatable");
        path
    }

    fn set_dependency_requirement(&self, version_req: &str) {
        write_root_manifest(&self.workspace_dir, &[(CRATE_NAME, version_req)])
            .expect("root manifest should be rewritable");
    }

    fn locked_version(&self) -> String {
        let lockfile = fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable");
        parse_lockfile_version(&lockfile, CRATE_NAME).expect("crate should exist in lockfile")
    }
}

struct DependencyChainHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
}

struct CoordinatedBundleHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
}

struct BacktrackingBundleHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

struct DuplicateNameBatchHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

struct DuplicateTransitiveBatchHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

struct FeatureCoverageHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

impl CoordinatedBundleHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let server = RegistryServer::with_crates(
            vec![
                PublishedCrate::new(
                    BUNDLE_A_NAME,
                    vec![
                        PackageVersion::new(BUNDLE_OLD_VERSION, Some(BUNDLE_OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BUNDLE_SHARED_NAME,
                                BUNDLE_OLD_VERSION,
                            )]),
                        PackageVersion::new(
                            BUNDLE_FRESH_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        )
                        .with_dependencies(vec![
                            RegistryDependency::exact(BUNDLE_SHARED_NAME, BUNDLE_FRESH_VERSION),
                        ]),
                    ],
                ),
                PublishedCrate::new(
                    BUNDLE_B_NAME,
                    vec![
                        PackageVersion::new(BUNDLE_OLD_VERSION, Some(BUNDLE_OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BUNDLE_SHARED_NAME,
                                BUNDLE_OLD_VERSION,
                            )]),
                        PackageVersion::new(
                            BUNDLE_FRESH_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        )
                        .with_dependencies(vec![
                            RegistryDependency::exact(BUNDLE_SHARED_NAME, BUNDLE_FRESH_VERSION),
                        ]),
                    ],
                ),
                PublishedCrate::new(
                    BUNDLE_SHARED_NAME,
                    vec![
                        PackageVersion::new(BUNDLE_OLD_VERSION, Some(BUNDLE_OLD_PUBTIME), false),
                        PackageVersion::new(
                            BUNDLE_FRESH_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        ),
                    ],
                ),
            ],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        create_workspace_with_dependencies(
            &workspace_dir,
            &server,
            &[(BUNDLE_A_NAME, "1"), (BUNDLE_B_NAME, "1")],
        )?;
        write_registry_config(&cargo_home, &server)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0");

        for (key, value) in extra_env {
            command.env(key, value);
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn locked_version(&self, crate_name: &str) -> String {
        let lockfile = fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable");
        parse_lockfile_version(&lockfile, crate_name).expect("crate should exist in lockfile")
    }
}

impl BacktrackingBundleHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let wrapper_dir = temp_root.join("wrapper-bin");
        let cargo_wrapper_log = temp_root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());
        let server = RegistryServer::with_crates(
            vec![
                PublishedCrate::new(
                    BACKTRACK_LEFT_NAME,
                    vec![
                        PackageVersion::new(BACKTRACK_OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_OLD_VERSION,
                            )]),
                        PackageVersion::new(BACKTRACK_COMPAT_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_OLD_VERSION,
                            )]),
                        PackageVersion::new(BACKTRACK_CONFLICT_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_COMPAT_VERSION,
                            )]),
                        PackageVersion::new(
                            BACKTRACK_FRESH_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        )
                        .with_dependencies(vec![
                            RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_COMPAT_VERSION,
                            ),
                        ]),
                    ],
                ),
                PublishedCrate::new(
                    BACKTRACK_RIGHT_NAME,
                    vec![
                        PackageVersion::new(BACKTRACK_OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_OLD_VERSION,
                            )]),
                        PackageVersion::new(BACKTRACK_COMPAT_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_OLD_VERSION,
                            )]),
                        PackageVersion::new(BACKTRACK_CONFLICT_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_OLD_VERSION,
                            )]),
                        PackageVersion::new(
                            BACKTRACK_FRESH_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        )
                        .with_dependencies(vec![
                            RegistryDependency::exact(
                                BACKTRACK_SHARED_NAME,
                                BACKTRACK_COMPAT_VERSION,
                            ),
                        ]),
                    ],
                ),
                PublishedCrate::new(
                    BACKTRACK_SHARED_NAME,
                    vec![
                        PackageVersion::new(BACKTRACK_OLD_VERSION, Some(OLD_PUBTIME), false),
                        PackageVersion::new(
                            BACKTRACK_COMPAT_VERSION,
                            Some(BUNDLE_FRESH_PUBTIME),
                            false,
                        ),
                    ],
                ),
            ],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        fs::create_dir_all(&wrapper_dir)?;
        create_workspace_with_dependencies(
            &workspace_dir,
            &server,
            &[(BACKTRACK_LEFT_NAME, "1"), (BACKTRACK_RIGHT_NAME, "1")],
        )?;
        write_registry_config(&cargo_home, &server)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore")
            .env("PATH", &self.path_with_wrapper)
            .output()
            .expect("cargo-cooldown should run")
    }

    fn locked_version(&self, crate_name: &str) -> String {
        let lockfile = fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable");
        parse_lockfile_version(&lockfile, crate_name).expect("crate should exist in lockfile")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl DuplicateNameBatchHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let wrapper_dir = temp_root.join("wrapper-bin");
        let cargo_wrapper_log = temp_root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());
        let server = RegistryServer::with_crates(
            vec![
                PublishedCrate::new(
                    DUP_ROOT_A_NAME,
                    vec![
                        PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::new(DUP_SHARED_NAME, "1")]),
                    ],
                ),
                PublishedCrate::new(
                    DUP_ROOT_B_NAME,
                    vec![
                        PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::new(DUP_SHARED_NAME, "2")]),
                    ],
                ),
                PublishedCrate::new(
                    DUP_SHARED_NAME,
                    vec![
                        PackageVersion::new(DUP_V1_OLD_VERSION, Some(OLD_PUBTIME), false),
                        PackageVersion::new(DUP_V1_FRESH_VERSION, Some(FRESH_PUBTIME), false),
                        PackageVersion::new(DUP_V2_OLD_VERSION, Some(OLD_PUBTIME), false),
                        PackageVersion::new(DUP_V2_FRESH_VERSION, Some(FRESH_PUBTIME), false),
                    ],
                ),
            ],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        fs::create_dir_all(&wrapper_dir)?;
        create_workspace_with_dependencies(
            &workspace_dir,
            &server,
            &[(DUP_ROOT_A_NAME, "1"), (DUP_ROOT_B_NAME, "1")],
        )?;
        write_registry_config(&cargo_home, &server)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore")
            .env("PATH", &self.path_with_wrapper)
            .output()
            .expect("cargo-cooldown should run")
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl DuplicateTransitiveBatchHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let wrapper_dir = temp_root.join("wrapper-bin");
        let cargo_wrapper_log = temp_root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());
        let server = RegistryServer::with_crates(
            vec![
                PublishedCrate::new(
                    DUP_ROOT_A_NAME,
                    vec![
                        PackageVersion::new(DUP_PARENT_OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                DUP_SHARED_NAME,
                                DUP_V1_OLD_VERSION,
                            )]),
                        PackageVersion::new(DUP_PARENT_FRESH_VERSION, Some(FRESH_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                DUP_SHARED_NAME,
                                DUP_V1_FRESH_VERSION,
                            )]),
                    ],
                ),
                PublishedCrate::new(
                    DUP_ROOT_B_NAME,
                    vec![
                        PackageVersion::new(DUP_PARENT_OLD_VERSION, Some(OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                DUP_SHARED_NAME,
                                DUP_V2_OLD_VERSION,
                            )]),
                        PackageVersion::new(DUP_PARENT_FRESH_VERSION, Some(FRESH_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                DUP_SHARED_NAME,
                                DUP_V2_FRESH_VERSION,
                            )]),
                    ],
                ),
                PublishedCrate::new(
                    DUP_SHARED_NAME,
                    vec![
                        PackageVersion::new(DUP_V1_OLD_VERSION, Some(OLD_PUBTIME), false),
                        PackageVersion::new(
                            DUP_V1_FRESH_VERSION,
                            Some(DUP_TRANSITIVE_CURRENT_PUBTIME),
                            false,
                        ),
                        PackageVersion::new(DUP_V2_OLD_VERSION, Some(OLD_PUBTIME), false),
                        PackageVersion::new(
                            DUP_V2_FRESH_VERSION,
                            Some(DUP_TRANSITIVE_CURRENT_PUBTIME),
                            false,
                        ),
                    ],
                ),
            ],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        fs::create_dir_all(&wrapper_dir)?;
        create_workspace_with_dependencies(
            &workspace_dir,
            &server,
            &[(DUP_ROOT_A_NAME, "1"), (DUP_ROOT_B_NAME, "1")],
        )?;
        write_registry_config(&cargo_home, &server)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore")
            .env("COOLDOWN_VERBOSE", "true")
            .env("PATH", &self.path_with_wrapper)
            .output()
            .expect("cargo-cooldown should run")
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn locked_version(&self, crate_name: &str) -> String {
        parse_lockfile_version(&self.lockfile_contents(), crate_name)
            .expect("crate should exist in lockfile")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl DependencyChainHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let server = RegistryServer::with_crates(
            vec![
                PublishedCrate::new(
                    CHAIN_A_NAME,
                    vec![
                        PackageVersion::new(CHAIN_A_OLD_VERSION, Some(CHAIN_A_OLD_PUBTIME), false)
                            .with_dependencies(vec![RegistryDependency::exact(
                                CHAIN_B_NAME,
                                CHAIN_B_OLD_VERSION,
                            )]),
                        PackageVersion::new(
                            CHAIN_A_FRESH_VERSION,
                            Some(CHAIN_A_FRESH_PUBTIME),
                            false,
                        )
                        .with_dependencies(vec![
                            RegistryDependency::exact(CHAIN_B_NAME, CHAIN_B_UPDATED_VERSION),
                        ]),
                    ],
                ),
                PublishedCrate::new(
                    CHAIN_B_NAME,
                    vec![
                        PackageVersion::new(CHAIN_B_OLD_VERSION, Some(CHAIN_B_OLD_PUBTIME), false),
                        PackageVersion::new(
                            CHAIN_B_UPDATED_VERSION,
                            Some(CHAIN_B_UPDATED_PUBTIME),
                            false,
                        ),
                    ],
                ),
            ],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        create_workspace_with_dependency(
            &workspace_dir,
            &server,
            CHAIN_A_NAME,
            &format!("={CHAIN_A_OLD_VERSION}"),
        )?;
        write_registry_config(&cargo_home, &server)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn request_exact_version(&self, version: &str) {
        let requirement = format!("={version}");
        write_root_manifest(&self.workspace_dir, &[(CHAIN_A_NAME, requirement.as_str())])
            .expect("root manifest should be rewritable");
    }

    fn run_cooldown(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0");

        for (key, value) in extra_env {
            command.env(key, value);
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn locked_version(&self, crate_name: &str) -> String {
        parse_lockfile_version(&self.lockfile_contents(), crate_name)
            .expect("crate should exist in lockfile")
    }
}

struct WorkspaceMemberHarness {
    _temp_dir: TempDir,
    workspace_dir: PathBuf,
    member_manifest: PathBuf,
    runner_dir: PathBuf,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

struct ScopedConflictHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
}

struct MultiPassBenchmarkHarness {
    _temp_dir: TempDir,
    cargo_home: PathBuf,
    workspace_dir: PathBuf,
    _server: RegistryServer,
    cargo_wrapper_log: PathBuf,
    path_with_wrapper: OsString,
}

impl MultiPassBenchmarkHarness {
    fn new(crate_count: usize) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let wrapper_dir = temp_root.join("wrapper-bin");
        let cargo_wrapper_log = temp_root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());
        let published_crates = benchmark_published_crates(crate_count);
        let server = RegistryServer::with_crates(published_crates, false)?;

        fs::create_dir_all(&cargo_home)?;
        fs::create_dir_all(&wrapper_dir)?;
        create_workspace_with_dependencies_owned(
            &workspace_dir,
            &server,
            &benchmark_dependency_requirements(crate_count),
        )?;
        write_registry_config(&cargo_home, &server)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self, extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore")
            .env("PATH", &self.path_with_wrapper);

        for (key, value) in extra_env {
            command.env(key, value);
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl FeatureCoverageHarness {
    fn new(
        published_crates: Vec<PublishedCrate>,
        files: Vec<(&str, String)>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let wrapper_dir = temp_root.join("wrapper-bin");
        let cargo_wrapper_log = temp_root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());
        let server = RegistryServer::with_crates(published_crates, false)?;

        fs::create_dir_all(&cargo_home)?;
        fs::create_dir_all(&wrapper_dir)?;
        fs::create_dir_all(workspace_dir.join(".cargo"))?;
        for (path, contents) in files {
            let full_path = workspace_dir.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(full_path, contents)?;
        }
        fs::write(
            workspace_dir.join(".cargo/config.toml"),
            format!(
                r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
                registry_name = REGISTRY_NAME,
                base_url = server.base_url(),
            ),
        )?;
        write_registry_config(&cargo_home, &server)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
            .arg("check")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore")
            .env("PATH", &self.path_with_wrapper)
            .output()
            .expect("cargo-cooldown should run")
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn locked_version(&self, crate_name: &str) -> String {
        parse_lockfile_version(&self.lockfile_contents(), crate_name)
            .expect("crate should exist in lockfile")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

impl ScopedConflictHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let temp_root = temp_dir.path().to_path_buf();
        let cargo_home = temp_root.join("cargo-home");
        let workspace_dir = temp_root.join("workspace");
        let server = RegistryServer::with_crates(
            vec![PublishedCrate::new(
                SCOPED_CONFLICT_NAME,
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            )],
            false,
        )?;

        fs::create_dir_all(&cargo_home)?;
        create_scoped_conflict_workspace(&workspace_dir, &server)?;
        write_registry_config(&cargo_home, &server)?;

        Ok(Self {
            _temp_dir: temp_dir,
            cargo_home,
            workspace_dir,
            _server: server,
        })
    }

    fn generate_lockfile(&mut self) {
        let output = Command::new("cargo")
            .arg("generate-lockfile")
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self, enforcement: Option<&str>) -> Output {
        let mut command = self.cooldown_command();

        if let Some(enforcement) = enforcement {
            command.env("COOLDOWN_ENFORCEMENT", enforcement);
            if enforcement == "cargo_compatible" {
                command.env("COOLDOWN_CARGO_COMPATIBLE_ACCEPT", "auto");
            }
        }

        command.output().expect("cargo-cooldown should run")
    }

    fn run_cooldown_requiring_prompt(&self) -> Output {
        let mut command = self.cooldown_command();
        command.env("COOLDOWN_ENFORCEMENT", "cargo_compatible");
        command.output().expect("cargo-cooldown should run")
    }

    fn cooldown_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .args(["check", "--package", SCOPED_MEMBER_A])
            .current_dir(&self.workspace_dir)
            .env("CARGO_HOME", &self.cargo_home)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_NOW", NOW)
            .env("COOLDOWN_MINUTES", COOLDOWN_MINUTES)
            .env("COOLDOWN_HTTP_RETRIES", "0")
            .env("COOLDOWN_LOCKFILE_BASELINE", "ignore");
        command
    }

    fn lockfile_contents(&self) -> String {
        fs::read_to_string(self.workspace_dir.join("Cargo.lock"))
            .expect("lockfile should be readable")
    }

    fn locked_version(&self) -> String {
        parse_lockfile_version(&self.lockfile_contents(), SCOPED_CONFLICT_NAME)
            .expect("crate should exist in lockfile")
    }
}

impl WorkspaceMemberHarness {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let temp_dir = tempdir()?;
        let root = temp_dir.path().to_path_buf();
        let workspace_dir = root.join("workspace");
        let runner_dir = root.join("runner");
        let wrapper_dir = root.join("wrapper-bin");
        let cargo_wrapper_log = root.join("cargo-invocations.log");
        let wrapper_path = wrapper_dir.join(wrapper_binary_name());

        fs::create_dir_all(&runner_dir)?;
        fs::create_dir_all(&wrapper_dir)?;
        let member_manifest = create_workspace_member_fixture(&workspace_dir)?;
        write_cargo_wrapper(&wrapper_path, &cargo_wrapper_log)?;
        let path_with_wrapper = prepend_to_path(&wrapper_dir)?;

        Ok(Self {
            _temp_dir: temp_dir,
            workspace_dir,
            member_manifest,
            runner_dir,
            cargo_wrapper_log,
            path_with_wrapper,
        })
    }

    fn generate_lockfile(&self) {
        let output = Command::new(real_cargo_binary())
            .arg("generate-lockfile")
            .arg("--manifest-path")
            .arg(&self.member_manifest)
            .current_dir(&self.runner_dir)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .output()
            .expect("cargo generate-lockfile should run");

        assert!(
            output.status.success(),
            "workspace lockfile generation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_cooldown(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
            .args([
                "check",
                "--manifest-path",
                self.member_manifest.to_string_lossy().as_ref(),
            ])
            .current_dir(&self.runner_dir)
            .env("CARGO_TERM_PROGRESS_WHEN", "never")
            .env("COOLDOWN_MINUTES", "60")
            .env("PATH", &self.path_with_wrapper)
            .env("COOLDOWN_CARGO_LOG", &self.cargo_wrapper_log)
            .output()
            .expect("cargo-cooldown should run")
    }

    fn workspace_lockfile(&self) -> PathBuf {
        self.workspace_dir.join("Cargo.lock")
    }

    fn member_lockfile(&self) -> PathBuf {
        self.workspace_dir.join("member").join("Cargo.lock")
    }

    fn cargo_log(&self) -> Vec<String> {
        fs::read_to_string(&self.cargo_wrapper_log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[derive(Clone, Copy)]
enum RegistryMode {
    PubtimeOnly,
    MissingPubtimeWithApi,
    MissingPubtimeNoApi,
}

#[derive(Clone)]
struct PublishedCrate {
    name: String,
    versions: Vec<PackageVersion>,
}

impl PublishedCrate {
    fn new(name: &str, versions: Vec<PackageVersion>) -> Self {
        Self {
            name: name.to_string(),
            versions,
        }
    }
}

struct RegistryServer {
    base_url: String,
    state: Arc<ServerState>,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl RegistryServer {
    fn new(mode: RegistryMode) -> Result<Self, Box<dyn std::error::Error>> {
        let published_crates = vec![PublishedCrate::new(
            CRATE_NAME,
            vec![
                PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                PackageVersion::new(FRESH_VERSION, mode.pubtime_for_fresh(), false),
            ],
        )];
        Self::with_crates(published_crates, mode.has_api())
    }

    fn with_crates(
        published_crates: Vec<PublishedCrate>,
        with_api: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let base_paths = build_registry_paths(&base_url, with_api, &published_crates)?;
        let state = Arc::new(ServerState {
            responses: Mutex::new(base_paths),
            request_counts: Mutex::new(HashMap::new()),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);

        let handle = thread::spawn(move || {
            while !thread_shutdown.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&thread_state);
                        thread::spawn(move || {
                            let _ = handle_stream(stream, state);
                        });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url,
            state,
            shutdown,
            handle: Some(handle),
        })
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn api_hits(&self) -> usize {
        self.state
            .count_for(&format!("/api/v1/crates/{CRATE_NAME}"))
    }

    fn reset_counts(&self) {
        self.state.request_counts.lock().unwrap().clear();
    }
}

impl Drop for RegistryServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base_url.strip_prefix("http://").unwrap());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct ServerState {
    responses: Mutex<HashMap<String, ResponseSpec>>,
    request_counts: Mutex<HashMap<String, usize>>,
}

impl ServerState {
    fn count_for(&self, path: &str) -> usize {
        *self.request_counts.lock().unwrap().get(path).unwrap_or(&0)
    }
}

fn handle_stream(mut stream: TcpStream, state: Arc<ServerState>) -> std::io::Result<()> {
    let mut buffer = [0_u8; 4096];
    let bytes = stream.read(&mut buffer)?;
    if bytes == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let _method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    *state
        .request_counts
        .lock()
        .unwrap()
        .entry(path.to_string())
        .or_insert(0) += 1;

    let response = state
        .responses
        .lock()
        .unwrap()
        .get(path)
        .cloned()
        .unwrap_or_else(ResponseSpec::not_found);

    write_response(&mut stream, response)
}

#[derive(Clone)]
struct ResponseSpec {
    status: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
}

impl ResponseSpec {
    fn ok(content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status: "200 OK",
            content_type,
            body,
        }
    }

    fn not_found() -> Self {
        Self {
            status: "404 Not Found",
            content_type: "text/plain",
            body: b"not found".to_vec(),
        }
    }
}

fn write_response(stream: &mut TcpStream, response: ResponseSpec) -> std::io::Result<()> {
    let headers = format!(
        "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.body.len(),
        response.content_type
    );
    stream.write_all(headers.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[derive(Clone)]
struct PackageVersion {
    version: String,
    pubtime: Option<String>,
    yanked: bool,
    dependencies: Vec<RegistryDependency>,
    features: Vec<(String, Vec<String>)>,
}

impl PackageVersion {
    fn new(version: &str, pubtime: Option<&str>, yanked: bool) -> Self {
        Self {
            version: version.to_string(),
            pubtime: pubtime.map(ToOwned::to_owned),
            yanked,
            dependencies: Vec::new(),
            features: Vec::new(),
        }
    }

    fn with_dependencies(mut self, dependencies: Vec<RegistryDependency>) -> Self {
        self.dependencies = dependencies;
        self
    }

    fn with_features(mut self, features: Vec<(&str, Vec<&str>)>) -> Self {
        self.features = features
            .into_iter()
            .map(|(name, values)| {
                (
                    name.to_string(),
                    values.into_iter().map(ToOwned::to_owned).collect(),
                )
            })
            .collect();
        self
    }
}

#[derive(Clone)]
struct RegistryDependency {
    name: String,
    requirement: String,
    optional: bool,
    target: Option<String>,
    kind: Option<String>,
}

impl RegistryDependency {
    fn new(name: &str, requirement: &str) -> Self {
        Self {
            name: name.to_string(),
            requirement: requirement.to_string(),
            optional: false,
            target: None,
            kind: None,
        }
    }

    fn exact(name: &str, version: &str) -> Self {
        Self::new(name, &format!("={version}"))
    }

    fn optional(mut self) -> Self {
        self.optional = true;
        self
    }

    fn target(mut self, target: &str) -> Self {
        self.target = Some(target.to_string());
        self
    }
}

fn create_workspace_with_dependency(
    workspace_dir: &Path,
    server: &RegistryServer,
    crate_name: &str,
    version_req: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    create_workspace_with_dependencies(workspace_dir, server, &[(crate_name, version_req)])
}

fn create_workspace_with_dependencies(
    workspace_dir: &Path,
    server: &RegistryServer,
    dependencies: &[(&str, &str)],
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(workspace_dir.join("src"))?;
    fs::create_dir_all(workspace_dir.join(".cargo"))?;
    write_root_manifest(workspace_dir, dependencies)?;
    fs::write(
        workspace_dir.join("src/main.rs"),
        render_main_file(dependencies),
    )?;
    fs::write(
        workspace_dir.join(".cargo/config.toml"),
        format!(
            r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
            registry_name = REGISTRY_NAME,
            base_url = server.base_url(),
        ),
    )?;

    Ok(())
}

fn create_workspace_with_dependencies_owned(
    workspace_dir: &Path,
    server: &RegistryServer,
    dependencies: &[(String, String)],
) -> Result<(), Box<dyn std::error::Error>> {
    let refs = dependencies
        .iter()
        .map(|(crate_name, version_req)| (crate_name.as_str(), version_req.as_str()))
        .collect::<Vec<_>>();
    create_workspace_with_dependencies(workspace_dir, server, &refs)
}

fn write_root_manifest(
    workspace_dir: &Path,
    dependencies: &[(&str, &str)],
) -> Result<(), Box<dyn std::error::Error>> {
    let dependency_lines = dependencies
        .iter()
        .map(|(crate_name, version_req)| {
            format!(
                r#"{crate_name} = {{ version = "{version_req}", registry = "{REGISTRY_NAME}" }}"#
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        workspace_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "cooldown-workspace"
version = "0.1.0"
edition = "2024"

[dependencies]
{dependency_lines}
"#
        ),
    )?;
    Ok(())
}

fn render_main_file(dependencies: &[(&str, &str)]) -> String {
    let lines = dependencies
        .iter()
        .map(|(crate_name, _)| format!("    println!(\"{{}}\", {crate_name}::value());"))
        .collect::<Vec<_>>()
        .join("\n");

    format!("fn main() {{\n{lines}\n}}\n")
}

fn root_package_files(dependency_section: &str) -> Vec<(&str, String)> {
    vec![
        (
            "Cargo.toml",
            format!(
                r#"[package]
name = "feature-coverage"
version = "0.1.0"
edition = "2024"

{dependency_section}
"#
            ),
        ),
        ("src/main.rs", "fn main() {}\n".to_string()),
    ]
}

fn assert_no_precise_updates(harness: &FeatureCoverageHarness) {
    let precise_updates = harness
        .cargo_log()
        .into_iter()
        .filter(|line| line.contains("--precise"))
        .count();
    assert_eq!(
        precise_updates, 0,
        "feature coverage cases should be cooled without per-crate cargo update --precise calls"
    );
}

fn create_workspace_member_fixture(
    workspace_dir: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let member_dir = workspace_dir.join("member");
    fs::create_dir_all(member_dir.join("src"))?;

    fs::write(
        workspace_dir.join("Cargo.toml"),
        r#"[workspace]
members = ["member"]
resolver = "3"
"#,
    )?;
    fs::write(
        member_dir.join("Cargo.toml"),
        r#"[package]
name = "member"
version = "0.1.0"
edition = "2024"
"#,
    )?;
    fs::write(
        member_dir.join("src/main.rs"),
        r#"fn main() {
    println!("member");
}
"#,
    )?;

    Ok(member_dir.join("Cargo.toml"))
}

fn create_scoped_conflict_workspace(
    workspace_dir: &Path,
    server: &RegistryServer,
) -> Result<(), Box<dyn std::error::Error>> {
    let selected_member_dir = workspace_dir.join(SCOPED_MEMBER_A);
    let blocking_member_dir = workspace_dir.join(SCOPED_MEMBER_B);
    fs::create_dir_all(selected_member_dir.join("src"))?;
    fs::create_dir_all(blocking_member_dir.join("src"))?;
    fs::create_dir_all(workspace_dir.join(".cargo"))?;

    fs::write(
        workspace_dir.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = ["{SCOPED_MEMBER_A}", "{SCOPED_MEMBER_B}"]
resolver = "3"
"#,
        ),
    )?;
    fs::write(
        workspace_dir.join(".cargo/config.toml"),
        format!(
            r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
            registry_name = REGISTRY_NAME,
            base_url = server.base_url(),
        ),
    )?;
    fs::write(
        selected_member_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{SCOPED_MEMBER_A}"
version = "0.1.0"
edition = "2024"

[dependencies]
{SCOPED_CONFLICT_NAME} = {{ version = "1", registry = "{REGISTRY_NAME}" }}
"#,
        ),
    )?;
    fs::write(
        blocking_member_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{SCOPED_MEMBER_B}"
version = "0.1.0"
edition = "2024"

[dependencies]
{SCOPED_CONFLICT_NAME} = {{ version = "={FRESH_VERSION}", registry = "{REGISTRY_NAME}" }}
"#,
        ),
    )?;
    fs::write(
        selected_member_dir.join("src/main.rs"),
        format!(
            r#"fn main() {{
    println!("{{}}", {SCOPED_CONFLICT_NAME}::value());
}}
"#,
        ),
    )?;
    fs::write(
        blocking_member_dir.join("src/main.rs"),
        format!(
            r#"fn main() {{
    println!("{{}}", {SCOPED_CONFLICT_NAME}::value());
}}
"#,
        ),
    )?;

    Ok(())
}

fn write_cargo_wrapper(
    wrapper_path: &Path,
    log_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    write_platform_cargo_wrapper(wrapper_path, log_path)?;
    Ok(())
}

#[cfg(unix)]
fn write_hold_asserting_cargo_wrapper(
    wrapper_path: &Path,
    log_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    fs::write(
        wrapper_path,
        format!(
            r#"#!/bin/sh
printf 'cargo %s cwd=%s\n' "$*" "$(pwd)" >> "{log_path}"
if [ "$1" = "update" ]; then
  lockfile="$COOLDOWN_EXPECT_HELD_WORKSPACE/Cargo.lock"
  if grep -q '^cargo-cooldown lockfile hold' "$lockfile"; then
    printf 'update-held-lockfile\n' >> "{log_path}"
  else
    printf 'update-missing-held-lockfile\n' >> "{log_path}"
    exit 97
  fi
  if [ "$(pwd)" = "$COOLDOWN_EXPECT_HELD_WORKSPACE" ]; then
    printf 'update-used-original-workspace\n' >> "{log_path}"
    exit 98
  else
    printf 'update-used-temp-workspace\n' >> "{log_path}"
  fi
  case "$*" in
    *"$COOLDOWN_EXPECT_HELD_WORKSPACE"*)
      printf 'update-kept-original-manifest-path\n' >> "{log_path}"
      exit 99
      ;;
    *)
      printf 'update-rewrote-manifest-path\n' >> "{log_path}"
      ;;
  esac
fi
exec "{real_cargo}" "$@"
"#,
            log_path = log_path.display(),
            real_cargo = real_cargo_binary(),
        ),
    )?;
    let mut permissions = fs::metadata(wrapper_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(wrapper_path, permissions)?;
    Ok(())
}

fn real_cargo_binary() -> String {
    std::env::var("CARGO").expect("cargo test should expose the real cargo binary path")
}

fn prepend_to_path(prefix: &Path) -> Result<OsString, Box<dyn std::error::Error>> {
    let mut paths = vec![prefix.to_path_buf()];
    paths.extend(
        std::env::var_os("PATH")
            .map(|raw| std::env::split_paths(&raw).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    Ok(std::env::join_paths(paths)?)
}

#[cfg(unix)]
fn wrapper_binary_name() -> &'static str {
    "cargo"
}

#[cfg(windows)]
fn wrapper_binary_name() -> &'static str {
    "cargo.bat"
}

#[cfg(unix)]
fn write_platform_cargo_wrapper(
    wrapper_path: &Path,
    log_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    fs::write(
        wrapper_path,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log_path}"
exec "{real_cargo}" "$@"
"#,
            log_path = log_path.display(),
            real_cargo = real_cargo_binary(),
        ),
    )?;
    let mut permissions = fs::metadata(wrapper_path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(wrapper_path, permissions)?;
    Ok(())
}

#[cfg(windows)]
fn write_platform_cargo_wrapper(
    wrapper_path: &Path,
    log_path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        wrapper_path,
        format!(
            "@echo off\r\necho %*>>\"{log_path}\"\r\n\"{real_cargo}\" %*\r\n",
            log_path = log_path.display(),
            real_cargo = real_cargo_binary(),
        ),
    )?;
    Ok(())
}

fn write_registry_config(
    cargo_home: &Path,
    server: &RegistryServer,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            r#"[registries.{registry_name}]
index = "sparse+{base_url}/index/"
"#,
            registry_name = REGISTRY_NAME,
            base_url = server.base_url(),
        ),
    )?;
    Ok(())
}

fn build_tarballs(
    crate_name: &str,
    versions: &[PackageVersion],
) -> Result<HashMap<String, Vec<u8>>, Box<dyn std::error::Error>> {
    let mut tarballs = HashMap::new();
    for version in versions {
        tarballs.insert(
            version.version.clone(),
            create_crate_archive(crate_name, version)?,
        );
    }
    Ok(tarballs)
}

fn create_crate_archive(
    crate_name: &str,
    version: &PackageVersion,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let package_dir = temp
        .path()
        .join(format!("{crate_name}-{}", version.version));
    let root_dir = format!("{crate_name}-{}", version.version);
    fs::create_dir_all(package_dir.join("src"))?;
    let dependency_section = render_crate_dependency_sections(&version.dependencies);
    let feature_section = render_feature_section(&version.features);
    fs::write(
        package_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{crate_name}"
version = "{version}"
edition = "2024"

[lib]
path = "src/lib.rs"
{dependency_section}
{feature_section}
"#,
            crate_name = crate_name,
            version = version.version,
            dependency_section = dependency_section,
            feature_section = feature_section,
        ),
    )?;
    fs::write(
        package_dir.join("src/lib.rs"),
        format!(
            r#"pub fn value() -> &'static str {{
    "{version}"
}}
"#,
            version = version.version,
        ),
    )?;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = Builder::new(&mut encoder);
    builder.append_dir(root_dir.clone(), &package_dir)?;
    builder.append_dir(format!("{root_dir}/src"), package_dir.join("src"))?;
    builder.append_path_with_name(
        package_dir.join("Cargo.toml"),
        format!("{root_dir}/Cargo.toml"),
    )?;
    builder.append_path_with_name(
        package_dir.join("src/lib.rs"),
        format!("{root_dir}/src/lib.rs"),
    )?;
    builder.finish()?;
    drop(builder);

    Ok(encoder.finish()?)
}

fn render_crate_dependency_sections(dependencies: &[RegistryDependency]) -> String {
    let mut normal = Vec::new();
    let mut dev = Vec::new();
    let mut target_sections = HashMap::<String, Vec<String>>::new();

    for dependency in dependencies {
        let entry = render_registry_dependency_entry(dependency, false);
        if let Some(target) = &dependency.target {
            target_sections
                .entry(target.clone())
                .or_default()
                .push(render_registry_dependency_entry(dependency, false));
        } else if dependency.kind.as_deref() == Some("dev") {
            dev.push(entry);
        } else {
            normal.push(entry);
        }
    }

    let mut sections = String::new();
    if !normal.is_empty() {
        sections.push_str("\n[dependencies]\n");
        sections.push_str(&normal.join("\n"));
        sections.push('\n');
    }
    if !dev.is_empty() {
        sections.push_str("\n[dev-dependencies]\n");
        sections.push_str(&dev.join("\n"));
        sections.push('\n');
    }

    let mut target_keys = target_sections.keys().cloned().collect::<Vec<_>>();
    target_keys.sort();
    for target in target_keys {
        if let Some(entries) = target_sections.get(&target) {
            sections.push_str(&format!("\n[target.'{target}'.dependencies]\n"));
            sections.push_str(&entries.join("\n"));
            sections.push('\n');
        }
    }

    sections
}

fn render_registry_dependency_entry(
    dependency: &RegistryDependency,
    include_registry: bool,
) -> String {
    let registry = if include_registry {
        format!(r#", registry = "{REGISTRY_NAME}""#)
    } else {
        String::new()
    };
    let optional = if dependency.optional {
        ", optional = true"
    } else {
        ""
    };

    format!(
        r#"{name} = {{ version = "{requirement}"{registry}{optional} }}"#,
        name = dependency.name,
        requirement = dependency.requirement,
        registry = registry,
        optional = optional,
    )
}

fn render_feature_section(features: &[(String, Vec<String>)]) -> String {
    if features.is_empty() {
        return String::new();
    }

    let mut entries = features
        .iter()
        .map(|(name, values)| {
            let values = values
                .iter()
                .map(|value| format!(r#""{value}""#))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{name} = [{values}]")
        })
        .collect::<Vec<_>>();
    entries.sort();
    format!("\n[features]\n{}\n", entries.join("\n"))
}

fn build_registry_paths(
    base_url: &str,
    with_api: bool,
    published_crates: &[PublishedCrate],
) -> Result<HashMap<String, ResponseSpec>, Box<dyn std::error::Error>> {
    let mut responses = HashMap::new();

    let config_body = if with_api {
        format!(r#"{{"dl":"{base_url}/crates","api":"{base_url}"}}"#)
    } else {
        format!(r#"{{"dl":"{base_url}/crates"}}"#)
    };
    responses.insert(
        "/index/config.json".to_string(),
        ResponseSpec::ok("application/json", config_body.into_bytes()),
    );

    for published in published_crates {
        let krate_name: KrateName<'_> = published.name.as_str().try_into()?;
        let relative_path = krate_name.relative_path(Some('/'));
        let tarballs = build_tarballs(&published.name, &published.versions)?;
        let index_body = build_index_body(&published.name, &published.versions, &tarballs)?;
        responses.insert(
            format!("/index/{relative_path}"),
            ResponseSpec::ok("text/plain", index_body.into_bytes()),
        );

        if with_api {
            responses.insert(
                format!("/api/v1/crates/{}", published.name),
                ResponseSpec::ok(
                    "application/json",
                    build_api_body(&published.versions).into_bytes(),
                ),
            );
        }

        for version in &published.versions {
            responses.insert(
                format!("/crates/{}/{}/download", published.name, version.version),
                ResponseSpec::ok(
                    "application/gzip",
                    tarballs
                        .get(&version.version)
                        .expect("tarball should exist")
                        .clone(),
                ),
            );
        }
    }

    Ok(responses)
}

fn build_index_body(
    crate_name: &str,
    versions: &[PackageVersion],
    tarballs: &HashMap<String, Vec<u8>>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut lines = Vec::new();
    for version in versions {
        let checksum = sha256_hex(
            tarballs
                .get(&version.version)
                .expect("tarball should exist"),
        );
        let mut value = serde_json::json!({
            "name": crate_name,
            "vers": version.version,
            "deps": version
                .dependencies
                .iter()
                .map(|dependency| serde_json::json!({
                    "name": dependency.name,
                    "req": dependency.requirement,
                    "features": [],
                    "optional": dependency.optional,
                    "default_features": true,
                    "target": dependency
                        .target
                        .clone()
                        .map_or(serde_json::Value::Null, serde_json::Value::String),
                    "kind": dependency
                        .kind
                        .clone()
                        .map_or(serde_json::Value::Null, serde_json::Value::String),
                }))
                .collect::<Vec<_>>(),
            "cksum": checksum,
            "features": version
                .features
                .iter()
                .map(|(name, values)| (name.clone(), values.clone()))
                .collect::<HashMap<_, _>>(),
            "yanked": version.yanked,
        });
        if let Some(pubtime) = &version.pubtime {
            value["pubtime"] = serde_json::Value::String(pubtime.clone());
        }
        lines.push(serde_json::to_string(&value)?);
    }
    Ok(lines.join("\n"))
}

fn build_api_body(versions: &[PackageVersion]) -> String {
    serde_json::json!({
        "versions": versions
            .iter()
            .rev()
            .map(|version| serde_json::json!({
                "num": version.version,
                "created_at": version
                    .pubtime
                    .clone()
                    .unwrap_or_else(|| match version.version.as_str() {
                        OLD_VERSION => OLD_PUBTIME.to_string(),
                        _ => FRESH_PUBTIME.to_string(),
                    }),
                "yanked": version.yanked,
            }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn parse_lockfile_version(lockfile: &str, crate_name: &str) -> Option<String> {
    let mut in_block = false;
    for line in lockfile.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_block = false;
            continue;
        }
        if trimmed == format!("name = \"{crate_name}\"") {
            in_block = true;
            continue;
        }
        if in_block && trimmed.starts_with("version = ") {
            return trimmed
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
                .map(ToOwned::to_owned);
        }
    }
    None
}

fn sorted_lockfile_versions(lockfile: &str, crate_name: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut in_block = false;
    for line in lockfile.lines() {
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            in_block = false;
            continue;
        }
        if trimmed == format!("name = \"{crate_name}\"") {
            in_block = true;
            continue;
        }
        if in_block
            && trimmed.starts_with("version = ")
            && let Some(version) = trimmed
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
        {
            versions.push(version.to_string());
        }
    }
    versions.sort_unstable();
    versions
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn benchmark_crate_name(index: usize) -> String {
    format!("benchdep{index:02}")
}

fn benchmark_dependency_requirements(crate_count: usize) -> Vec<(String, String)> {
    (0..crate_count)
        .map(|index| (benchmark_crate_name(index), "1".to_string()))
        .collect()
}

fn benchmark_published_crates(crate_count: usize) -> Vec<PublishedCrate> {
    (0..crate_count)
        .map(|index| {
            PublishedCrate::new(
                &benchmark_crate_name(index),
                vec![
                    PackageVersion::new(OLD_VERSION, Some(OLD_PUBTIME), false),
                    PackageVersion::new(FRESH_VERSION, Some(FRESH_PUBTIME), false),
                ],
            )
        })
        .collect()
}

impl RegistryMode {
    fn pubtime_for_fresh(self) -> Option<&'static str> {
        match self {
            RegistryMode::PubtimeOnly => Some(FRESH_PUBTIME),
            RegistryMode::MissingPubtimeWithApi | RegistryMode::MissingPubtimeNoApi => None,
        }
    }

    fn has_api(self) -> bool {
        matches!(
            self,
            RegistryMode::PubtimeOnly | RegistryMode::MissingPubtimeWithApi
        )
    }
}
