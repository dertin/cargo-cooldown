//! Temporary workspace isolation for speculative Cargo lockfile resolution.
//!
//! Cargo's stable interface does not let us resolve against an alternate
//! lockfile path, so cooldown copies the workspace to a temporary directory and
//! runs Cargo there. The user-visible `Cargo.lock` is held with a sentinel while
//! the temporary lockfile is being updated and cooled.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap_cargo::Manifest;
use tempfile::{Builder, TempDir};
use tracing::{debug, warn};

use crate::project::ProjectContext;

const HELD_LOCKFILE_PREFIX: &str = "cargo-cooldown lockfile hold";

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

        let current_dir = map_current_dir(project, &workspace_root)?;
        let manifest = map_manifest(project, manifest, &workspace_root)?;
        let lockfile_path = workspace_root.join("Cargo.lock");
        let real_lockfile = LockfileHoldGuard::hold(&real_lockfile_path)?;

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
    backup_path: Option<PathBuf>,
    sentinel: String,
    id: String,
    committed: bool,
}

impl LockfileHoldGuard {
    fn hold(lockfile_path: &Path) -> Result<Self> {
        let id = unique_hold_id();
        let backup_candidate =
            lockfile_path.with_file_name(format!("Cargo.lock.cooldown-backup.{id}"));

        let backup_path = if lockfile_path.exists() {
            fs::rename(lockfile_path, &backup_candidate).with_context(|| {
                format!(
                    "failed to hold lockfile {} at {}",
                    lockfile_path.display(),
                    backup_candidate.display()
                )
            })?;
            Some(backup_candidate)
        } else {
            None
        };
        let backup_label = backup_path
            .as_ref()
            .and_then(|path| path.file_name())
            .and_then(OsStr::to_str)
            .unwrap_or("<none>");
        let sentinel = format!(
            "{HELD_LOCKFILE_PREFIX} {id}\n\
             The real Cargo.lock is temporarily held while cargo-cooldown resolves and cools a lockfile in an isolated workspace.\n\
             Backup: {backup_label}\n",
        );

        if let Err(err) = fs::write(lockfile_path, &sentinel) {
            if let Some(backup_path) = &backup_path {
                let _ = fs::rename(backup_path, lockfile_path);
            }
            return Err(err).with_context(|| {
                format!("failed to write lockfile hold {}", lockfile_path.display())
            });
        }

        debug!(
            lockfile = %lockfile_path.display(),
            backup = backup_path.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "<none>".to_string()),
            "held real Cargo.lock while cooling temporary lockfile"
        );

        Ok(Self {
            lockfile_path: lockfile_path.to_path_buf(),
            backup_path,
            sentinel,
            id,
            committed: false,
        })
    }

    fn commit_from(&mut self, source_lockfile: &Path) -> Result<()> {
        self.ensure_sentinel_unchanged()?;
        let final_contents = fs::read(source_lockfile).with_context(|| {
            format!(
                "temporary cooldown workspace did not produce {}",
                source_lockfile.display()
            )
        })?;
        let pending_path = self
            .lockfile_path
            .with_file_name(format!("Cargo.lock.cooldown-final.{}", self.id));
        fs::write(&pending_path, final_contents).with_context(|| {
            format!("failed to stage cooled lockfile {}", pending_path.display())
        })?;

        publish_pending_lockfile(&pending_path, &self.lockfile_path).with_context(|| {
            format!(
                "failed to publish cooled lockfile to {}",
                self.lockfile_path.display()
            )
        })?;
        self.committed = true;

        if let Some(backup_path) = &self.backup_path
            && let Err(err) = fs::remove_file(backup_path)
        {
            warn!(
                backup = %backup_path.display(),
                error = %err,
                "cooled lockfile was published but the temporary lockfile backup could not be removed"
            );
        }

        Ok(())
    }

    fn ensure_sentinel_unchanged(&self) -> Result<()> {
        let contents = fs::read_to_string(&self.lockfile_path).with_context(|| {
            format!(
                "failed to verify lockfile hold {}",
                self.lockfile_path.display()
            )
        })?;
        if contents != self.sentinel {
            bail!(
                "{} changed while cargo-cooldown was resolving in an isolated workspace; refusing to overwrite it. The original lockfile backup is at {}.",
                self.lockfile_path.display(),
                self.backup_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "<none>".to_string())
            );
        }
        Ok(())
    }

    fn restore(&mut self) {
        if self.committed {
            return;
        }

        match fs::read_to_string(&self.lockfile_path) {
            Ok(contents) if contents == self.sentinel => {
                if let Err(err) = fs::remove_file(&self.lockfile_path) {
                    warn!(
                        lockfile = %self.lockfile_path.display(),
                        error = %err,
                        "failed to remove cooldown lockfile hold"
                    );
                    return;
                }
                if let Some(backup_path) = &self.backup_path
                    && let Err(err) = fs::rename(backup_path, &self.lockfile_path)
                {
                    warn!(
                        backup = %backup_path.display(),
                        lockfile = %self.lockfile_path.display(),
                        error = %err,
                        "failed to restore original Cargo.lock after cooldown isolation"
                    );
                }
            }
            Ok(_) => {
                warn!(
                    lockfile = %self.lockfile_path.display(),
                    backup = self.backup_path.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "<none>".to_string()),
                    "Cargo.lock changed while cargo-cooldown held it; leaving the changed file and backup in place"
                );
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if let Some(backup_path) = &self.backup_path
                    && let Err(err) = fs::rename(backup_path, &self.lockfile_path)
                {
                    warn!(
                        backup = %backup_path.display(),
                        lockfile = %self.lockfile_path.display(),
                        error = %err,
                        "failed to restore original Cargo.lock after lockfile hold disappeared"
                    );
                }
            }
            Err(err) => {
                warn!(
                    lockfile = %self.lockfile_path.display(),
                    error = %err,
                    "failed to inspect cooldown lockfile hold during restore"
                );
            }
        }
    }
}

impl Drop for LockfileHoldGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(unix)]
fn publish_pending_lockfile(pending_path: &Path, lockfile_path: &Path) -> Result<()> {
    fs::rename(pending_path, lockfile_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            pending_path.display(),
            lockfile_path.display()
        )
    })
}

#[cfg(windows)]
fn publish_pending_lockfile(pending_path: &Path, lockfile_path: &Path) -> Result<()> {
    fs::remove_file(lockfile_path).with_context(|| {
        format!(
            "failed to remove held lockfile {} before publishing",
            lockfile_path.display()
        )
    })?;
    fs::rename(pending_path, lockfile_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            pending_path.display(),
            lockfile_path.display()
        )
    })
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

fn ensure_no_existing_lockfile_hold(lockfile_path: &Path) -> Result<()> {
    let contents = match fs::read_to_string(lockfile_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to inspect {}", lockfile_path.display()));
        }
    };

    if !contents.starts_with(HELD_LOCKFILE_PREFIX) {
        return Ok(());
    }

    let backup = contents
        .lines()
        .find_map(|line| line.strip_prefix("Backup: "))
        .unwrap_or("<unknown>");
    if backup == "<none>" {
        bail!(
            "{} is a cargo-cooldown hold sentinel from a previous interrupted run. No original lockfile existed before that run; delete the sentinel Cargo.lock before retrying.",
            lockfile_path.display()
        )
    } else {
        bail!(
            "{} is a cargo-cooldown hold sentinel from a previous interrupted run. Restore the original lockfile first by moving {} back to Cargo.lock.",
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

fn should_skip_top_level_workspace_entry(name: &OsStr) -> bool {
    matches!(name.to_str(), Some(".git"))
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

    #[test]
    fn existing_lockfile_hold_sentinel_requires_manual_restore() {
        let temp_dir = tempfile::tempdir().expect("tempdir should build");
        let lockfile_path = temp_dir.path().join("Cargo.lock");
        fs::write(
            &lockfile_path,
            format!("{HELD_LOCKFILE_PREFIX} test\nBackup: Cargo.lock.cooldown-backup.test\n"),
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
}
