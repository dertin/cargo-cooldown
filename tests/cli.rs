//! End-to-end tests for top-level CLI behavior.

use std::process::Command;

use tempfile::tempdir;

#[test]
fn help_lists_documented_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("--help")
        .output()
        .expect("cargo-cooldown help should run");

    assert!(
        output.status.success(),
        "help should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in ["init", "version", "check", "build", "test", "run", "update"] {
        assert!(
            stdout.contains(&format!("  {expected}")),
            "help should list `{expected}`:\n{stdout}"
        );
    }
}

#[test]
fn version_subcommand_prints_tool_version_without_project_discovery() {
    let temp_dir = tempdir().expect("tempdir should be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("version")
        .current_dir(temp_dir.path())
        .output()
        .expect("cargo-cooldown version should run");

    assert!(
        output.status.success(),
        "version should not require a Cargo project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("cargo-cooldown {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn version_flag_prints_tool_version_without_project_discovery() {
    let temp_dir = tempdir().expect("tempdir should be creatable");

    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("--version")
        .current_dir(temp_dir.path())
        .output()
        .expect("cargo-cooldown --version should run");

    assert!(
        output.status.success(),
        "--version should not require a Cargo project: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("cargo-cooldown {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn wrapped_check_help_is_forwarded_without_project_discovery() {
    let output = run_cooldown_without_project(&["check", "--help"]);

    assert!(
        output.status.success(),
        "check --help should be forwarded directly to Cargo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Usage: cargo check"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn wrapped_update_help_is_forwarded_without_running_update_flow() {
    let output = run_cooldown_without_project(&["update", "--help"]);

    assert!(
        output.status.success(),
        "update --help should be forwarded directly to Cargo: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("Usage: cargo update"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

fn run_cooldown_without_project(args: &[&str]) -> std::process::Output {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .args(args)
        .current_dir(temp_dir.path())
        .output()
        .expect("cargo-cooldown should spawn")
}
