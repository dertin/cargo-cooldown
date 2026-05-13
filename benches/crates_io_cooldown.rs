//! Cargo bench harness that delegates to the real crates.io benchmark scripts.

use std::env;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let scenario = match parse_scenario(env::args().skip(1).collect()) {
        Ok(Some(Scenario::Help)) => {
            print_help();
            return ExitCode::SUCCESS;
        }
        Ok(Some(Scenario::Large60d)) => Scenario::Large60d,
        Ok(Some(Scenario::Default)) | Ok(None) => Scenario::Default,
        Err(message) => {
            eprintln!("{message}");
            print_help();
            return ExitCode::FAILURE;
        }
    };

    let repo_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = match scenario {
        Scenario::Default => repo_dir.join("examples/run-crates-io-benchmark.sh"),
        Scenario::Large60d => repo_dir.join("examples/run-crates-io-large-60d-benchmark.sh"),
        Scenario::Help => unreachable!("help exits before running a benchmark"),
    };

    let mut command = Command::new(&script);
    command.current_dir(&repo_dir);
    if let Some(binary) = option_env!("CARGO_BIN_EXE_cargo-cooldown") {
        command.env("CMD", binary);
    }

    eprintln!(
        "Running crates.io cooldown benchmark via {}",
        script.display()
    );
    let status = match command.status() {
        Ok(status) => status,
        Err(err) => {
            eprintln!("failed to run {}: {err}", script.display());
            return ExitCode::FAILURE;
        }
    };

    ExitCode::from(status.code().unwrap_or(1) as u8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Default,
    Large60d,
    Help,
}

fn parse_scenario(args: Vec<String>) -> Result<Option<Scenario>, String> {
    let mut scenario = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => return Ok(Some(Scenario::Help)),
            "--bench" => {
                index += 1;
            }
            "--scenario" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "--scenario requires a value".to_string())?;
                scenario = Some(parse_scenario_value(value)?);
                index += 2;
            }
            value if value.starts_with("--scenario=") => {
                let value = value.trim_start_matches("--scenario=");
                scenario = Some(parse_scenario_value(value)?);
                index += 1;
            }
            "--large-60d" => {
                scenario = Some(Scenario::Large60d);
                index += 1;
            }
            other => {
                return Err(format!("unknown benchmark argument: {other}"));
            }
        }
    }

    Ok(scenario)
}

fn parse_scenario_value(value: &str) -> Result<Scenario, String> {
    match value {
        "default" | "smoke" | "small" => Ok(Scenario::Default),
        "large-60d" | "aggressive-60d" => Ok(Scenario::Large60d),
        _ => Err(format!("unknown benchmark scenario: {value}")),
    }
}

fn print_help() {
    eprintln!(
        r#"Usage: cargo bench --bench crates_io_cooldown -- [OPTIONS]

Options:
  --scenario <default|large-60d>  Select the crates.io workload
  --large-60d                    Shortcut for --scenario large-60d
  -h, --help                     Show this help

Environment is forwarded to the benchmark script. Useful variables include:
  SAMPLES, CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE,
  COOLDOWN_INCOMPATIBLE_PUBLISH_AGE, COOLDOWN_FALLBACK_ACCEPT,
  BENCH_OFFLINE, BENCH_PREFETCH_COOLDOWN, BENCH_ISOLATED_CARGO_HOME,
  BENCH_ARTIFACT_ROOT, BENCH_RUN_ID, RUST_LOG, COOLDOWN_VERBOSE

The benchmark defaults to fallback policy because current crates.io
graphs may contain fresh resolver-constrained groups. Set
COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=deny to benchmark fail-closed behavior.
COOLDOWN_VERBOSE defaults to 1 so the runner can report registry API fallback
usage from captured logs.
"#
    );
}
