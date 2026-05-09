//! CLI entry point for the `cargo cooldown` wrapper.
//!
//! This file handles argument normalization, configuration discovery, the
//! `cargo cooldown init` command, and forwarding normal Cargo commands after the
//! cooldown guard has prepared or validated the lockfile.

/// Parses and merges allow rules from `cooldown.toml`.
mod allow_rules;
/// Provides a small JSON cache for registry API fallback responses.
mod cache;
/// Loads configuration from files and environment variables.
mod config;
/// Runs the cooldown lockfile rewrite and validation loop.
mod executor;
/// Implements the interactive `cargo cooldown init` setup wizard.
mod init;
/// Keeps speculative Cargo resolution away from the user-visible workspace.
mod isolation;
/// Captures, restores, and indexes `Cargo.lock` baselines.
mod lockfile;
/// Wraps `cargo metadata` invocations.
mod metadata;
/// Discovers the active Cargo project, workspace, and member config paths.
mod project;
/// Resolves registries and reads release metadata from indexes or APIs.
mod registry;
/// Builds the cooldown view of Cargo's resolved dependency graph.
mod resolution_state;
/// Selects compatible older releases from a registry timeline.
mod resolver;
/// Owns terminal progress, status formatting, and color behavior.
mod ui;

use std::ffi::OsString;
use std::io::Write;
use std::process::{Command, Output};
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use clap_cargo::{Features, Manifest, Workspace};
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Enforcement;
use crate::isolation::{CurrentDirGuard, IsolatedWorkspace};
use crate::project::ProjectContext;
use crate::ui::PhaseStatus;

#[derive(Debug, Parser)]
#[command(bin_name = "cargo")]
enum CargoCli {
    #[command(
        name = "cooldown",
        about = "Cargo wrapper that enforces a cooldown window for freshly published registry crates.",
        after_help = "Any other Cargo command is also forwarded through cooldown.",
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
    #[command(subcommand)]
    command: CooldownCommand,
    #[arg(skip)]
    forwarded_cargo_args: Vec<OsString>,
}

#[derive(Debug, Subcommand)]
enum CooldownCommand {
    #[command(
        about = "Initialize cooldown.toml in the current project root.",
        long_about = "Initialize cooldown.toml in the current project root.\n\nThis is cargo-cooldown's setup wizard, not Cargo's `cargo init`. Use plain `cargo init` to create a new package."
    )]
    Init,
    #[command(about = "Print cargo-cooldown's version.")]
    Version,
    #[command(
        about = "Cool the dependency graph, then run `cargo check`.",
        disable_help_flag = true
    )]
    Check {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(
        about = "Cool the dependency graph, then run `cargo build`.",
        disable_help_flag = true
    )]
    Build {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(
        about = "Cool the dependency graph, then run `cargo test`.",
        disable_help_flag = true
    )]
    Test {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(
        about = "Cool the dependency graph, then run `cargo run`.",
        disable_help_flag = true
    )]
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(
        about = "Run `cargo update`, then cool the updated dependency graph.",
        disable_help_flag = true
    )]
    Update {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    #[command(external_subcommand)]
    Cargo(Vec<OsString>),
}

impl CooldownCommand {
    fn cargo_args(&self, raw_cargo_args: &[OsString]) -> Option<Vec<OsString>> {
        match self {
            Self::Init | Self::Version => None,
            Self::Check { args } => Some(preserved_cargo_args("check", args, raw_cargo_args)),
            Self::Build { args } => Some(preserved_cargo_args("build", args, raw_cargo_args)),
            Self::Test { args } => Some(preserved_cargo_args("test", args, raw_cargo_args)),
            Self::Run { args } => Some(preserved_cargo_args("run", args, raw_cargo_args)),
            Self::Update { args } => Some(preserved_cargo_args("update", args, raw_cargo_args)),
            Self::Cargo(args) => {
                if raw_cargo_args.is_empty() {
                    Some(args.clone())
                } else {
                    Some(raw_cargo_args.to_vec())
                }
            }
        }
    }
}

fn preserved_cargo_args(
    command: &str,
    clap_args: &[OsString],
    raw_cargo_args: &[OsString],
) -> Vec<OsString> {
    if raw_cargo_args
        .first()
        .is_some_and(|arg| arg.to_str() == Some(command))
    {
        raw_cargo_args.to_vec()
    } else {
        named_cargo_args(command, clap_args)
    }
}

fn named_cargo_args(command: &str, args: &[OsString]) -> Vec<OsString> {
    let mut cargo_args = Vec::with_capacity(args.len() + 1);
    cargo_args.push(OsString::from(command));
    cargo_args.extend(args.iter().cloned());
    cargo_args
}

fn init_logging(verbose: bool) {
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if verbose {
        filter = filter.add_directive("cargo_cooldown=debug".parse().expect("valid directive"));
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .try_init();
}

/// Parse CLI arguments in both `cargo cooldown ...` and direct binary forms.
///
/// Cargo subcommands receive a leading `cooldown` token, while direct test or
/// development invocations may not. This normalizes those shapes, detects the
/// common `cargo cooldown init <path>` confusion early, and returns the parsed
/// command state used by the rest of the program.
fn parse_cli(raw_args: &[OsString]) -> Cli {
    let user_args = raw_user_args(raw_args);
    if is_top_level_version_request(user_args) {
        println!("cargo-cooldown {}", env!("CARGO_PKG_VERSION"));
        exit_with(0);
    }

    let (_, cargo_args) = hoist_cargo_selectors(user_args);
    if init_looks_like_forwarded_cargo_init(&cargo_args) {
        eprintln!(
            "`cargo cooldown init` is cargo-cooldown's configuration wizard, not Cargo's project generator.\n\
             Use plain `cargo init ...` to create a new package, then run `cargo cooldown init` from the project root."
        );
        exit_with(2);
    }

    match CargoCli::try_parse_from(normalize_cli_args(raw_args)) {
        Ok(CargoCli::Cooldown(mut cli)) => {
            cli.forwarded_cargo_args = cargo_args;
            cli
        }
        Err(err) => err.exit(),
    }
}

fn raw_user_args(raw_args: &[OsString]) -> &[OsString] {
    if raw_args.get(1).is_some_and(|arg| arg == "cooldown") {
        &raw_args[2..]
    } else {
        &raw_args[1..]
    }
}

/// Normalize user arguments into the shape expected by clap.
///
/// Cargo allows package, workspace, manifest, and feature selectors in flexible
/// positions. clap-cargo expects those selectors before the external Cargo
/// subcommand, so this function hoists them while preserving the forwarded Cargo
/// command and trailing arguments.
fn normalize_cli_args(raw_args: &[OsString]) -> Vec<OsString> {
    let Some(binary) = raw_args.first() else {
        return Vec::new();
    };

    let user_args = raw_user_args(raw_args);
    let (selectors, cargo_args) = hoist_cargo_selectors(user_args);

    let mut normalized = Vec::with_capacity(raw_args.len() + 1);
    normalized.push(binary.clone());
    normalized.push(OsString::from("cooldown"));
    normalized.extend(selectors);
    normalized.extend(cargo_args);
    normalized
}

/// Split top-level Cargo selectors from the forwarded Cargo command.
///
/// The input is the user-facing argument list after the optional `cooldown`
/// token. The return value is `(selectors, cargo_args)`: selectors are parsed by
/// this wrapper, and `cargo_args` are passed back to Cargo after cooldown runs.
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
            // clap-cargo expects selectors before the external subcommand, while
            // users often type them in Cargo's flexible order after the command.
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

fn is_top_level_version_request(args: &[OsString]) -> bool {
    matches!(
        args,
        [arg] if matches!(arg.to_str(), Some("-V" | "--version"))
    )
}

fn init_looks_like_forwarded_cargo_init(cargo_args: &[OsString]) -> bool {
    matches!(
        cargo_args.first().and_then(|value| value.to_str()),
        Some("init")
    ) && cargo_args
        .iter()
        .skip(1)
        .any(|arg| !matches!(arg.to_str(), Some("-h" | "--help")))
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

fn init_uses_runtime_selectors(cli: &Cli) -> bool {
    cli.manifest.manifest_path.is_some()
        || !cli.workspace.package.is_empty()
        || cli.workspace.workspace
        || cli.workspace.all
        || !cli.workspace.exclude.is_empty()
        || cli.features.all_features
        || cli.features.no_default_features
        || !cli.features.features.is_empty()
}

fn is_update_command(cargo_args: &[OsString]) -> bool {
    matches!(
        cargo_args.first().and_then(|value| value.to_str()),
        Some("update")
    )
}

fn is_cargo_help_request(cargo_args: &[OsString]) -> bool {
    if matches!(
        cargo_args.first().and_then(|value| value.to_str()),
        Some("help")
    ) {
        return true;
    }

    cargo_args
        .iter()
        .take_while(|arg| arg.to_str() != Some("--"))
        .any(|arg| matches!(arg.to_str(), Some("-h" | "--help")))
}

/// Canonicalize the Cargo invocation so the subcommand leads and the selectors
/// parsed by clap-cargo (`--manifest-path`, `--package`, feature flags, etc.)
/// are re-applied in the order that upstream `cargo` expects.
/// Rebuild the Cargo command that should run after cooldown processing.
///
/// The parsed wrapper state is converted back into normal Cargo flags, including
/// manifest path, workspace selectors, and feature selectors. The returned vector
/// is passed directly to `cargo`, so the original subcommand and its trailing
/// arguments stay under Cargo's control.
fn assemble_cargo_args(cli: &Cli, cargo_args: &[OsString]) -> Vec<OsString> {
    let mut args = Vec::new();
    let mut cargo_iter = cargo_args.iter();
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
        .map(ToString::to_string)
        .collect()
}

fn exit_with(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}

fn write_captured_output(output: &Output) {
    if !output.stdout.is_empty() {
        let _ = std::io::stdout().write_all(&output.stdout);
    }
    if !output.stderr.is_empty() {
        let _ = std::io::stderr().write_all(&output.stderr);
    }
}

/// Run the user's initial `cargo update` before applying cooldown.
///
/// `cargo cooldown update` first lets Cargo compute the newest valid graph for
/// the user's manifests and selectors. Its output is captured so the progress UI
/// stays clean; output is replayed only when Cargo itself fails. The returned
/// status decides whether cooldown should continue.
fn run_initial_cargo_update(
    forwarded_args: &[OsString],
    phase: &PhaseStatus,
) -> Result<std::process::ExitStatus> {
    debug!("refreshing lockfile via cargo update before applying cooldown");
    phase.set_message("Running cargo update...");
    let started = Instant::now();
    // Capture Cargo's output so the cooldown progress UI does not interleave with it.
    let output = Command::new("cargo").args(forwarded_args).output()?;
    debug!(
        target: "cargo_cooldown::timing",
        elapsed_ms = started.elapsed().as_millis(),
        "cooldown timing: initial cargo update"
    );
    phase.finish();
    if !output.status.success() {
        write_captured_output(&output);
    }
    Ok(output.status)
}

fn run_cooldown_guard_isolated(
    config: &config::Config,
    project: &ProjectContext,
    cli: &Cli,
    success_message: &str,
) -> Result<()> {
    let isolated = IsolatedWorkspace::create(project, &cli.manifest)?;
    {
        let _cwd = CurrentDirGuard::enter(isolated.current_dir())?;
        let initial_lockfile = executor::capture_initial_lockfile(config, isolated.manifest())?;
        executor::run_pinning_flow_with_snapshot(
            config,
            isolated.manifest(),
            &cli.workspace,
            &cli.features,
            initial_lockfile,
            success_message,
        )?;
    }
    isolated.publish_lockfile()
}

enum IsolatedUpdateOutcome {
    Done,
    CargoFailed(i32),
}

fn run_update_with_cooldown_isolation(
    config: &config::Config,
    project: &ProjectContext,
    cli: &Cli,
    forwarded_args: &[OsString],
    phase: &PhaseStatus,
) -> Result<IsolatedUpdateOutcome> {
    phase.set_message("Preparing isolated workspace...");
    let isolated = IsolatedWorkspace::create(project, &cli.manifest)?;
    let temp_forwarded_args = isolated.rewrite_cargo_args(forwarded_args);

    {
        let _cwd = CurrentDirGuard::enter(isolated.current_dir())?;
        phase.set_message("Capturing lockfile baseline...");
        let initial_lockfile = executor::capture_initial_lockfile(config, isolated.manifest())?;
        let status = run_initial_cargo_update(&temp_forwarded_args, phase)?;
        if !status.success() {
            return Ok(IsolatedUpdateOutcome::CargoFailed(
                status.code().unwrap_or(1),
            ));
        }
        let post_update_lockfile = executor::capture_initial_lockfile(config, isolated.manifest())?;

        if config.enforcement != Enforcement::Off && config.cooldown_minutes > 0 {
            match executor::run_pinning_flow_with_snapshot(
                config,
                isolated.manifest(),
                &cli.workspace,
                &cli.features,
                initial_lockfile,
                "dependency graph updated and cooled down",
            ) {
                Ok(()) => {}
                Err(err) => match config.enforcement {
                    Enforcement::CargoCompatible
                        if executor::is_cargo_compatible_fresh_versions_not_accepted(&err) =>
                    {
                        return Err(err);
                    }
                    Enforcement::CargoCompatible => {
                        warn!(error = %err, "cooldown guard failed after cargo update; continuing due to cargo_compatible enforcement");
                        executor::restore_lockfile_snapshot(
                            &post_update_lockfile,
                            isolated.manifest(),
                        )?;
                    }
                    Enforcement::Strict => {
                        return Err(err);
                    }
                    Enforcement::Off => {}
                },
            }
        }
    }

    isolated.publish_lockfile()?;
    Ok(IsolatedUpdateOutcome::Done)
}

/// Program entry point.
///
/// The command either runs the setup wizard or forwards a Cargo command through
/// cooldown. For `cargo cooldown update`, Cargo updates first, then cooldown
/// cools the resulting lockfile from the pre-update baseline. Other Cargo
/// commands run cooldown first when it is enabled, then execute Cargo with the
/// prepared lockfile.
fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let cli = parse_cli(&raw_args);

    match &cli.command {
        CooldownCommand::Init => {
            if init_uses_runtime_selectors(&cli) {
                eprintln!(
                    "`cargo cooldown init` only works from the current project root and does not accept Cargo selection flags."
                );
                exit_with(2);
            }

            let project = ProjectContext::discover_for_init()?;
            init::run(&project)?;
            Ok(())
        }
        CooldownCommand::Version => {
            println!("cargo-cooldown {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        command => {
            let cargo_args = command
                .cargo_args(&cli.forwarded_cargo_args)
                .expect("non-wrapper command should provide Cargo args");
            let forwarded_args = assemble_cargo_args(&cli, &cargo_args);
            if forwarded_args.is_empty() {
                eprintln!("Usage: cargo cooldown <cargo-command> [args...]");
                exit_with(2);
            }

            if is_cargo_help_request(&forwarded_args) {
                let status = Command::new("cargo").args(&forwarded_args).status()?;
                exit_with(status.code().unwrap_or(1));
            }

            let project = ProjectContext::discover_for_runtime(&cli.manifest, &cli.workspace)?;
            let config = config::Config::load(&project)?;
            init_logging(config.verbose);

            if is_update_command(&cargo_args) {
                let phase = PhaseStatus::new(config.verbose);
                match run_update_with_cooldown_isolation(
                    &config,
                    &project,
                    &cli,
                    &forwarded_args,
                    &phase,
                )? {
                    IsolatedUpdateOutcome::Done => exit_with(0),
                    IsolatedUpdateOutcome::CargoFailed(code) => exit_with(code),
                }
            }

            if config.enforcement != Enforcement::Off && config.cooldown_minutes > 0 {
                match run_cooldown_guard_isolated(
                    &config,
                    &project,
                    &cli,
                    "dependency graph cooled down; continuing with Cargo command",
                ) {
                    Ok(()) => {}
                    Err(err) => match config.enforcement {
                        Enforcement::CargoCompatible
                            if executor::is_cargo_compatible_fresh_versions_not_accepted(&err) =>
                        {
                            return Err(err);
                        }
                        Enforcement::CargoCompatible => {
                            warn!(error = %err, "cooldown guard failed; continuing due to cargo_compatible enforcement");
                        }
                        Enforcement::Strict => {
                            return Err(err);
                        }
                        Enforcement::Off => {}
                    },
                }
            }

            let status = Command::new("cargo").args(&forwarded_args).status()?;
            exit_with(status.code().unwrap_or(1));
        }
    }
}

/// Unit tests for CLI parsing and forwarded Cargo argument assembly.
#[cfg(test)]
mod tests {
    use super::{
        CooldownCommand, assemble_cargo_args, init_looks_like_forwarded_cargo_init,
        init_uses_runtime_selectors, is_cargo_help_request, is_update_command, parse_cli,
        split_features,
    };
    use clap::CommandFactory;
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

    fn cargo_args(cli: &super::Cli) -> Vec<OsString> {
        cli.command
            .cargo_args(&cli.forwarded_cargo_args)
            .unwrap_or_default()
    }

    #[test]
    fn help_lists_documented_commands() {
        let mut command = super::CargoCli::command();
        let help = command
            .find_subcommand_mut("cooldown")
            .expect("cooldown subcommand should exist")
            .render_help()
            .to_string();

        for expected in ["init", "version", "check", "build", "test", "run", "update"] {
            assert!(
                help.contains(&format!("  {expected}")),
                "help should list `{expected}`:\n{help}"
            );
        }
    }

    #[test]
    fn assemble_drops_leading_cooldown_token() {
        let raw = to_os_vec(&["cargo-cooldown", "cooldown", "build", "--release"]);
        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
        assert_eq!(to_string_vec(&forwarded), vec!["build", "--release"]);
    }

    #[test]
    fn assemble_supports_direct_invocation() {
        let raw = to_os_vec(&["cargo-cooldown", "build", "--release"]);
        let cli = parse_cli(&raw);
        let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
        assert_eq!(to_string_vec(&forwarded), vec!["build", "--release"]);
    }

    #[test]
    fn assemble_reinserts_manifest_before_command() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/crates-io-smoke-workspace/Cargo.toml",
            "build",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cli.manifest.manifest_path,
            Some(PathBuf::from(
                "examples/crates-io-smoke-workspace/Cargo.toml"
            ))
        );

        let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
        assert_eq!(
            to_string_vec(&forwarded),
            vec![
                "build",
                "--manifest-path",
                "examples/crates-io-smoke-workspace/Cargo.toml"
            ]
        );
    }

    #[test]
    fn parse_detects_update_command() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/crates-io-smoke-workspace/Cargo.toml",
            "update",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cargo_args(&cli)
                .first()
                .and_then(|arg| arg.to_str())
                .unwrap(),
            "update"
        );
        assert!(is_update_command(&cargo_args(&cli)));
    }

    #[test]
    fn parse_preserves_wrapped_command_help_for_cargo() {
        for (raw, expected) in [
            (
                &["cargo-cooldown", "cooldown", "check", "--help"][..],
                &["check", "--help"][..],
            ),
            (
                &["cargo-cooldown", "cooldown", "build", "-h"][..],
                &["build", "-h"][..],
            ),
            (
                &["cargo-cooldown", "cooldown", "update", "--help"][..],
                &["update", "--help"][..],
            ),
            (
                &["cargo-cooldown", "cooldown", "help", "check"][..],
                &["help", "check"][..],
            ),
        ] {
            let cli = parse_cli(&to_os_vec(raw));
            let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
            assert_eq!(to_string_vec(&forwarded), expected);
            assert!(is_cargo_help_request(&forwarded));
        }
    }

    #[test]
    fn cargo_help_detection_ignores_args_after_double_dash() {
        let args = to_os_vec(&["test", "--", "--help"]);
        assert!(!is_cargo_help_request(&args));
    }

    #[test]
    fn parse_detects_init_subcommand() {
        let raw = to_os_vec(&["cargo-cooldown", "cooldown", "init"]);
        let cli = parse_cli(&raw);
        assert!(matches!(cli.command, CooldownCommand::Init));
    }

    #[test]
    fn parse_detects_version_subcommand() {
        let raw = to_os_vec(&["cargo-cooldown", "cooldown", "version"]);
        let cli = parse_cli(&raw);
        assert!(matches!(cli.command, CooldownCommand::Version));
        assert!(cargo_args(&cli).is_empty());
    }

    #[test]
    fn detects_forwarded_cargo_init_arguments_as_collision() {
        let args = to_os_vec(&["init", "--bin"]);
        assert!(init_looks_like_forwarded_cargo_init(&args));
    }

    #[test]
    fn allows_init_help_without_triggering_collision() {
        let args = to_os_vec(&["init", "--help"]);
        assert!(!init_looks_like_forwarded_cargo_init(&args));
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
        let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
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
            "examples/crates-io-smoke-workspace/Cargo.toml",
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
        let forwarded = assemble_cargo_args(&cli, &cargo_args(&cli));
        assert_eq!(
            to_string_vec(&forwarded),
            vec![
                "check",
                "--manifest-path",
                "examples/crates-io-smoke-workspace/Cargo.toml",
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
            "examples/crates-io-smoke-workspace/Cargo.toml",
        ]);

        let cli = parse_cli(&raw);
        assert_eq!(
            cli.manifest.manifest_path,
            Some(PathBuf::from(
                "examples/crates-io-smoke-workspace/Cargo.toml"
            ))
        );
        assert_eq!(
            to_string_vec(&assemble_cargo_args(&cli, &cargo_args(&cli))),
            vec![
                "check",
                "--manifest-path",
                "examples/crates-io-smoke-workspace/Cargo.toml",
            ]
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
            to_string_vec(&assemble_cargo_args(&cli, &cargo_args(&cli))),
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

    #[test]
    fn init_rejects_runtime_selectors() {
        let raw = to_os_vec(&[
            "cargo-cooldown",
            "cooldown",
            "--manifest-path",
            "examples/crates-io-smoke-workspace/Cargo.toml",
            "init",
        ]);

        let cli = parse_cli(&raw);

        assert!(init_uses_runtime_selectors(&cli));
    }
}
