use anyhow::Result;
use cargo_metadata::Metadata;

use clap_cargo::{Features, Manifest};

pub fn read_metadata(manifest: &Manifest, features: &Features) -> Result<Metadata> {
    read_metadata_with_locking(manifest, features, false)
}

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
