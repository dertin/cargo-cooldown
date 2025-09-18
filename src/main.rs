mod allowlist;
mod cache;
mod config;
mod executor;
mod metadata;
mod registry;
mod resolver;

use std::ffi::OsString;
use std::process::Command;

use anyhow::Result;
use clap::Parser;
use clap_cargo::{Features, Manifest, Workspace};
use tracing::warn;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, Mode};

#[derive(Debug, Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(
        name = "cooldown",
        about = "Cargo wrapper that enforces a cooldown window for freshly published crates on crates.io.",
        disable_help_subcommand = true,
        arg_required_else_help = true,
        styles = clap_cargo::style::CLAP_STYLING
    )]
    Cooldown(Cli),
}

#[derive(Debug, Parser)]
struct Cli {
    #[command(flatten)]
    manifest: Manifest,
    #[command(flatten)]
    workspace: Workspace,
    #[command(flatten)]
    features: Features,
    #[arg(
        value_name = "CARGO_ARG",
        trailing_var_arg = true,
        num_args = 1..,
        allow_hyphen_values = true,
        help = "Cargo subcommand and params to forward after cooldown checks (build/check/test/run; avoid `cargo update`)."
    )]
    cargo_args: Vec<OsString>,
}

fn init_logging(verbose: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            EnvFilter::new("cargo_cooldown=debug,cargo_cooldown::executor=debug,info")
        } else {
            EnvFilter::new("info")
        }
    });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}

fn parse_cli(raw_args: &[OsString]) -> Cli {
    match CargoCli::try_parse_from(raw_args.iter().cloned()) {
        Ok(CargoCli::Cooldown(cli)) => cli,
        Err(original_err) => {
            if raw_args.len() > 1 && raw_args.get(1).map(|arg| arg != "cooldown").unwrap_or(true) {
                let mut patched = Vec::with_capacity(raw_args.len() + 1);
                if let Some(first) = raw_args.first() {
                    patched.push(first.clone());
                }
                patched.push(OsString::from("cooldown"));
                patched.extend(raw_args.iter().skip(1).cloned());
                match CargoCli::try_parse_from(patched) {
                    Ok(CargoCli::Cooldown(cli)) => cli,
                    Err(err) => err.exit(),
                }
            } else {
                original_err.exit()
            }
        }
    }
}

/// Canonicalize the Cargo invocation so the subcommand leads and the selectors
/// parsed by clap-cargo (`--manifest-path`, `--package`, feature flags, etc.)
/// are re-applied in the order that upstream `cargo` expects.
fn assemble_cargo_args(cli: &Cli) -> Vec<OsString> {
    let mut args = Vec::new();
    let mut cargo_iter = cli.cargo_args.iter();
    let command = cargo_iter.next().cloned().expect("cargo command required");

    args.push(command);

    if let Some(path) = &cli.manifest.manifest_path {
        args.push(OsString::from("--manifest-path"));
        args.push(path.into());
    }

    for package in &cli.workspace.package {
        args.push(OsString::from("--package"));
        args.push(OsString::from(package));
    }

    if cli.workspace.workspace {
        args.push(OsString::from("--workspace"));
    }

    if cli.workspace.all {
        args.push(OsString::from("--all"));
    }

    for exclude in &cli.workspace.exclude {
        args.push(OsString::from("--exclude"));
        args.push(OsString::from(exclude));
    }

    if cli.features.all_features {
        args.push(OsString::from("--all-features"));
    }

    if cli.features.no_default_features {
        args.push(OsString::from("--no-default-features"));
    }

    if !cli.features.features.is_empty() {
        args.push(OsString::from("--features"));
        let merged = cli
            .features
            .features
            .iter()
            .flat_map(|value| split_features(value))
            .collect::<Vec<_>>()
            .join(",");
        args.push(OsString::from(merged));
    }

    args.extend(cargo_iter.cloned());

    args
}

fn split_features(raw: &str) -> Vec<String> {
    raw.split([' ', ','])
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = parse_cli(&raw_args);
    let config = Config::from_env();
    init_logging(config.verbose);

    let forwarded_args = assemble_cargo_args(&cli);

    if forwarded_args.is_empty() {
        eprintln!("Usage: cargo cooldown <cargo-command> [args...]");
        std::process::exit(2);
    }

    if matches!(
        cli.cargo_args.first().and_then(|value| value.to_str()),
        Some("update")
    ) {
        eprintln!(
            "cargo-cooldown is designed for commands like build, check, test, or run.\n\
             Running it with `cargo update` would replace the lockfile you just cooled down.\n\
             Invoke `cargo update` directly instead if you truly intend to refresh dependency versions."
        );
        std::process::exit(2);
    }

    if config.mode != Mode::Off && config.cooldown_minutes > 0 {
        match executor::run_pinning_flow(&config, &cli.manifest, &cli.features).await {
            Ok(_) => {}
            Err(err) => match config.mode {
                Mode::Warn => {
                    warn!(error = %err, "cooldown guard failed; continuing due to warn mode");
                }
                Mode::Enforce => {
                    return Err(err);
                }
                Mode::Off => {}
            },
        }
    }

    let status = Command::new("cargo").args(&forwarded_args).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::{assemble_cargo_args, parse_cli};
    use std::ffi::OsString;
    use std::path::PathBuf;

    fn to_os_vec(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn to_string_vec(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect()
    }

    #[test]
    fn assemble_drops_leading_cooldown_token() {
        let raw = to_os_vec(&["cargo-cooldown", "cooldown", "build", "--release"]);
        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli);
        assert_eq!(to_string_vec(&forwarded), vec!["build", "--release"]);
    }

    #[test]
    fn assemble_supports_direct_invocation() {
        let raw = to_os_vec(&["cargo-cooldown", "build", "--release"]);
        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli);
        assert_eq!(to_string_vec(&forwarded), vec!["build", "--release"]);
    }

    #[test]
    fn assemble_reinserts_manifest_before_command() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/demo/Cargo.toml",
            "build",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cli.manifest.manifest_path,
            Some(PathBuf::from("examples/demo/Cargo.toml"))
        );

        let forwarded = assemble_cargo_args(&cli);
        assert_eq!(
            to_string_vec(&forwarded),
            vec!["build", "--manifest-path", "examples/demo/Cargo.toml"]
        );
    }

    #[test]
    fn parse_detects_update_command() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/demo/Cargo.toml",
            "update",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cli.cargo_args.first().and_then(|arg| arg.to_str()).unwrap(),
            "update"
        );
    }

    #[test]
    fn assemble_preserves_trailing_arguments() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "test",
            "--features",
            "foo bar",
            "--",
            "--nocapture",
        ]);

        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli);
        assert_eq!(
            to_string_vec(&forwarded),
            vec!["test", "--features", "foo bar", "--", "--nocapture"]
        );
    }
}
