//! End-to-end tests for the interactive `cargo cooldown init` command.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;

#[test]
fn init_creates_cooldown_toml_for_crate_root() {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    write_crate_fixture(temp_dir.path());

    let output = run_init(temp_dir.path(), "\n\n\n\n\n\n");
    assert!(
        output.status.success(),
        "init should succeed for a crate root: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let config = fs::read_to_string(temp_dir.path().join("cooldown.toml"))
        .expect("init should create cooldown.toml");
    assert!(config.contains("cooldown_minutes = 1440"));
    assert!(config.contains("enforcement = \"cargo_compatible\""));
    assert!(config.contains("cargo_compatible_accept = \"prompt\""));
    assert!(config.contains("lockfile_baseline = \"floor\""));
    assert!(config.contains("[allow.global]"));
}

#[test]
fn init_creates_workspace_root_and_selected_member_override() {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    let members = write_workspace_fixture(temp_dir.path(), &["member-a", "member-b"]);

    let output = run_init(temp_dir.path(), "2\n\n\n\n\n\n1\nn\n\n\n");
    assert!(
        output.status.success(),
        "init should succeed for a workspace root: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let workspace_config = fs::read_to_string(temp_dir.path().join("cooldown.toml"))
        .expect("workspace config should exist");
    assert!(workspace_config.contains("cooldown_minutes = 1440"));
    assert!(workspace_config.contains("[allow.global]"));

    let member_override = fs::read_to_string(members[0].join("cooldown.toml"))
        .expect("selected member override should exist");
    assert!(member_override.contains("overrides the workspace defaults"));
    assert!(!member_override.contains("cooldown_minutes ="));
    assert!(member_override.contains("[allow.global]"));

    assert!(
        !members[1].join("cooldown.toml").exists(),
        "unselected members should not receive override files"
    );
}

#[test]
fn init_rejects_non_root_directory() {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    let members = write_workspace_fixture(temp_dir.path(), &["member-a"]);

    let output = run_init(&members[0], "");
    assert!(
        !output.status.success(),
        "init should fail outside the project root"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("must run from the project root"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!members[0].join("cooldown.toml").exists());
}

#[test]
fn init_refuses_to_overwrite_existing_config() {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    write_crate_fixture(temp_dir.path());
    let config_path = temp_dir.path().join("cooldown.toml");
    fs::write(&config_path, "enforcement = \"cargo_compatible\"\n")
        .expect("fixture config should be writable");

    let output = run_init(temp_dir.path(), "\n\n\n\n\n\n");
    assert!(
        !output.status.success(),
        "init should refuse to overwrite an existing config"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("refusing to overwrite"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&config_path).expect("existing config should still exist"),
        "enforcement = \"cargo_compatible\"\n"
    );
}

#[test]
fn init_with_cargo_init_flags_explains_the_collision() {
    let temp_dir = tempdir().expect("tempdir should be creatable");
    write_crate_fixture(temp_dir.path());

    let output = run_cooldown(temp_dir.path(), &["init", "--bin"], "");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Use plain `cargo init ...` to create a new package"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_init(project_root: &Path, input: &str) -> Output {
    run_cooldown(project_root, &["init"], input)
}

fn run_cooldown(project_root: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .args(args)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cargo-cooldown init should spawn");

    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(input.as_bytes())
        .expect("init stdin should be writable");

    child
        .wait_with_output()
        .expect("cargo-cooldown init should complete")
}

fn write_crate_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("crate src dir should be creatable");
    fs::write(
        root.join("Cargo.toml"),
        r#"[package]
name = "demo-crate"
version = "0.1.0"
edition = "2024"
"#,
    )
    .expect("crate manifest should be writable");
    fs::write(
        root.join("src/main.rs"),
        r#"fn main() {
    println!("demo");
}
"#,
    )
    .expect("crate main.rs should be writable");
}

fn write_workspace_fixture(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    let member_list = names
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ");
    fs::write(
        root.join("Cargo.toml"),
        format!(
            r#"[workspace]
members = [{member_list}]
resolver = "3"
"#
        ),
    )
    .expect("workspace manifest should be writable");

    names
        .iter()
        .map(|name| {
            let member_dir = root.join(name);
            fs::create_dir_all(member_dir.join("src"))
                .expect("workspace member src dir should be creatable");
            fs::write(
                member_dir.join("Cargo.toml"),
                format!(
                    r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2024"
"#
                ),
            )
            .expect("member manifest should be writable");
            fs::write(
                member_dir.join("src/main.rs"),
                format!(
                    r#"fn main() {{
    println!("{name}");
}}
"#
                ),
            )
            .expect("member main.rs should be writable");
            member_dir
        })
        .collect()
}
