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

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("Usage: cargo-cooldown <cargo-command> [args...]");
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
