//! Real Cargo coverage for external local checkouts on Unix and Windows.
use std::{fs, path::Path, process::Command};

fn crate_at(path: &Path, name: &str, extra: &str) {
    fs::create_dir_all(path.join("src")).unwrap();
    fs::write(
        path.join("Cargo.toml"),
        format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n{extra}"),
    )
    .unwrap();
    fs::write(path.join("src/lib.rs"), "").unwrap();
}

#[test]
fn reader_process() {
    let Some(root) = std::env::var_os("COOLDOWN_READER_ROOT") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let mut reads = 0usize;
    while !root.join("reader-stop").exists() {
        let contents = fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let value: toml::Value = toml::from_str(&contents).unwrap();
        assert!(value["version"].as_integer().is_some());
        reads += 1;
        fs::write(root.join("reader-ready"), "").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(reads > 0);
}

#[test]
fn external_dependency_retains_its_workspace_inheritance() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    let external = temp.path().join("checkout");
    crate_at(
        &root,
        "app",
        "[dependencies]\nexternal={path='../checkout/member'}\n",
    );
    fs::create_dir_all(external.join("member/src")).unwrap();
    fs::write(
        external.join("Cargo.toml"),
        "[workspace]\nmembers=['member']\n[workspace.package]\nversion='0.7.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(
        external.join("member/Cargo.toml"),
        "[package]\nname='external'\nversion.workspace=true\nedition.workspace=true\n",
    )
    .unwrap();
    fs::write(external.join("member/src/lib.rs"), "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("update")
        .current_dir(&root)
        .env("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", "7 days")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read_to_string(root.join("Cargo.lock"))
            .unwrap()
            .contains("0.7.0")
    );
    assert!(!external.join("Cargo.lock").exists());
}

#[test]
fn excluded_local_package_can_depend_on_an_external_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("app");
    let external = temp.path().join("external");
    crate_at(&external, "external", "");
    crate_at(
        &root.join("local"),
        "local",
        "[workspace]\n[dependencies]\nexternal={path='../../external'}\n",
    );
    crate_at(
        &root,
        "app",
        "[workspace]\nexclude=['local']\n[dependencies]\nlocal={path='local'}\n",
    );
    let manifests = [
        root.join("Cargo.toml"),
        root.join("local/Cargo.toml"),
        external.join("Cargo.toml"),
    ];
    let originals = manifests.each_ref().map(|path| fs::read(path).unwrap());
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
        .arg("update")
        .current_dir(&root)
        .env("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", "7 days")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("Cargo.lock")).unwrap()).unwrap();
    assert!(lock["package"].as_array().unwrap().iter().any(|package| {
        package["name"].as_str() == Some("external")
            && package["version"].as_str() == Some("0.1.0")
            && package.get("source").is_none()
    }));
    for (manifest, original) in manifests.iter().zip(originals) {
        assert_eq!(
            fs::read(manifest).unwrap(),
            original,
            "{} changed",
            manifest.display()
        );
        assert!(
            !fs::symlink_metadata(manifest)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    assert!(!external.join("Cargo.lock").exists());
    assert!(!root.join("local/Cargo.lock").exists());
    assert!(!external.join("Cargo.lock.cooldown-hold").exists());
    assert!(
        !fs::symlink_metadata(&external)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let metadata = Command::new("cargo")
        .args(["metadata", "--locked", "--offline", "--format-version", "1"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        metadata.status.success(),
        "{}",
        String::from_utf8_lossy(&metadata.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
    let resolved = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "external")
        .unwrap();
    assert_eq!(
        fs::canonicalize(resolved["manifest_path"].as_str().unwrap()).unwrap(),
        fs::canonicalize(external.join("Cargo.toml")).unwrap()
    );
}

#[test]
fn concurrent_commands_keep_lockfile_parseable_to_another_process() {
    let temp = tempfile::tempdir().unwrap();
    crate_at(temp.path(), "app", "");
    let initial = Command::new("cargo")
        .arg("generate-lockfile")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(initial.status.success());
    let mut reader = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reader_process"])
        .env("COOLDOWN_READER_ROOT", temp.path())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !temp.path().join("reader-ready").exists() {
        if std::time::Instant::now() > deadline {
            let _ = reader.kill();
            let _ = reader.wait();
            panic!("reader did not become ready");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut children = vec![];
    for command in ["check", "update"] {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
                .arg(command)
                .current_dir(temp.path())
                .env("CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE", "7 days")
                .env("CARGO_TARGET_DIR", temp.path().join("target"))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    fs::write(temp.path().join("reader-stop"), "").unwrap();
    assert!(reader.wait().unwrap().success());
    for output in outputs {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!temp.path().join("Cargo.lock.cooldown-hold").exists());
}

#[test]
fn external_paths_work_from_root_and_member_including_transitives_and_absolute_paths() {
    for member in [false, true] {
        for absolute in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("project");
            let external = temp.path().join("external");
            crate_at(&temp.path().join("nested"), "nested", "");
            crate_at(
                &external,
                "external",
                "[dependencies]\nnested={path='../nested'}\n",
            );
            let app = if member {
                root.join("app")
            } else {
                root.clone()
            };
            let path = if absolute {
                external.to_str().unwrap().to_owned()
            } else if member {
                "../../external".into()
            } else {
                "../external".into()
            };
            let encoded = toml::Value::String(path).to_string();
            crate_at(
                &app,
                "app",
                &format!("[dependencies]\nexternal={{path={encoded}}}\n"),
            );
            if member {
                fs::write(
                    root.join("Cargo.toml"),
                    "[workspace]\nmembers=['app']\nresolver='2'\n",
                )
                .unwrap();
            }
            fs::write(
                root.join("cooldown.toml"),
                "[registry]\nglobal-min-publish-age='7 days'\n",
            )
            .unwrap();
            // Changes after configuration must be visible to the next resolution.
            let manifest = external.join("Cargo.toml");
            let updated = fs::read_to_string(&manifest)
                .unwrap()
                .replace("0.1.0", "0.2.0");
            fs::write(&manifest, &updated).unwrap();
            for command in ["check", "update"] {
                let output = Command::new(env!("CARGO_BIN_EXE_cargo-cooldown"))
                    .args([command, "--manifest-path"])
                    .arg(app.join("Cargo.toml"))
                    .current_dir(temp.path())
                    .env("CARGO_TARGET_DIR", temp.path().join("target"))
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{member}/{absolute}/{command}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let metadata = Command::new("cargo")
                    .args(["metadata", "--locked", "--offline", "--format-version", "1"])
                    .arg("--manifest-path")
                    .arg(app.join("Cargo.toml"))
                    .output()
                    .unwrap();
                assert!(
                    metadata.status.success(),
                    "{}",
                    String::from_utf8_lossy(&metadata.stderr)
                );
                let value: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
                assert!(
                    value["packages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|p| p["name"] == "external" && p["version"] == "0.2.0")
                );
            }
            assert_eq!(fs::read_to_string(manifest).unwrap(), updated);
            assert!(!external.join("Cargo.lock").exists());
        }
    }
}
