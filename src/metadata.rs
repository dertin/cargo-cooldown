//! Thin wrappers around `cargo metadata` with shared selector forwarding.

use anyhow::Result;
use cargo_metadata::Metadata;

use clap_cargo::{Features, Manifest};

/// Read Cargo metadata with the same manifest and feature selectors as the user command.
///
/// This is the unlocked form used when Cargo may refresh derived lockfile data.
/// It returns the resolved dependency graph that cooldown scans for fresh
/// registry packages.
pub fn read_metadata(manifest: &Manifest, features: &Features) -> Result<Metadata> {
    read_metadata_with_locking(manifest, features, false)
}

/// Read Cargo metadata while requiring the lockfile to already be valid.
///
/// The batch solver uses this after rewriting `Cargo.lock` to confirm Cargo can
/// accept the file without changing it again. A failure usually means the
/// proposed assignment violated resolver constraints or needs another pass.
pub fn read_metadata_locked(manifest: &Manifest, features: &Features) -> Result<Metadata> {
    read_metadata_with_locking(manifest, features, true)
}

fn read_metadata_with_locking(
    manifest: &Manifest,
    features: &Features,
    locked: bool,
) -> Result<Metadata> {
    let mut command = manifest.metadata();
    features.forward_metadata(&mut command);
    if locked {
        command.other_options(vec!["--locked".to_string()]);
    }
    let metadata = command.exec()?;
    Ok(metadata)
}
