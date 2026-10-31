//! Reject Cargo lockfile overrides until their publication target can be preserved.
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

pub fn ensure_default_lockfile(cwd: &Path, args: &[OsString]) -> Result<()> {
    ensure!(
        std::env::var_os("CARGO_RESOLVER_LOCKFILE_PATH").is_none(),
        "custom Cargo resolver.lockfile-path is not supported; unset CARGO_RESOLVER_LOCKFILE_PATH"
    );
    let mut files = Vec::new();
    if let Some(home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")))
    {
        files.push(cwd.join(home));
    }
    files.extend(cwd.ancestors().map(|parent| parent.join(".cargo")));
    let mut seen = HashSet::new();
    for dir in files {
        let legacy = dir.join("config");
        let path = if legacy.exists() {
            legacy
        } else {
            dir.join("config.toml")
        };
        if path.exists() {
            check_file(&path, &mut seen)?;
        }
    }
    let mut iter = args.iter().take_while(|arg| *arg != "--");
    while let Some(arg) = iter.next() {
        if arg == "--lockfile-path"
            || arg
                .to_str()
                .is_some_and(|arg| arg.starts_with("--lockfile-path="))
        {
            bail!("custom Cargo --lockfile-path is not supported by cargo-cooldown");
        }
        let value = if arg == "--config" {
            Some(
                iter.next()
                    .context("missing value for Cargo --config")?
                    .as_os_str(),
            )
        } else {
            arg.to_str()
                .and_then(|arg| arg.strip_prefix("--config="))
                .map(std::ffi::OsStr::new)
        };
        if let Some(value) = value {
            let value = value.to_str().context("Cargo --config must be UTF-8")?;
            if value.contains('=') {
                let table: toml::Table =
                    toml::from_str(value).context("invalid Cargo --config override")?;
                check_table(&table, cwd, &mut seen)?;
            } else {
                check_file(&cwd.join(value), &mut seen)?;
            }
        }
    }
    Ok(())
}

fn check_file(path: &Path, seen: &mut HashSet<PathBuf>) -> Result<()> {
    let identity = fs::canonicalize(path)
        .with_context(|| format!("cannot inspect Cargo configuration {}", path.display()))?;
    ensure!(
        seen.len() < 64,
        "Cargo configuration include nesting is too deep"
    );
    ensure!(
        seen.insert(identity.clone()),
        "Cargo configuration include cycle"
    );
    let table: toml::Table = toml::from_str(&fs::read_to_string(path)?)
        .with_context(|| format!("invalid Cargo configuration {}", path.display()))?;
    let result = check_table(
        &table,
        path.parent().context("Cargo configuration has no parent")?,
        seen,
    );
    seen.remove(&identity);
    result
}

fn check_table(table: &toml::Table, base: &Path, seen: &mut HashSet<PathBuf>) -> Result<()> {
    if table
        .get("resolver")
        .and_then(|value| value.get("lockfile-path"))
        .is_some()
    {
        bail!("custom Cargo resolver.lockfile-path is not supported by cargo-cooldown");
    }
    if let Some(includes) = table.get("include") {
        let includes = includes
            .as_array()
            .context("Cargo configuration include must be an array")?;
        for include in includes {
            let (path, optional) = if let Some(path) = include.as_str() {
                (path, false)
            } else {
                let table = include
                    .as_table()
                    .context("invalid Cargo configuration include")?;
                let path = table
                    .get("path")
                    .and_then(|v| v.as_str())
                    .context("Cargo configuration include is missing a path")?;
                let optional = table
                    .get("optional")
                    .map(|v| v.as_bool().context("invalid optional Cargo include"))
                    .transpose()?
                    .unwrap_or(false);
                (path, optional)
            };
            let path = base.join(path);
            if optional && !path.exists() {
                continue;
            }
            check_file(&path, seen)?;
        }
    }
    Ok(())
}
