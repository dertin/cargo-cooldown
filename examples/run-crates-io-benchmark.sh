#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_DIR=$(cd "${ROOT_DIR}/.." && pwd)
WORKSPACE_DIR=${WORKSPACE_DIR:-"${ROOT_DIR}/crates-io-smoke-workspace"}
SAMPLES=${SAMPLES:-3}
MIN_PUBLISH_AGE_VALUE=${CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE:-"3 months"}
COOLDOWN_NOW_VALUE=${COOLDOWN_NOW:-$(date -u +"%Y-%m-%dT%H:%M:%SZ")}
INCOMPATIBLE_PUBLISH_AGE_VALUE=${COOLDOWN_INCOMPATIBLE_PUBLISH_AGE:-fallback}
COOLDOWN_FALLBACK_ACCEPT_VALUE=${COOLDOWN_FALLBACK_ACCEPT:-auto}
LOCKFILE_BASELINE_VALUE=${COOLDOWN_LOCKFILE_BASELINE:-ignore}
BENCH_VERBOSE=${BENCH_VERBOSE:-0}
BENCH_OFFLINE=${BENCH_OFFLINE:-0}
BENCH_PREFETCH_COOLDOWN=${BENCH_PREFETCH_COOLDOWN:-0}
BENCH_ISOLATED_CARGO_HOME=${BENCH_ISOLATED_CARGO_HOME:-0}
BENCH_ARTIFACT_ROOT=${BENCH_ARTIFACT_ROOT:-"${REPO_DIR}/target/cargo-cooldown-benchmarks"}
BENCH_RUN_ID=${BENCH_RUN_ID:-$(date -u +"%Y%m%dT%H%M%SZ")}
COOLDOWN_VERBOSE_VALUE=${COOLDOWN_VERBOSE:-1}

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

if [[ -n ${CMD:-} ]]; then
  CMD_BIN="${CMD}"
  echo "Using override command: ${CMD_BIN}" >&2
else
  CMD_BIN=$(resolve_local_binary)
  echo "Using local cargo-cooldown binary built from current sources: ${CMD_BIN}" >&2
fi

if [[ ! -d "${WORKSPACE_DIR}" ]]; then
  echo "Expected benchmark workspace at ${WORKSPACE_DIR}" >&2
  exit 1
fi

TMP_ROOT=$(mktemp -d)
trap 'rm -rf "${TMP_ROOT}"' EXIT
ARTIFACT_DIR="${BENCH_ARTIFACT_ROOT}/${BENCH_RUN_ID}"
ARTIFACT_INDEX="${TMP_ROOT}/artifacts.txt"
SAMPLE_INDEX="${TMP_ROOT}/samples.tsv"
DURATIONS_FILE="${TMP_ROOT}/durations.txt"
mkdir -p "${ARTIFACT_DIR}"
: >"${ARTIFACT_INDEX}"
: >"${SAMPLE_INDEX}"
: >"${DURATIONS_FILE}"

case "${BENCH_ISOLATED_CARGO_HOME}" in
  1|true|TRUE|yes|YES|on|ON)
    BENCH_CARGO_HOME_DIR="${TMP_ROOT}/cargo-home"
    mkdir -p "${BENCH_CARGO_HOME_DIR}"
    ;;
  0|false|FALSE|no|NO|off|OFF)
    BENCH_CARGO_HOME_DIR="${CARGO_HOME:-}"
    ;;
  *)
    echo "Expected boolean value, got: ${BENCH_ISOLATED_CARGO_HOME}" >&2
    exit 1
    ;;
esac

prepare_workspace() {
  local destination=$1
  cp -R "${WORKSPACE_DIR}" "${destination}"
  rm -f "${destination}/Cargo.lock"
}

now_ns() {
  python3 - <<'PY'
import time
print(time.perf_counter_ns())
PY
}

configure_cargo_home() {
  if [[ -n "${BENCH_CARGO_HOME_DIR}" ]]; then
    export CARGO_HOME="${BENCH_CARGO_HOME_DIR}"
  fi
}

bench_bool_enabled() {
  case "$1" in
    1|true|TRUE|yes|YES|on|ON)
      return 0
      ;;
    0|false|FALSE|no|NO|off|OFF)
      return 1
      ;;
    *)
      echo "Expected boolean value, got: $1" >&2
      exit 1
      ;;
  esac
}

show_log_tail() {
  local log_file=$1
  if [[ -f "${log_file}" ]]; then
    echo "Last log lines from ${log_file}:" >&2
    tail -n 80 "${log_file}" >&2 || true
  elif [[ "${BENCH_VERBOSE}" != "1" ]]; then
    echo "No log file was captured at ${log_file}." >&2
  fi
}

assert_lockfile_exists() {
  local workspace=$1
  local log_file=$2
  local label=$3

  if [[ ! -f "${workspace}/Cargo.lock" ]]; then
    echo "${label} did not produce Cargo.lock." >&2
    show_log_tail "${log_file}"
    return 1
  fi
}

record_run_artifacts() {
  local sample=$1
  local workspace=$2
  local log_file=$3
  local destination

  destination="${ARTIFACT_DIR}/cooldown-${sample}"
  mkdir -p "${destination}"
  cp "${workspace}/Cargo.lock" "${destination}/Cargo.lock"
  if [[ -f "${log_file}" ]]; then
    cp "${log_file}" "${destination}/cooldown.log"
  fi
  printf '  sample %-4s %s\n' "${sample}" "${destination}/Cargo.lock" >>"${ARTIFACT_INDEX}"
}

warm_crates_io_snapshot() {
  local workspace="${TMP_ROOT}/warmup"
  local log_file="${TMP_ROOT}/warmup.log"
  local start_ns
  local end_ns
  prepare_workspace "${workspace}"

  echo "Warming crates.io cache with plain cargo update and cargo fetch..." >&2
  start_ns=$(now_ns)
  if [[ "${BENCH_VERBOSE}" == "1" ]]; then
    if ! (
      cd "${workspace}" >&2
      configure_cargo_home
      export CARGO_TERM_PROGRESS_WHEN=never
      cargo update
      cargo fetch --locked
    ); then
      echo "crates.io warm-up failed." >&2
      return 1
    fi
  else
    if ! (
      cd "${workspace}" >&2
      configure_cargo_home
      export CARGO_TERM_PROGRESS_WHEN=never
      cargo update
      cargo fetch --locked
    ) >"${log_file}" 2>&1; then
      echo "crates.io warm-up failed." >&2
      show_log_tail "${log_file}"
      return 1
    fi
  fi

  assert_lockfile_exists "${workspace}" "${log_file}" "crates.io warm-up"
  end_ns=$(now_ns)
  echo "Warm-up complete: $(format_seconds "$((end_ns - start_ns))")" >&2
}

run_cooldown_update() {
  local workspace=$1
  local log_file=$2
  local label=$3
  local offline=$4

  if [[ "${BENCH_VERBOSE}" == "1" ]]; then
    if ! (
      cd "${workspace}" >&2
      configure_cargo_home
      if [[ "${offline}" == "true" ]]; then
        export CARGO_NET_OFFLINE=true
      fi
      export CARGO_TERM_PROGRESS_WHEN=never
      export COOLDOWN_NOW="${COOLDOWN_NOW_VALUE}"
      export CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE="${MIN_PUBLISH_AGE_VALUE}"
      export COOLDOWN_INCOMPATIBLE_PUBLISH_AGE="${INCOMPATIBLE_PUBLISH_AGE_VALUE}"
      export COOLDOWN_FALLBACK_ACCEPT="${COOLDOWN_FALLBACK_ACCEPT_VALUE}"
      export COOLDOWN_LOCKFILE_BASELINE="${LOCKFILE_BASELINE_VALUE}"
      export COOLDOWN_VERBOSE="${COOLDOWN_VERBOSE_VALUE}"
      "${CMD_BIN}" update
    ); then
      echo "${label} failed." >&2
      return 1
    fi
  else
    if ! (
      cd "${workspace}" >&2
      configure_cargo_home
      if [[ "${offline}" == "true" ]]; then
        export CARGO_NET_OFFLINE=true
      fi
      export CARGO_TERM_PROGRESS_WHEN=never
      export COOLDOWN_NOW="${COOLDOWN_NOW_VALUE}"
      export CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE="${MIN_PUBLISH_AGE_VALUE}"
      export COOLDOWN_INCOMPATIBLE_PUBLISH_AGE="${INCOMPATIBLE_PUBLISH_AGE_VALUE}"
      export COOLDOWN_FALLBACK_ACCEPT="${COOLDOWN_FALLBACK_ACCEPT_VALUE}"
      export COOLDOWN_LOCKFILE_BASELINE="${LOCKFILE_BASELINE_VALUE}"
      export COOLDOWN_VERBOSE="${COOLDOWN_VERBOSE_VALUE}"
      "${CMD_BIN}" update
    ) >"${log_file}" 2>&1; then
      echo "${label} failed." >&2
      show_log_tail "${log_file}"
      return 1
    fi
  fi

  assert_lockfile_exists "${workspace}" "${log_file}" "${label}"
}

prefetch_cooldown_sources() {
  local workspace
  local log_file
  local start_ns
  local end_ns

  if ! bench_bool_enabled "${BENCH_PREFETCH_COOLDOWN}"; then
    echo "Skipping cooldown candidate preload." >&2
    return 0
  fi

  echo "Preloading cooldown candidate crates..." >&2
  workspace="${TMP_ROOT}/preload-cooldown"
  log_file="${TMP_ROOT}/preload-cooldown.log"

  prepare_workspace "${workspace}"
  echo "  preload: running..." >&2
  start_ns=$(now_ns)
  run_cooldown_update \
    "${workspace}" \
    "${log_file}" \
    "cooldown preload" \
    false
  end_ns=$(now_ns)
  echo "  preload: $(format_seconds "$((end_ns - start_ns))")" >&2
}

measure_run() {
  local sample=$1
  local workspace
  local log_file
  local start_ns
  local end_ns
  local duration_ns
  local fallback_inspections
  local fallback_unique
  local fallback

  workspace="${TMP_ROOT}/cooldown-${sample}"
  log_file="${TMP_ROOT}/cooldown-${sample}.log"

  prepare_workspace "${workspace}"
  echo "  cooldown: running..." >&2
  start_ns=$(now_ns)
  if ! run_cooldown_update \
    "${workspace}" \
    "${log_file}" \
    "cooldown sample ${sample}" \
    "$(bench_bool_enabled "${BENCH_OFFLINE}" && printf 'true' || printf 'false')"; then
    return 1
  fi
  record_run_artifacts "${sample}" "${workspace}" "${log_file}"
  end_ns=$(now_ns)
  duration_ns=$((end_ns - start_ns))
  fallback_inspections=$(fallback_inspection_count "${log_file}")
  fallback_unique=$(fallback_unique_count "${log_file}")
  fallback=$(fallback_detail "${fallback_inspections}" "${fallback_unique}")
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "${sample}" \
    "${duration_ns}" \
    "${fallback}" \
    "${fallback_inspections}" \
    "${fallback_unique}" >>"${SAMPLE_INDEX}"
  printf '%s\n' "${duration_ns}"
}

format_seconds() {
  python3 - "$1" <<'PY'
import sys
value = int(sys.argv[1]) / 1_000_000_000
print(f"{value:.3f}s")
PY
}

average_ns() {
  python3 - "$@" <<'PY'
import sys
values = [int(value) for value in sys.argv[1:]]
print(sum(values) // len(values))
PY
}

fallback_inspection_count() {
  local log_file=$1

  if ! fallback_report_enabled || [[ ! -f "${log_file}" ]]; then
    printf '0\n'
    return
  fi

  awk '
    /cooldown: / && /release_time_source=registry_api_fallback/ { count++ }
    END { print count + 0 }
  ' "${log_file}"
}

fallback_unique_count() {
  local log_file=$1

  if ! fallback_report_enabled || [[ ! -f "${log_file}" ]]; then
    printf '0\n'
    return
  fi

  awk '
    /cooldown: / && /release_time_source=registry_api_fallback/ {
      crate = ""
      version = ""
      for (i = 1; i <= NF; i++) {
        if ($i ~ /^crate=/) {
          crate = substr($i, 7)
        }
        if ($i ~ /^version=/) {
          version = substr($i, 9)
        }
      }
      if (crate != "" && version != "") {
        seen[crate "@" version] = 1
      }
    }
    END {
      for (key in seen) {
        count++
      }
      print count + 0
    }
  ' "${log_file}"
}

fallback_report_enabled() {
  bench_bool_enabled "${COOLDOWN_VERBOSE_VALUE}" && [[ "${BENCH_VERBOSE}" != "1" ]]
}

fallback_detail() {
  local inspections=$1
  local unique=$2

  if ! fallback_report_enabled; then
    printf 'not captured'
  elif [[ "${inspections}" == "0" ]]; then
    printf 'no'
  else
    printf 'yes (%s inspections, %s crate versions)' "${inspections}" "${unique}"
  fi
}

sample_fallback_detail() {
  local sample=$1

  awk -F '\t' -v sample="${sample}" '$1 == sample { value = $3 } END { print value }' "${SAMPLE_INDEX}"
}

bench_bool_enabled "${BENCH_OFFLINE}" || true
bench_bool_enabled "${BENCH_PREFETCH_COOLDOWN}" || true
bench_bool_enabled "${COOLDOWN_VERBOSE_VALUE}" || true

warm_crates_io_snapshot
prefetch_cooldown_sources

for sample in $(seq 1 "${SAMPLES}"); do
  echo >&2
  echo "Sample ${sample}/${SAMPLES}" >&2

  if ! duration=$(measure_run "${sample}"); then
    exit 1
  fi
  printf '%s\n' "${duration}" >>"${DURATIONS_FILE}"
  echo "  cooldown: $(format_seconds "${duration}")"
  echo "  fallback: $(sample_fallback_detail "${sample}")"
done

mapfile -t durations <"${DURATIONS_FILE}"
avg_duration=$(average_ns "${durations[@]}")
fallback_samples=$(awk -F '\t' '$3 ~ /^yes/ { count++ } END { print count + 0 }' "${SAMPLE_INDEX}")
fallback_inspections=$(awk -F '\t' '{ count += $4 } END { print count + 0 }' "${SAMPLE_INDEX}")
fallback_unique_sum=$(awk -F '\t' '{ count += $5 } END { print count + 0 }' "${SAMPLE_INDEX}")

cat <<EOF

Benchmark complete
  workspace: ${WORKSPACE_DIR}
  crates.io snapshot time: ${COOLDOWN_NOW_VALUE}
  samples: ${SAMPLES}
  min_publish_age: ${MIN_PUBLISH_AGE_VALUE}
  incompatible_publish_age: ${INCOMPATIBLE_PUBLISH_AGE_VALUE}
  fallback-accept: ${COOLDOWN_FALLBACK_ACCEPT_VALUE}
  lockfile-baseline: ${LOCKFILE_BASELINE_VALUE}
  offline samples: ${BENCH_OFFLINE}
  preload cooldown candidates: ${BENCH_PREFETCH_COOLDOWN}
  isolated cargo home: ${BENCH_ISOLATED_CARGO_HOME}
  cooldown verbose: ${COOLDOWN_VERBOSE_VALUE}
  artifacts: ${ARTIFACT_DIR}

EOF

printf '  %-12s %12s\n' "average" "$(format_seconds "${avg_duration}")"

cat <<EOF

Fallback report
  samples with HTTP fallback: ${fallback_samples}/${SAMPLES}
  HTTP fallback inspections: ${fallback_inspections}
  HTTP fallback crate versions: ${fallback_unique_sum}
EOF

if ! fallback_report_enabled; then
  cat <<EOF
  note: fallback report requires COOLDOWN_VERBOSE=1 and BENCH_VERBOSE=0
EOF
fi

cat <<EOF

Cargo.lock artifacts
EOF
cat "${ARTIFACT_INDEX}"
