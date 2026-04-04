mod allowlist;
mod cache;
mod config;
mod executor;
mod metadata;
mod registry;
mod resolver;

use std::ffi::OsString;
use std::io::Write;
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
        about = "Cargo wrapper that enforces a cooldown window for freshly published registry crates.",
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
    match CargoCli::try_parse_from(normalize_cli_args(raw_args)) {
        Ok(CargoCli::Cooldown(cli)) => cli,
        Err(err) => err.exit(),
    }
}

fn normalize_cli_args(raw_args: &[OsString]) -> Vec<OsString> {
    let Some(binary) = raw_args.first() else {
        return Vec::new();
    };

    let user_args = if raw_args
        .get(1)
        .map(|arg| arg == "cooldown")
        .unwrap_or(false)
    {
        &raw_args[2..]
    } else {
        &raw_args[1..]
    };
    let (selectors, cargo_args) = hoist_cargo_selectors(user_args);

    let mut normalized = Vec::with_capacity(raw_args.len() + 1);
    normalized.push(binary.clone());
    normalized.push(OsString::from("cooldown"));
    normalized.extend(selectors);
    normalized.extend(cargo_args);
    normalized
}

fn hoist_cargo_selectors(args: &[OsString]) -> (Vec<OsString>, Vec<OsString>) {
    let mut selectors = Vec::new();
    let mut cargo_args = Vec::new();
    let mut command_seen = false;
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        let Some(arg_str) = arg.to_str() else {
            if !command_seen {
                command_seen = true;
            }
            cargo_args.push(arg.clone());
            index += 1;
            continue;
        };

        if arg_str == "--" {
            cargo_args.extend(args[index..].iter().cloned());
            break;
        }

        if is_top_level_help_flag(arg_str) && !command_seen {
            selectors.push(arg.clone());
            index += 1;
            continue;
        }

        if let Some(consumed) = selector_width(arg_str) {
            selectors.push(arg.clone());
            if consumed == 2 {
                if let Some(value) = args.get(index + 1) {
                    selectors.push(value.clone());
                    index += 2;
                } else {
                    index += 1;
                }
            } else {
                index += 1;
            }
            continue;
        }

        if !command_seen {
            command_seen = true;
        }
        cargo_args.push(arg.clone());
        index += 1;
    }

    (selectors, cargo_args)
}

fn is_top_level_help_flag(value: &str) -> bool {
    matches!(value, "-h" | "--help")
}

fn selector_width(value: &str) -> Option<usize> {
    match value {
        "--manifest-path" | "--package" | "-p" | "--exclude" | "--features" | "-F" => Some(2),
        "--workspace" | "--all" | "--all-features" | "--no-default-features" => Some(1),
        _ if value.starts_with("--manifest-path=")
            || value.starts_with("--package=")
            || value.starts_with("--exclude=")
            || value.starts_with("--features=")
            || (value.starts_with("-p") && value.len() > 2)
            || (value.starts_with("-F") && value.len() > 2) =>
        {
            Some(1)
        }
        _ => None,
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

fn exit_with(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = parse_cli(&raw_args);
    let config = Config::from_env();
    init_logging(config.verbose);

    let forwarded_args = assemble_cargo_args(&cli);

    if forwarded_args.is_empty() {
        eprintln!("Usage: cargo cooldown <cargo-command> [args...]");
        exit_with(2);
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
        exit_with(2);
    }

    if config.mode != Mode::Off && config.cooldown_minutes > 0 {
        match executor::run_pinning_flow(&config, &cli.manifest, &cli.workspace, &cli.features) {
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
    exit_with(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::{assemble_cargo_args, parse_cli, split_features};
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
            vec!["test", "--features", "foo,bar", "--", "--nocapture"]
        );
    }

    #[test]
    fn split_features_accepts_commas_and_spaces() {
        assert_eq!(
            split_features("foo,bar baz,,qux"),
            vec!["foo", "bar", "baz", "qux"]
        );
    }

    #[test]
    fn assemble_reapplies_workspace_and_feature_selectors() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/demo/Cargo.toml",
            "--package",
            "demo",
            "--workspace",
            "--exclude",
            "internal-only",
            "--all-features",
            "--no-default-features",
            "--features",
            "foo bar,baz",
            "check",
            "--quiet",
        ]);

        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli);
        assert_eq!(
            to_string_vec(&forwarded),
            vec![
                "check",
                "--manifest-path",
                "examples/demo/Cargo.toml",
                "--package",
                "demo",
                "--workspace",
                "--exclude",
                "internal-only",
                "--all-features",
                "--no-default-features",
                "--features",
                "foo,bar,baz",
                "--quiet",
            ]
        );
    }

    #[test]
    fn parse_supports_manifest_after_cargo_subcommand() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "check",
            "--manifest-path",
            "examples/demo/Cargo.toml",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cli.manifest.manifest_path,
            Some(PathBuf::from("examples/demo/Cargo.toml"))
        );
        assert_eq!(
            to_string_vec(&assemble_cargo_args(&cli)),
            vec!["check", "--manifest-path", "examples/demo/Cargo.toml",]
        );
    }

    #[test]
    fn parse_supports_workspace_selectors_after_cargo_subcommand() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "check",
            "--package",
            "demo",
            "--workspace",
            "--exclude",
            "internal-only",
            "--all-features",
            "--no-default-features",
            "--features",
            "foo bar,baz",
            "--quiet",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(cli.workspace.package, vec!["demo"]);
        assert!(cli.workspace.workspace);
        assert_eq!(cli.workspace.exclude, vec!["internal-only"]);
        assert!(cli.features.all_features);
        assert!(cli.features.no_default_features);
        assert_eq!(cli.features.features, vec!["foo", "bar,baz"]);
        assert_eq!(
            to_string_vec(&assemble_cargo_args(&cli)),
            vec![
                "check",
                "--package",
                "demo",
                "--workspace",
                "--exclude",
                "internal-only",
                "--all-features",
                "--no-default-features",
                "--features",
                "foo,bar,baz",
                "--quiet",
            ]
        );
    }
}
