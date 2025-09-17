mod allowlist;
mod cache;
mod config;
mod executor;
mod metadata;
mod registry;
mod resolver;

use std::process::Command;

use anyhow::Result;
use tracing::warn;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, Mode};

fn sanitize_args(mut args: Vec<String>) -> Vec<String> {
    if matches!(args.first(), Some(first) if first == "cooldown") {
        args.remove(0);
    }
    args
}

fn collect_cargo_args() -> Vec<String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    sanitize_args(raw)
}

fn init_logging(verbose: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            EnvFilter::new("cargo_cooldown=debug,cargo_cooldown::executor=debug,info")
        } else {
            EnvFilter::new("info")
        }
    });
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::from_env();
    init_logging(config.verbose);

    let args = collect_cargo_args();
    if args.is_empty() {
        eprintln!("Usage: cargo cooldown <cargo-command> [args...]");
        std::process::exit(2);
    }

    if matches!(args.first().map(String::as_str), Some("update")) {
        eprintln!(
            "cargo-cooldown is designed for commands like build, check, test, or run.\n\
             Running it with `cargo update` would replace the lockfile you just cooled down.\n\
             Invoke `cargo update` directly instead if you truly intend to refresh dependency versions."
        );
        std::process::exit(2);
    }

    if config.mode != Mode::Off && config.cooldown_minutes > 0 {
        match executor::run_pinning_flow(&config).await {
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

    let status = Command::new("cargo").args(&args).status()?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::sanitize_args;

    #[test]
    fn strips_leading_cooldown_subcommand() {
        let args = vec!["cooldown".to_string(), "update".to_string()];
        let sanitized = sanitize_args(args);
        assert_eq!(sanitized, vec!["update".to_string()]);
    }

    #[test]
    fn keeps_regular_arguments() {
        let args = vec!["build".to_string(), "--release".to_string()];
        let sanitized = sanitize_args(args.clone());
        assert_eq!(sanitized, args);
    }
}
