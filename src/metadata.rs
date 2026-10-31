//! Thin wrappers around `cargo metadata` with shared selector forwarding.

use anyhow::Result;
use cargo_metadata::Metadata;
use std::path::Path;

use clap_cargo::{Features, Manifest};

/// Read Cargo metadata with the same manifest and feature selectors as the user command.
///
/// This is the unlocked form used when Cargo may refresh derived lockfile data.
/// It returns the resolved dependency graph that cooldown scans for fresh
/// registry packages.
pub fn read_metadata(
    manifest: &Manifest,
    features: &Features,
    lockfile: &Path,
) -> Result<Metadata> {
    read_metadata_with_locking(manifest, features, lockfile, false)
}

/// Read Cargo metadata while requiring the lockfile to already be valid.
///
/// The batch solver uses this after rewriting `Cargo.lock` to confirm Cargo can
/// accept the file without changing it again. A failure usually means the
/// proposed assignment violated resolver constraints or needs another pass.
pub fn read_metadata_locked(
    manifest: &Manifest,
    features: &Features,
    lockfile: &Path,
) -> Result<Metadata> {
    read_metadata_with_locking(manifest, features, lockfile, true)
}

fn read_metadata_with_locking(
    manifest: &Manifest,
    features: &Features,
    lockfile: &Path,
    locked: bool,
) -> Result<Metadata> {
    let mut command = manifest.metadata();
    crate::backend::configure_metadata(&mut command);
    features.forward_metadata(&mut command);
    let mut options = vec![
        "--config".to_string(),
        crate::isolation::lockfile_config(lockfile)?,
    ];
    if locked {
        options.push("--locked".to_string());
    }
    command.other_options(options);
    command.env("CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE", "allow");
    crate::backend::record_cargo_invocation();
    let metadata = command.exec()?;
    Ok(metadata)
}
