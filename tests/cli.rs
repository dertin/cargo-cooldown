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
        String::from_utf8_lossy(&output.stdout).contains(&format!(
            "Usage: cargo{} check",
            std::env::consts::EXE_SUFFIX
        )),
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
        String::from_utf8_lossy(&output.stdout).contains(&format!(
            "Usage: cargo{} update",
            std::env::consts::EXE_SUFFIX
        )),
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

#[test]
fn version_does_not_require_a_configured_rustup_toolchain() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("version")
        .env("RUSTUP_HOME", temp.path())
        .env_remove("RUSTUP_TOOLCHAIN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[cfg(unix)]
#[test]
fn old_selected_cargo_is_rejected_before_touching_lockfile() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(&cargo, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'cargo 1.96.0 (test)'; exit 0; fi\nexit 77\n").unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lock = temp.path().join("Cargo.lock");
    std::fs::write(&lock, "original lockfile\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("update")
        .current_dir(temp.path())
        .env("PATH", bin)
        .env("RUSTUP_TOOLCHAIN", "test")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires Cargo 1.97"));
    assert_eq!(
        std::fs::read_to_string(lock).unwrap(),
        "original lockfile\n"
    );
    assert!(!temp.path().join("Cargo.lock.cooldown-hold").exists());
}

#[test]
fn configured_alternate_lockfile_is_rejected_without_writes() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(project.join(".cargo")).unwrap();
    let original = project.join("Cargo.lock");
    let alternate = project.join("alternate/Cargo.lock");
    std::fs::create_dir_all(alternate.parent().unwrap()).unwrap();
    std::fs::write(&original, "original\n").unwrap();
    std::fs::write(&alternate, "alternate\n").unwrap();
    let config = project.join(".cargo/config.toml");
    let included = project.join(".cargo/included.toml");
    for case in [
        "direct",
        "include",
        "environment",
        "cli",
        "cli-file",
        "cli-flag",
    ] {
        std::fs::write(
            &config,
            if case == "direct" {
                "[resolver]\nlockfile-path = 'alternate/Cargo.lock'\n"
            } else if case == "include" {
                "include = ['included.toml']\n"
            } else {
                ""
            },
        )
        .unwrap();
        std::fs::write(
            &included,
            "[resolver]\nlockfile-path = 'alternate/Cargo.lock'\n",
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"));
        command
            .arg("update")
            .current_dir(&project)
            .env("CARGO_HOME", &home)
            .env_remove("CARGO_RESOLVER_LOCKFILE_PATH");
        if case == "environment" {
            command.env("CARGO_RESOLVER_LOCKFILE_PATH", &alternate);
        }
        if case == "cli" {
            command.args(["--config", "resolver.lockfile-path='alternate/Cargo.lock'"]);
        }
        if case == "cli-file" {
            command.args(["--config", ".cargo/included.toml"]);
        }
        if case == "cli-flag" {
            command.args(["--lockfile-path", "alternate/Cargo.lock"]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{case}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("lockfile-path"),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "original\n");
        assert_eq!(std::fs::read_to_string(&alternate).unwrap(), "alternate\n");
        assert!(!project.join("Cargo.lock.cooldown-hold").exists());
    }
}

#[cfg(unix)]
#[test]
fn symlinked_cargo_config_resolves_includes_from_link_directory() {
    use std::os::unix::fs::symlink;
    let temp = tempdir().unwrap();
    let project = temp.path().join("project");
    let cargo_dir = project.join(".cargo");
    let external = temp.path().join("external");
    let home = temp.path().join("home");
    for path in [&cargo_dir, &external, &home] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(external.join("config.toml"), "include = ['extra.toml']\n").unwrap();
    std::fs::write(external.join("extra.toml"), "").unwrap();
    std::fs::write(
        cargo_dir.join("extra.toml"),
        "[resolver]\nlockfile-path = 'alternate/Cargo.lock'\n",
    )
    .unwrap();
    symlink(external.join("config.toml"), cargo_dir.join("config.toml")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("update")
        .current_dir(&project)
        .env("CARGO_HOME", &home)
        .env_remove("CARGO_RESOLVER_LOCKFILE_PATH")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("lockfile-path"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!project.join("Cargo.lock").exists());
}
