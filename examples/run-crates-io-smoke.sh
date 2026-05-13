#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "${ROOT_DIR}/.." && pwd)
WORKSPACE_DIR="${ROOT_DIR}/crates-io-smoke-workspace"
CONFIG_FILE="${WORKSPACE_DIR}/cooldown.toml"
CARGO_SUBCOMMAND=${CARGO_SUBCOMMAND:-build}

resolve_local_binary() {
  local target_dir
  target_dir=$(
    cargo metadata \
      --quiet \
      --format-version 1 \
      --no-deps \
      --manifest-path "${REPO_DIR}/Cargo.toml" \
      | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
  )

  cargo build --quiet --manifest-path "${REPO_DIR}/Cargo.toml"
  printf '%s/debug/cargo-cooldown' "${target_dir}"
}

CMD_ARGS=()

run_case() {
  local name=$1
  local description=$2
  local reset_lock=$3
  shift 3

  echo
  echo "=== ${name} ==="
  echo "Description: ${description}"
  (
    cd "${WORKSPACE_DIR}" >&2
    if [[ ${reset_lock} == yes ]]; then
      rm -f Cargo.lock
    fi
    env "$@" "${CMD_BIN}" "${CMD_ARGS[@]}" "${CARGO_SUBCOMMAND}"
  )
}

run_case_with_appended_config() {
  local name=$1
  local description=$2
  local reset_lock=$3
  local config_block=$4
  shift 4

  (
    local backup_config
    backup_config=$(mktemp)
    cp "${CONFIG_FILE}" "${backup_config}"
    trap 'cp "${backup_config}" "${CONFIG_FILE}"; rm -f "${backup_config}"' EXIT

    printf '\n%s\n' "${config_block}" >> "${CONFIG_FILE}"
    run_case "${name}" "${description}" "${reset_lock}" "$@"
  )
}

usage() {
  cat <<'USAGE'
Usage: ./examples/run-crates-io-smoke.sh [CASE ...]

Run smoke-test scenarios for cargo-cooldown against the current crates.io
state. This is intentionally non-deterministic and meant only as a quick
manual check of the public workflow.

Available CASE values:
  allow-rules      Append package-scoped allow rules to the smoke workspace
                   config for one run, showing mixed per-crate cooldown
                   overrides.
  fallback         Keep cooldown enabled, but continue the Cargo command if
                   Cargo requires fresh versions.
  skip-crates-io   Skip crates.io entirely through COOLDOWN_SKIP_REGISTRIES
                   and verify that cooldown does not inspect registry packages.
  aggressive-ttl   Use a shorter cooldown window, short cache TTL, retries,
                   and verbose logging to inspect the resolver behavior.
USAGE
}

if [[ ${1:-} == "-h" || ${1:-} == "--help" ]]; then
  usage
  exit 0
fi

if [[ ${CARGO_SUBCOMMAND} == "update" ]]; then
  echo "This smoke script focuses on build/check-style scenarios. For lockfile refreshes under cooldown, run 'cargo cooldown update' directly in the smoke workspace instead." >&2
  exit 1
fi

if [[ ! -d "${WORKSPACE_DIR}" ]]; then
  echo "Expected workspace at ${WORKSPACE_DIR}" >&2
  exit 1
fi

if [[ ! -f "${CONFIG_FILE}" ]]; then
  echo "Expected smoke workspace configuration at ${CONFIG_FILE}" >&2
  exit 1
fi

if [[ -n ${CMD:-} ]]; then
  CMD_BIN="${CMD}"
  echo "Using override command: ${CMD_BIN}" >&2
else
  CMD_BIN=$(resolve_local_binary)
  echo "Using local cargo-cooldown binary built from current sources: ${CMD_BIN}" >&2
fi

SELECTED=("allow-rules" "fallback" "skip-crates-io" "aggressive-ttl")
if [[ $# -gt 0 ]]; then
  SELECTED=("$@")
fi

for case_name in "${SELECTED[@]}"; do
  case "${case_name}" in
    allow-rules)
      run_case_with_appended_config \
        "allow-rules" \
        "Temporarily appends allow.package rules so one dependency gets a shorter cooldown and another is excluded entirely." \
        yes \
        $'[[allow.package]]\ncrate = "chrono"\nminutes = 60\n\n[[allow.package]]\ncrate = "serde_json"\nminutes = 0' \
        CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE="3 months"
      ;;
    fallback)
      run_case \
        "fallback" \
        "Cooldown is active, but fresh versions required by Cargo are reported as warnings and the Cargo command still runs." \
        yes \
        COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback \
        COOLDOWN_FALLBACK_ACCEPT=auto
      ;;
    skip-crates-io)
      run_case \
        "skip-crates-io" \
        "crates.io is explicitly skipped. The command should bypass cooldown inspection for registry packages from crates.io." \
        yes \
        COOLDOWN_SKIP_REGISTRIES=crates-io
      ;;
    aggressive-ttl)
      run_case \
        "aggressive-ttl" \
        "Shorter cooldown window with verbose logs, shorter TTL, and retries. Useful to inspect cache behavior and timestamp source decisions." \
        yes \
        CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE="3 hours" \
        COOLDOWN_TTL_SECONDS=300 \
        COOLDOWN_HTTP_RETRIES=4 \
        COOLDOWN_VERBOSE=1
      ;;
    *)
      echo "Unknown case: ${case_name}" >&2
      usage
      exit 1
      ;;
  esac
done
