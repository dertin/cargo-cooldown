//! Temporary lockfile isolation for speculative Cargo resolution.
//!
//! Resolve using a temporary Cargo.lock while coordinating publication of the
//! user-visible lockfile. Cargo 1.97 or newer is required for this isolation.

use std::ffi::OsString;
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

/// Temporary lockfile used for all speculative Cargo operations in one run.
pub struct IsolatedWorkspace {
    _temp_dir: TempDir,
    current_dir: PathBuf,
    manifest: Manifest,
    lockfile_path: PathBuf,
    real_lockfile: LockfileHoldGuard,
}

impl IsolatedWorkspace {
    /// Capture the root lockfile and create an isolated candidate.
    pub fn create(project: &ProjectContext, manifest: &Manifest) -> Result<Self> {
        let real_lockfile_path = project.workspace_root.join("Cargo.lock");
        let real_lockfile = LockfileHoldGuard::hold(&real_lockfile_path)?;
        ensure_no_existing_lockfile_hold(&real_lockfile_path)?;

        let temp_dir = Builder::new()
            .prefix("cargo-cooldown-")
            .tempdir()
            .context("failed to create temporary cooldown workspace")?;
        let current_dir = project.cwd.clone();
        let manifest = manifest.clone();
        let lockfile_path = temp_dir.path().join("Cargo.lock");
        real_lockfile.ensure_original_unchanged()?;
        match &real_lockfile.original_contents {
            Some(contents) => fs::write(&lockfile_path, contents)?,
            None if lockfile_path.exists() => fs::remove_file(&lockfile_path)?,
            None => {}
        }

        debug!(
            temp_lockfile = %lockfile_path.display(),
            temp_current_dir = %current_dir.display(),
            "created isolated cooldown lockfile"
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

    pub fn lockfile_path(&self) -> &Path {
        &self.lockfile_path
    }

    pub fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    /// Override the lockfile after forwarded Cargo config arguments.
    pub fn cargo_args(&self, args: &[OsString]) -> Result<Vec<OsString>> {
        let boundary = args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(args.len());
        let mut result = args[..boundary].to_vec();
        let has_manifest = result.iter().any(|arg| {
            arg == "--manifest-path"
                || arg
                    .to_str()
                    .is_some_and(|arg| arg.starts_with("--manifest-path="))
        });
        if !has_manifest && let Some(path) = &self.manifest.manifest_path {
            result.push(OsString::from("--manifest-path"));
            result.push(path.clone().into_os_string());
        }
        result.push(OsString::from("--config"));
        result.push(OsString::from(lockfile_config(&self.lockfile_path)?));
        result.extend_from_slice(&args[boundary..]);
        Ok(result)
    }

    /// Reset a failed candidate while retaining the original coordination hold.
    pub fn restore_initial_lockfile(&self) -> Result<()> {
        self.real_lockfile.ensure_original_unchanged()?;
        match &self.real_lockfile.original_contents {
            Some(contents) => fs::write(&self.lockfile_path, contents)?,
            None => match fs::remove_file(&self.lockfile_path) {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            },
        }
        Ok(())
    }

    /// Publish the cooled temporary lockfile back to the real workspace.
    pub fn publish_lockfile(mut self) -> Result<()> {
        let lockfile_path = self.lockfile_path.clone();
        self.real_lockfile.commit_from(&lockfile_path)
    }
}

pub fn lockfile_config(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .context("temporary lockfile path is not UTF-8")?;
    Ok(format!(
        "resolver.lockfile-path={}",
        toml::Value::String(path.to_owned())
    ))
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
        if self.original_contents.as_deref() == Some(final_contents.as_slice()) {
            self.ensure_original_unchanged()?;
            self.finish_commit();
            return Ok(());
        }
        let parent = self
            .lockfile_path
            .parent()
            .context("lockfile has no parent")?;
        let mut builder = Builder::new();
        builder.prefix("Cargo.lock.cooldown-final.");
        // For new lockfiles use normal file creation permissions, respecting umask.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o666));
        }
        let mut pending = builder.tempfile_in(parent)?;
        pending.write_all(&final_contents)?;
        if self.original_contents.is_some() {
            pending
                .as_file()
                .set_permissions(fs::metadata(&self.lockfile_path)?.permissions())?;
        }
        pending.as_file().sync_all()?;
        // Windows readers may temporarily deny replacement. Keep the pending
        // file and recheck the original before every bounded atomic retry.
        let mut retries = 0;
        loop {
            self.ensure_original_unchanged()?;
            match pending.persist(&self.lockfile_path) {
                Ok(_) => break,
                Err(err)
                    if cfg!(windows)
                        && retries < 100
                        && matches!(err.error.raw_os_error(), Some(5 | 32 | 33)) =>
                {
                    pending = err.file;
                    retries += 1;
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err).context("failed to atomically publish Cargo.lock"),
            }
        }
        self.finish_commit();
        Ok(())
    }

    fn finish_commit(&mut self) {
        self.committed = true;
        if let Err(err) = fs::remove_file(&self.marker_path) {
            warn!(error = %err, "completed cooldown but could not remove coordination marker");
        }
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
                "{} changed while cargo-cooldown was resolving with a temporary lockfile; refusing to overwrite it. The auxiliary coordination marker is {}.",
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
    use std::env;

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
            members: vec![],
            active_member: None,
        }
    }

    #[test]
    fn resetting_candidate_retains_coordination_and_exact_baseline() {
        for original in [None, Some("version = 4\n# original\n")] {
            let temp = tempfile::tempdir().unwrap();
            let project = project_at(temp.path());
            let real = temp.path().join("Cargo.lock");
            if let Some(contents) = original {
                fs::write(&real, contents).unwrap();
            }
            let isolated = IsolatedWorkspace::create(&project, &Manifest::default()).unwrap();
            let marker = temp.path().join(LOCKFILE_MARKER_NAME);
            let held = fs::read(&marker).unwrap();
            fs::write(isolated.lockfile_path(), "partial candidate").unwrap();
            isolated.restore_initial_lockfile().unwrap();
            assert_eq!(fs::read(&marker).unwrap(), held);
            assert_eq!(
                fs::read_to_string(isolated.lockfile_path()).ok().as_deref(),
                original
            );
            assert_eq!(fs::read_to_string(&real).ok().as_deref(), original);
            fs::write(&real, "external writer").unwrap();
            assert!(isolated.restore_initial_lockfile().is_err());
            assert_eq!(fs::read_to_string(&real).unwrap(), "external writer");
            drop(isolated);
            assert!(!marker.exists());
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

    #[cfg(unix)]
    #[test]
    fn publication_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o644, 0o664, 0o444] {
            let temp = tempfile::tempdir().unwrap();
            let lock = temp.path().join("Cargo.lock");
            let replacement = temp.path().join("replacement");
            fs::write(&lock, "version = 4\n").unwrap();
            fs::write(&replacement, "version = 4\n# updated\n").unwrap();
            fs::set_permissions(&lock, fs::Permissions::from_mode(mode)).unwrap();
            let mut guard = LockfileHoldGuard::hold(&lock).unwrap();
            guard.commit_from(&replacement).unwrap();
            assert_eq!(
                fs::metadata(lock).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
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
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
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
    fn unchanged_lockfile_keeps_the_original_file() {
        let temp = tempfile::tempdir().unwrap();
        let lockfile = temp.path().join("Cargo.lock");
        let candidate = temp.path().join("candidate");
        let alias = temp.path().join("original-file");
        fs::write(&lockfile, "version = 4\n").unwrap();
        fs::copy(&lockfile, &candidate).unwrap();
        fs::hard_link(&lockfile, &alias).unwrap();
        let mut hold = LockfileHoldGuard::hold(&lockfile).unwrap();
        hold.commit_from(&candidate).unwrap();
        assert!(same_file::is_same_file(&lockfile, &alias).unwrap());
        assert!(!hold.marker_path.exists());
    }

    #[cfg(windows)]
    #[test]
    fn publication_waits_for_windows_reader_and_rechecks_external_changes() {
        use std::os::windows::fs::OpenOptionsExt;
        for external_change in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let lockfile = temp.path().join("Cargo.lock");
            let candidate = temp.path().join("candidate");
            fs::write(&lockfile, "version = 4\n").unwrap();
            fs::write(&candidate, "version = 4\n# candidate\n").unwrap();
            let mut hold = LockfileHoldGuard::hold(&lockfile).unwrap();
            // Allow reads/writes but prevent deletion while the reader is open.
            let reader = fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(&lockfile)
                .unwrap();
            let path = lockfile.clone();
            let release = thread::spawn(move || {
                thread::sleep(Duration::from_millis(80));
                if external_change {
                    fs::write(path, "version = 4\n# external\n").unwrap();
                }
                drop(reader);
            });
            let result = hold.commit_from(&candidate);
            release.join().unwrap();
            if external_change {
                assert!(result.unwrap_err().to_string().contains("changed while"));
                assert_eq!(
                    fs::read_to_string(&lockfile).unwrap(),
                    "version = 4\n# external\n"
                );
            } else {
                result.unwrap();
                assert_eq!(
                    fs::read_to_string(&lockfile).unwrap(),
                    "version = 4\n# candidate\n"
                );
            }
        }
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
}
