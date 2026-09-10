//! Temporary workspace isolation for speculative Cargo lockfile resolution.
//!
//! Cargo's stable interface does not let us resolve against an alternate
//! lockfile path, so cooldown copies the workspace to a temporary directory and
//! runs Cargo there. A separate marker coordinates cooldown processes while the
//! user-visible `Cargo.lock` remains valid for editors and other readers.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap_cargo::Manifest;
use tempfile::{Builder, TempDir};
use tracing::{debug, warn};

use crate::project::ProjectContext;

const LOCKFILE_MARKER_PREFIX: &str = "cargo-cooldown lockfile lock";
const LOCKFILE_MARKER_NAME: &str = "Cargo.lock.cooldown-hold";
const LOCK_ACQUIRE_ATTEMPTS: usize = 600;

/// Workspace copy used for all speculative Cargo operations in one run.
pub struct IsolatedWorkspace {
    _temp_dir: TempDir,
    current_dir: PathBuf,
    manifest: Manifest,
    lockfile_path: PathBuf,
    real_lockfile: LockfileHoldGuard,
}

impl IsolatedWorkspace {
    /// Copy the workspace and hold the real root lockfile.
    pub fn create(project: &ProjectContext, manifest: &Manifest) -> Result<Self> {
        let real_lockfile_path = project.workspace_root.join("Cargo.lock");
        let real_lockfile = LockfileHoldGuard::hold(&real_lockfile_path)?;
        ensure_no_existing_lockfile_hold(&real_lockfile_path)?;

        let temp_dir = Builder::new()
            .prefix("cargo-cooldown-")
            .tempdir()
            .context("failed to create temporary cooldown workspace")?;
        let workspace_root = temp_dir.path().join("workspace");
        copy_workspace(
            &project.workspace_root,
            &workspace_root,
            &project.target_directory,
        )?;
        map_path_dependencies(project, &workspace_root)?;

        let current_dir = map_current_dir(project, &workspace_root)?;
        let manifest = map_manifest(project, manifest, &workspace_root)?;
        let lockfile_path = workspace_root.join("Cargo.lock");
        real_lockfile.ensure_original_unchanged()?;
        match &real_lockfile.original_contents {
            Some(contents) => fs::write(&lockfile_path, contents)?,
            None if lockfile_path.exists() => fs::remove_file(&lockfile_path)?,
            None => {}
        }

        debug!(
            temp_workspace = %workspace_root.display(),
            temp_current_dir = %current_dir.display(),
            "created isolated cooldown workspace"
        );

        Ok(Self {
            _temp_dir: temp_dir,
            current_dir,
            manifest,
            lockfile_path,
            real_lockfile,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    /// Replace any forwarded `--manifest-path` with the temp workspace path.
    pub fn rewrite_cargo_args(&self, args: &[OsString]) -> Vec<OsString> {
        let Some(temp_manifest_path) = &self.manifest.manifest_path else {
            return args.to_vec();
        };

        let mut rewritten = Vec::with_capacity(args.len() + 2);
        let mut replaced = false;
        let mut index = 0;

        while index < args.len() {
            let arg = &args[index];
            if arg == "--manifest-path" {
                rewritten.push(arg.clone());
                rewritten.push(temp_manifest_path.clone().into_os_string());
                index += 2;
                replaced = true;
                continue;
            }

            if let Some(arg_str) = arg.to_str()
                && arg_str.starts_with("--manifest-path=")
            {
                rewritten.push(OsString::from(format!(
                    "--manifest-path={}",
                    temp_manifest_path.display()
                )));
                index += 1;
                replaced = true;
                continue;
            }

            rewritten.push(arg.clone());
            index += 1;
        }

        if replaced {
            return rewritten;
        }

        let mut with_manifest = Vec::with_capacity(rewritten.len() + 2);
        if let Some((command, rest)) = rewritten.split_first() {
            with_manifest.push(command.clone());
            with_manifest.push(OsString::from("--manifest-path"));
            with_manifest.push(temp_manifest_path.clone().into_os_string());
            with_manifest.extend(rest.iter().cloned());
        }
        with_manifest
    }

    /// Publish the cooled temporary lockfile back to the real workspace.
    pub fn publish_lockfile(mut self) -> Result<()> {
        let lockfile_path = self.lockfile_path.clone();
        self.real_lockfile.commit_from(&lockfile_path)
    }
}

/// Temporarily changes the process cwd so Cargo and registry config lookup behave
/// like the user's command, but inside the workspace copy.
pub struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    pub fn enter(path: &Path) -> Result<Self> {
        let previous = env::current_dir().context("failed to capture current directory")?;
        env::set_current_dir(path)
            .with_context(|| format!("failed to enter temporary workspace {}", path.display()))?;
        Ok(Self { previous })
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        if let Err(err) = env::set_current_dir(&self.previous) {
            warn!(
                path = %self.previous.display(),
                error = %err,
                "failed to restore process current directory after cooldown isolation"
            );
        }
    }
}

struct LockfileHoldGuard {
    lockfile_path: PathBuf,
    marker_path: PathBuf,
    original_contents: Option<Vec<u8>>,
    committed: bool,
}

impl LockfileHoldGuard {
    fn hold(lockfile_path: &Path) -> Result<Self> {
        let id = unique_hold_id();
        let marker_path = lockfile_path.with_file_name(LOCKFILE_MARKER_NAME);
        acquire_marker(&marker_path, &id)?;
        let original_contents = match fs::read(lockfile_path) {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                let _ = fs::remove_file(&marker_path);
                return Err(err).with_context(|| {
                    format!("failed to read lockfile {}", lockfile_path.display())
                });
            }
        };

        debug!(
            lockfile = %lockfile_path.display(),
            marker = %marker_path.display(),
            "coordinating isolated cooldown while keeping Cargo.lock valid"
        );

        Ok(Self {
            lockfile_path: lockfile_path.to_path_buf(),
            marker_path,
            original_contents,
            committed: false,
        })
    }

    fn commit_from(&mut self, source_lockfile: &Path) -> Result<()> {
        self.ensure_original_unchanged()?;
        let final_contents = fs::read(source_lockfile).with_context(|| {
            format!(
                "temporary cooldown workspace did not produce {}",
                source_lockfile.display()
            )
        })?;
        let parent = self
            .lockfile_path
            .parent()
            .context("lockfile has no parent")?;
        let mut pending = Builder::new()
            .prefix("Cargo.lock.cooldown-final.")
            .tempfile_in(parent)?;
        pending.write_all(&final_contents)?;
        pending.as_file().sync_all()?;
        // Stage first, then check immediately before the atomic replacement.
        self.ensure_original_unchanged()?;
        pending
            .persist(&self.lockfile_path)
            .context("failed to atomically publish Cargo.lock")?;
        self.committed = true;
        if let Err(err) = fs::remove_file(&self.marker_path) {
            warn!(error = %err, "published Cargo.lock but could not remove coordination marker");
        }

        Ok(())
    }

    fn ensure_original_unchanged(&self) -> Result<()> {
        let contents = match fs::read(&self.lockfile_path) {
            Ok(contents) => Some(contents),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to verify lockfile hold {}",
                        self.lockfile_path.display()
                    )
                });
            }
        };
        if contents != self.original_contents {
            bail!(
                "{} changed while cargo-cooldown was resolving in an isolated workspace; refusing to overwrite it. The auxiliary coordination marker is {}.",
                self.lockfile_path.display(),
                self.marker_path.display()
            );
        }
        Ok(())
    }

    fn restore(&mut self) {
        if self.committed {
            return;
        }

        if let Err(err) = self.ensure_original_unchanged() {
            warn!(
                lockfile = %self.lockfile_path.display(),
                error = %err,
                "leaving externally changed Cargo.lock untouched"
            );
        }
        if let Err(err) = fs::remove_file(&self.marker_path)
            && err.kind() != ErrorKind::NotFound
        {
            warn!(
                marker = %self.marker_path.display(),
                error = %err,
                "failed to remove cooldown coordination marker"
            );
        }
    }
}

impl Drop for LockfileHoldGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn map_manifest(
    project: &ProjectContext,
    manifest: &Manifest,
    temp_workspace_root: &Path,
) -> Result<Manifest> {
    let Some(manifest_path) = &manifest.manifest_path else {
        return Ok(Manifest::default());
    };

    let workspace_root = canonicalize_existing(&project.workspace_root)?;
    let manifest_path = canonicalize_existing(manifest_path)?;
    let relative_manifest = manifest_path.strip_prefix(&workspace_root).with_context(|| {
        format!(
            "manifest {} is outside workspace root {}; cooldown isolation requires workspace-local manifests",
            manifest_path.display(),
            workspace_root.display()
        )
    })?;

    let mut mapped = Manifest::default();
    mapped.manifest_path = Some(temp_workspace_root.join(relative_manifest));
    Ok(mapped)
}

fn map_current_dir(project: &ProjectContext, temp_workspace_root: &Path) -> Result<PathBuf> {
    let workspace_root = canonicalize_existing(&project.workspace_root)?;
    let cwd = canonicalize_existing(&project.cwd)?;
    let current_dir = match cwd.strip_prefix(&workspace_root) {
        Ok(relative) => temp_workspace_root.join(relative),
        Err(_) => temp_workspace_root.to_path_buf(),
    };

    if current_dir.exists() {
        Ok(current_dir)
    } else {
        Ok(temp_workspace_root.to_path_buf())
    }
}

fn canonicalize_existing(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| format!("failed to canonicalize {}", path.display()))
}

fn acquire_marker(marker_path: &Path, id: &str) -> Result<()> {
    for attempt in 0..LOCK_ACQUIRE_ATTEMPTS {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker_path)
        {
            Ok(file) => {
                use std::io::Write;
                let mut file = file;
                if let Err(err) = writeln!(file, "{LOCKFILE_MARKER_PREFIX} {id}") {
                    drop(file);
                    let _ = fs::remove_file(marker_path);
                    return Err(err).context("failed to write cooldown coordination marker");
                }
                return Ok(());
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                if attempt == 0 {
                    eprintln!("Waiting for cargo-cooldown lock: {}", marker_path.display());
                }
                thread::sleep(Duration::from_millis(50 + (attempt.min(15) as u64) * 10));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "failed to acquire cooldown marker {}",
                        marker_path.display()
                    )
                });
            }
        }
    }

    bail!(
        "timed out waiting for {}; another cargo-cooldown process may be interrupted; remove the marker only after confirming no cooldown process is running",
        marker_path.display()
    )
}

fn ensure_no_existing_lockfile_hold(lockfile_path: &Path) -> Result<()> {
    // Older releases wrote an invalid sentinel into Cargo.lock. Keep a clear
    // recovery diagnostic for that state rather than treating it as a valid
    // lockfile or silently overwriting it.
    let contents = match fs::read_to_string(lockfile_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to inspect {}", lockfile_path.display()));
        }
    };

    if !contents.starts_with("cargo-cooldown lockfile hold") {
        return Ok(());
    }

    let backup = contents
        .lines()
        .find_map(|line| line.strip_prefix("Backup: "))
        .unwrap_or("<unknown>");
    if backup == "<none>" {
        bail!(
            "{} is an obsolete cargo-cooldown hold sentinel from a previous interrupted run. No original lockfile backup was recorded; restore Cargo.lock manually before retrying.",
            lockfile_path.display()
        )
    } else {
        bail!(
            "{} is an obsolete cargo-cooldown hold sentinel from a previous interrupted run. Restore the original lockfile first from {}.",
            lockfile_path.display(),
            backup
        )
    }
}

fn copy_workspace(source: &Path, destination: &Path, target_directory: &Path) -> Result<()> {
    fs::create_dir_all(destination).with_context(|| {
        format!(
            "failed to create temporary workspace root {}",
            destination.display()
        )
    })?;
    let target_directory = target_directory_to_skip(source, target_directory);

    for entry in fs::read_dir(source)
        .with_context(|| format!("failed to read workspace root {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        if should_skip_top_level_workspace_entry(&entry.file_name()) {
            continue;
        }
        copy_entry(
            &entry.path(),
            &destination.join(entry.file_name()),
            target_directory.as_deref(),
        )?;
    }

    Ok(())
}

fn copy_entry(source: &Path, destination: &Path, target_directory: Option<&Path>) -> Result<()> {
    if target_directory.is_some_and(|target_directory| source == target_directory) {
        return Ok(());
    }

    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect {}", source.display()))?;
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        copy_symlink(source, destination)
    } else if file_type.is_dir() {
        fs::create_dir_all(destination)
            .with_context(|| format!("failed to create {}", destination.display()))?;
        for entry in fs::read_dir(source)
            .with_context(|| format!("failed to read directory {}", source.display()))?
        {
            let entry =
                entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
            copy_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                target_directory,
            )?;
        }
        Ok(())
    } else if file_type.is_file() {
        fs::copy(source, destination).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                destination.display()
            )
        })?;
        Ok(())
    } else {
        Ok(())
    }
}

/// Rewrite only dependency paths in copied workspace manifests. External
/// checkouts remain at their original absolute locations, preserving their own
/// relative dependencies and workspace inheritance without symlink privileges.
fn map_path_dependencies(project: &ProjectContext, temp_root: &Path) -> Result<()> {
    let source_root = canonicalize_existing(&project.workspace_root)?;
    let mut manifests = vec![source_root.join("Cargo.toml")];
    manifests.extend(
        project
            .members
            .iter()
            .map(|member| member.manifest_path.clone()),
    );
    manifests.sort();
    manifests.dedup();
    for manifest in manifests {
        let manifest = canonicalize_existing(&manifest)?;
        let relative = manifest
            .strip_prefix(&source_root)
            .with_context(|| format!("manifest {} is outside workspace", manifest.display()))?;
        let destination = temp_root.join(relative);
        if fs::symlink_metadata(&destination)?.file_type().is_symlink() {
            bail!(
                "cannot safely rewrite symlinked manifest {}",
                manifest.display()
            );
        }
        let contents = fs::read_to_string(&destination)?;
        let mut value: toml::Value = toml::from_str(&contents)?;
        let parent = manifest.parent().context("manifest has no parent")?;
        rewrite_dependency_tables(&mut value, parent, &source_root, temp_root)
            .with_context(|| format!("cannot map dependencies in {}", manifest.display()))?;
        fs::write(destination, toml::to_string(&value)?)?;
    }
    Ok(())
}

fn rewrite_dependency_tables(
    value: &mut toml::Value,
    manifest_dir: &Path,
    source_root: &Path,
    temp_root: &Path,
) -> Result<()> {
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = value.get_mut(key).and_then(toml::Value::as_table_mut) {
            for (_, dependency) in table.iter_mut() {
                rewrite_dependency_path(dependency, manifest_dir, source_root, temp_root)?;
            }
        }
    }
    for key in ["workspace", "target"] {
        if let Some(section) = value.get_mut(key) {
            if key == "workspace" {
                rewrite_dependency_tables(section, manifest_dir, source_root, temp_root)?;
            } else if let Some(targets) = section.as_table_mut() {
                for (_, target) in targets.iter_mut() {
                    rewrite_dependency_tables(target, manifest_dir, source_root, temp_root)?;
                }
            }
        }
    }
    if let Some(patches) = value.get_mut("patch").and_then(toml::Value::as_table_mut) {
        for registry in patches
            .iter_mut()
            .filter_map(|(_, value)| value.as_table_mut())
        {
            for (_, dependency) in registry.iter_mut() {
                rewrite_dependency_path(dependency, manifest_dir, source_root, temp_root)?;
            }
        }
    }
    if let Some(replacements) = value.get_mut("replace").and_then(toml::Value::as_table_mut) {
        for (_, dependency) in replacements.iter_mut() {
            rewrite_dependency_path(dependency, manifest_dir, source_root, temp_root)?;
        }
    }
    Ok(())
}

fn rewrite_dependency_path(
    dependency: &mut toml::Value,
    manifest_dir: &Path,
    source_root: &Path,
    temp_root: &Path,
) -> Result<()> {
    let Some(path) = dependency.get_mut("path") else {
        return Ok(());
    };
    let raw = path.as_str().context("dependency path must be a string")?;
    let original = canonicalize_existing(&manifest_dir.join(raw))?;
    if !original.join("Cargo.toml").is_file() {
        bail!("path dependency {} has no Cargo.toml", original.display());
    }
    let mapped = match original.strip_prefix(source_root) {
        Ok(relative) => temp_root.join(relative),
        Err(_) => original,
    };
    *path = toml::Value::String(
        mapped
            .to_str()
            .context("dependency path is not UTF-8")?
            .to_owned(),
    );
    Ok(())
}

fn should_skip_top_level_workspace_entry(name: &OsStr) -> bool {
    // Materialize Cargo.lock from the captured bytes, never copy a symlink
    // that could let speculative Cargo operations reach the original file.
    matches!(
        name.to_str(),
        Some(".git" | "Cargo.lock" | LOCKFILE_MARKER_NAME)
    )
}

fn target_directory_to_skip(workspace_root: &Path, target_directory: &Path) -> Option<PathBuf> {
    let target_directory = if target_directory.is_absolute() {
        target_directory.to_path_buf()
    } else {
        workspace_root.join(target_directory)
    };
    target_directory
        .starts_with(workspace_root)
        .then_some(target_directory)
}

#[cfg(unix)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)
        .with_context(|| format!("failed to read symlink {}", source.display()))?;
    std::os::unix::fs::symlink(&target, destination).with_context(|| {
        format!(
            "failed to copy symlink {} to {}",
            source.display(),
            destination.display()
        )
    })
}

#[cfg(windows)]
fn copy_symlink(source: &Path, destination: &Path) -> Result<()> {
    let target = fs::read_link(source)
        .with_context(|| format!("failed to read symlink {}", source.display()))?;
    let source_metadata = fs::metadata(source)
        .with_context(|| format!("failed to inspect symlink target {}", source.display()))?;
    if source_metadata.is_dir() {
        std::os::windows::fs::symlink_dir(&target, destination)
    } else {
        std::os::windows::fs::symlink_file(&target, destination)
    }
    .with_context(|| {
        format!(
            "failed to copy symlink {} to {}",
            source.display(),
            destination.display()
        )
    })
}

fn unique_hold_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{}-{nanos}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project_at(root: &Path) -> ProjectContext {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='app'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        ProjectContext {
            cwd: root.to_path_buf(),
            kind: crate::project::ProjectKind::Crate,
            workspace_root: root.to_path_buf(),
            target_directory: root.join("target"),
            members: vec![],
            active_member: None,
        }
    }

    #[test]
    fn external_change_after_isolation_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let project = project_at(temp.path());
        let real = temp.path().join("Cargo.lock");
        fs::write(&real, "version = 4\n# original\n").unwrap();
        let isolated = IsolatedWorkspace::create(&project, &Manifest::default()).unwrap();
        let changed = "version = 4\n# external change\n";
        fs::write(&real, changed).unwrap();
        assert!(isolated.publish_lockfile().is_err());
        assert_eq!(fs::read_to_string(real).unwrap(), changed);
        assert!(!temp.path().join(LOCKFILE_MARKER_NAME).exists());
    }

    #[test]
    fn waiting_instance_copies_the_published_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let project = project_at(temp.path());
        fs::write(temp.path().join("Cargo.lock"), "version = 4\n# old\n").unwrap();
        let first = IsolatedWorkspace::create(&project, &Manifest::default()).unwrap();
        let second = thread::spawn(move || {
            IsolatedWorkspace::create(&project, &Manifest::default()).unwrap()
        });
        thread::sleep(Duration::from_millis(100));
        assert!(!second.is_finished());
        let updated = "version = 4\n# published\n";
        fs::write(&first.lockfile_path, updated).unwrap();
        first.publish_lockfile().unwrap();
        let second = second.join().unwrap();
        assert_eq!(fs::read_to_string(&second.lockfile_path).unwrap(), updated);
        second.publish_lockfile().unwrap();
    }

    #[test]
    fn publication_without_initial_lockfile_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let project = project_at(temp.path());
        let isolated = IsolatedWorkspace::create(&project, &Manifest::default()).unwrap();
        fs::write(&isolated.lockfile_path, "version = 4\n").unwrap();
        isolated.publish_lockfile().unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("Cargo.lock")).unwrap(),
            "version = 4\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_original_lockfile_is_not_followed_by_speculative_writes() {
        let temp = tempfile::tempdir().unwrap();
        let project = project_at(&temp.path().join("project"));
        let shared = temp.path().join("shared.lock");
        fs::write(&shared, "version = 4\n# original\n").unwrap();
        std::os::unix::fs::symlink(&shared, project.workspace_root.join("Cargo.lock")).unwrap();
        let isolated = IsolatedWorkspace::create(&project, &Manifest::default()).unwrap();
        assert!(
            !fs::symlink_metadata(&isolated.lockfile_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::write(&isolated.lockfile_path, "version = 4\n# speculative\n").unwrap();
        assert_eq!(
            fs::read_to_string(shared).unwrap(),
            "version = 4\n# original\n"
        );
    }

    #[test]
    fn lock_holder_child_process() {
        let Some(root) = env::var_os("COOLDOWN_TEST_LOCK_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let _guard = LockfileHoldGuard::hold(&root.join("Cargo.lock")).unwrap();
        fs::write(root.join("ready"), "").unwrap();
        loop {
            thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn interrupted_process_preserves_lockfile_and_allows_manual_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let lockfile = temp.path().join("Cargo.lock");
        fs::write(&lockfile, "version = 4\n").unwrap();
        let mut child = std::process::Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "isolation::tests::lock_holder_child_process",
                "--nocapture",
            ])
            .env("COOLDOWN_TEST_LOCK_ROOT", temp.path())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !temp.path().join("ready").exists() {
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child did not acquire lock");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(fs::read_to_string(&lockfile).unwrap(), "version = 4\n");
        fs::remove_file(temp.path().join(LOCKFILE_MARKER_NAME)).unwrap();
        let guard = LockfileHoldGuard::hold(&lockfile).unwrap();
        drop(guard);
    }

    #[test]
    fn existing_lockfile_hold_sentinel_requires_manual_restore() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let lockfile_path = temp_dir.path().join("Cargo.lock");
        fs::write(
            &lockfile_path,
            "cargo-cooldown lockfile hold test\nBackup: Cargo.lock.cooldown-backup.test\n",
        )
        .expect("sentinel should be writable");

        let err = ensure_no_existing_lockfile_hold(&lockfile_path).unwrap_err();

        assert!(
            format!("{err:#}").contains("previous interrupted run"),
            "{err:#}"
        );
    }

    #[test]
    fn workspace_copy_skips_cargo_metadata_target_directory() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let source = temp_dir.path().join("source");
        let destination = temp_dir.path().join("destination");
        let target_directory = source.join("target");
        fs::create_dir_all(source.join(".git")).expect("top-level git dir should be creatable");
        fs::create_dir_all(source.join("target"))
            .expect("top-level target dir should be creatable");
        fs::create_dir_all(source.join("fixtures/target"))
            .expect("nested target dir should be creatable");
        fs::write(source.join(".git/config"), "").expect("git config should be writable");
        fs::write(source.join("target/cache"), "").expect("target cache should be writable");
        fs::write(source.join("fixtures/target/keep.txt"), "keep")
            .expect("nested fixture should be writable");

        copy_workspace(&source, &destination, &target_directory).expect("workspace should copy");

        assert!(!destination.join(".git").exists());
        assert!(!destination.join("target").exists());
        assert_eq!(
            fs::read_to_string(destination.join("fixtures/target/keep.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn workspace_copy_keeps_target_directory_when_cargo_target_is_external() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let source = temp_dir.path().join("source");
        let destination = temp_dir.path().join("destination");
        let external_target_directory = temp_dir.path().join("shared-target");
        fs::create_dir_all(source.join("target")).expect("project target dir should be creatable");
        fs::write(source.join("target/fixture.txt"), "keep")
            .expect("project target fixture should be writable");

        copy_workspace(&source, &destination, &external_target_directory)
            .expect("workspace should copy");

        assert_eq!(
            fs::read_to_string(destination.join("target/fixture.txt")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn lockfile_hold_keeps_visible_lockfile_valid_and_publishes_atomically() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let lockfile = temp_dir.path().join("Cargo.lock");
        let replacement = temp_dir.path().join("replacement.lock");
        fs::write(&lockfile, "version = 4\n").expect("lockfile should be writable");
        fs::write(
            &replacement,
            "version = 4\n\n[[package]]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )
        .expect("replacement should be writable");

        let mut hold = LockfileHoldGuard::hold(&lockfile).expect("hold should succeed");
        assert_eq!(fs::read_to_string(&lockfile).unwrap(), "version = 4\n");
        assert!(hold.marker_path.exists());
        hold.commit_from(&replacement)
            .expect("publish should succeed");
        assert!(!hold.marker_path.exists());
        drop(hold);
        assert!(!temp_dir.path().join(LOCKFILE_MARKER_NAME).exists());
        assert!(fs::read_to_string(&lockfile).unwrap().contains("demo"));
    }

    #[test]
    fn marker_acquisition_waits_for_previous_cooldown() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let marker = temp_dir.path().join(LOCKFILE_MARKER_NAME);
        fs::write(&marker, "previous run").expect("marker should be writable");
        let release_marker = marker.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(70));
            fs::remove_file(release_marker).expect("previous marker should be removable");
        });

        acquire_marker(&marker, "test").expect("acquisition should wait and succeed");
        assert!(marker.exists());
        fs::remove_file(marker).expect("test marker should be removable");
    }

    #[test]
    fn dependency_paths_are_rewritten_without_writing_outside_temp() {
        let temp = tempfile::tempdir().unwrap();
        let original = temp.path().join("original/a/b");
        let external = temp.path().join("original/external");
        let isolated = temp.path().join("sandbox/workspace");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir_all(&external).unwrap();
        fs::write(
            external.join("Cargo.toml"),
            "[package]\nname='external'\nversion='0.1.0'",
        )
        .unwrap();
        let mut value: toml::Value = toml::from_str("[dependencies]\nexternal={path='../../external'}\n[[bin]]\nname='app'\npath='not-a-dependency.rs'").unwrap();
        rewrite_dependency_tables(&mut value, &original, &original, &isolated).unwrap();
        assert_eq!(
            value["dependencies"]["external"]["path"].as_str().unwrap(),
            fs::canonicalize(&external).unwrap().to_str().unwrap()
        );
        assert_eq!(
            value["bin"][0]["path"].as_str().unwrap(),
            "not-a-dependency.rs"
        );
        assert!(!temp.path().join("external").exists());
    }
}
