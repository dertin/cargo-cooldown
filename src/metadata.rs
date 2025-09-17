use anyhow::Result;
use cargo_metadata::{Metadata, MetadataCommand};

pub fn read_metadata() -> Result<Metadata> {
    let metadata = MetadataCommand::new().exec()?;
    Ok(metadata)
}
