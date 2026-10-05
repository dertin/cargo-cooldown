//! Conservative engine selection and the Cargo process boundary.
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

static CARGO_PROGRAM: OnceLock<PathBuf> = OnceLock::new();
static CARGO_TOOLCHAIN: OnceLock<PathBuf> = OnceLock::new();
static CARGO_VERSION: OnceLock<semver::Version> = OnceLock::new();

/// Pin rustup directory overrides before entering a temporary workspace. An
/// explicit RUSTUP_TOOLCHAIN already pins the launcher, including +toolchain.
pub fn initialize_cargo() -> Result<()> {
    let executable = if cfg!(windows) { "cargo.exe" } else { "cargo" };
    let path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(executable))
            .find(|path| path.is_file())
    });
    let Some(mut path) = path else {
        return Ok(());
    };
    if std::env::var_os("RUSTUP_TOOLCHAIN").is_none() {
        let canonical = std::fs::canonicalize(&path)?;
        let sibling = canonical.with_file_name(if cfg!(windows) {
            "rustup.exe"
        } else {
            "rustup"
        });
        let rustup = if canonical.file_stem().is_some_and(|name| name == "rustup") {
            Some(canonical)
        } else if sibling.is_file() && same_file::is_same_file(&canonical, &sibling)? {
            Some(sibling)
        } else {
            None
        };
        if let Some(rustup) = rustup {
            path = rustup_which(&rustup, "cargo")?;
            // A custom toolchain may borrow Cargo from another toolchain, so
            // derive the selected sysroot from rustc, not from Cargo's path.
            let rustc = rustup_which(&rustup, "rustc")?;
            let toolchain = rustc
                .parent()
                .and_then(Path::parent)
                .context("rustup rustc path has no toolchain root")?;
            let _ = CARGO_TOOLCHAIN.set(toolchain.to_path_buf());
        }
    }
    let _ = CARGO_PROGRAM.set(path);
    Ok(())
}

fn rustup_which(rustup: &Path, tool: &str) -> Result<PathBuf> {
    let output = Command::new(rustup).args(["which", tool]).output()?;
    if !output.status.success() {
        bail!(
            "could not resolve the selected rustup {tool}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let path = PathBuf::from(String::from_utf8(output.stdout)?.trim());
    anyhow::ensure!(
        path.is_absolute(),
        "rustup returned a non-absolute {tool} path"
    );
    Ok(path)
}

pub fn configure_metadata(command: &mut cargo_metadata::MetadataCommand) {
    command.cargo_path(cargo_path());
    if let Some(toolchain) = CARGO_TOOLCHAIN.get() {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
}

pub fn cargo_path() -> PathBuf {
    CARGO_PROGRAM
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("cargo"))
}
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static CARGO_INVOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REPORT_COUNTS: AtomicBool = AtomicBool::new(false);

pub fn record_cargo_invocation() {
    CARGO_INVOCATIONS.fetch_add(1, Ordering::Relaxed);
}
pub fn report_counts_on_exit(verbose: bool) {
    REPORT_COUNTS.store(verbose, Ordering::Relaxed);
}
pub fn report_counts() {
    if REPORT_COUNTS.load(Ordering::Relaxed) {
        eprintln!(
            "cooldown: cargo_invocations={}",
            CARGO_INVOCATIONS.load(Ordering::Relaxed)
        );
    }
}

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::{Config, IncompatiblePublishAgePolicy, LockfileBaselineMode};
use crate::project::ProjectContext;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Auto,
    Filtered,
    Native,
    Legacy,
}

impl Backend {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "filtered" => Ok(Self::Filtered),
            "native" => Ok(Self::Native),
            "legacy" => Ok(Self::Legacy),
            _ => {
                bail!("invalid cooldown backend `{value}`; expected auto, filtered, native, legacy")
            }
        }
    }
}

/// Rustup exports RUSTUP_TOOLCHAIN for `cargo +toolchain cooldown ...`.
/// Keep invoking the same launcher, including PATH shims, in every engine.
pub fn cargo() -> Command {
    record_cargo_invocation();
    let mut command = Command::new(cargo_path());
    if let Some(toolchain) = CARGO_TOOLCHAIN.get() {
        command.env("RUSTUP_TOOLCHAIN", toolchain);
    }
    command
}

/// Disable native filtering so wrapper exceptions remain reachable.
pub fn own_cargo() -> Command {
    let mut command = cargo();
    command.env("CARGO_RESOLVER_INCOMPATIBLE_PUBLISH_AGE", "allow");
    command
}

pub fn own_cargo_with_args(args: &[OsString]) -> Command {
    let mut command = own_cargo();
    let boundary = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    command.args(&args[..boundary]);
    if args
        .first()
        .and_then(|arg| arg.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                "update" | "generate-lockfile" | "check" | "build" | "test" | "run"
            )
        })
    {
        command.args(["--config", "resolver.incompatible-publish-age=\"allow\""]);
    }
    command.args(&args[boundary..]);
    command
}

pub fn cargo_version() -> Result<semver::Version> {
    if let Some(version) = CARGO_VERSION.get() {
        return Ok(version.clone());
    }
    let output = cargo()
        .arg("--version")
        .output()
        .context("querying selected Cargo")?;
    if !output.status.success() {
        bail!("selected Cargo failed --version");
    }
    let text = String::from_utf8(output.stdout)?;
    let version = text
        .split_whitespace()
        .nth(1)
        .context("invalid Cargo version")?;
    let version = semver::Version::parse(version)?;
    let _ = CARGO_VERSION.set(version.clone());
    Ok(version)
}

pub fn native_available() -> Result<bool> {
    let version = cargo_version()?;
    // 1.100 nightlies existed before stabilization. Without a capability probe,
    // require the beta/stable branch or a later minor version on nightly.
    Ok(version.major > 1
        || version.major == 1
            && (version.minor > 100
                || version.minor == 100
                    && (version.pre.is_empty() || version.pre.as_str().starts_with("beta"))))
}

pub fn select(config: &Config, project: &ProjectContext, args: &[OsString]) -> Result<Backend> {
    if config.backend == Backend::Legacy {
        return Ok(Backend::Legacy);
    }
    let supported = crate::filtered::support(project, args);
    let native_policy = native_policy_compatible(config, project, args)
        && crate::filtered::native_config_compatible(project)?;
    if supported.is_ok()
        && native_policy
        && config.backend != Backend::Filtered
        && native_available()?
    {
        return Ok(Backend::Native);
    }
    match config.backend {
        Backend::Native => bail!(
            "native backend requires Cargo >= 1.100 and an equivalent policy (no exceptions, simulated time, skipped registries, or existing baseline)"
        ),
        Backend::Filtered => {
            supported.context("filtered backend unsupported; use backend=legacy")?;
            Ok(Backend::Filtered)
        }
        Backend::Auto if supported.is_ok() => Ok(Backend::Filtered),
        Backend::Auto => {
            tracing::debug!(reason = %supported.unwrap_err(), "using legacy backend");
            Ok(Backend::Legacy)
        }
        Backend::Legacy => Ok(Backend::Legacy),
    }
}

fn native_policy_compatible(config: &Config, project: &ProjectContext, args: &[OsString]) -> bool {
    config.now_override.is_none()
        && config.allow_rules.allow.exact.is_empty()
        && config.allow_rules.allow.package.is_empty()
        && config.allow_rules.allow.global.is_none()
        && config.skip_registries.is_empty()
        && config.registry_min_publish_age.registries.is_empty()
        && config.registry_min_publish_age.crates_io_seconds.is_none()
        && config.incompatible_publish_age == IncompatiblePublishAgePolicy::Deny
        && (!project.workspace_root.join("Cargo.lock").exists()
            || (config.lockfile_baseline == LockfileBaselineMode::Ignore
                && args.first().is_some_and(|arg| arg == "generate-lockfile")))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backend_values_are_strict() {
        for name in ["auto", "filtered", "native", "legacy"] {
            assert!(Backend::parse(name).is_ok());
        }
        assert!(Backend::parse("fast").is_err());
    }
}
