#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
  cat <<'USAGE'
Compare Cargo.lock from cargo-cooldown with nightly -Zmin-publish-age.

Usage:
  compare-lockfile.sh [OPTIONS]

Options:
  --repo PATH                    Rust workspace/package path. Default: current dir.
  --age DURATION                 Minimum publish age. Default: "14 days".
  --toolchain TOOLCHAIN          Rust nightly toolchain. Default: nightly-2026-06-21.
                                 cargo-cooldown uses this toolchain's Cargo binary
                                 (without -Zmin-publish-age) so both sides share
                                 the same resolver version.
  --out-dir PATH                 Output directory. Default: temporary directory.
  --order VALUE                  Run order: cooldown-first or nightly-first. Default: cooldown-first.
  --fresh-lock                   Remove Cargo.lock in both temp copies before resolving.
  --cooldown-baseline VALUE      cargo-cooldown lockfile-baseline: floor or ignore. Default: floor.
  --cooldown-fallback            Use cargo-cooldown fallback policy instead of deny.
  --install-cargo-cooldown       Install cargo-cooldown with cargo install --locked if missing.
  --keep-workdirs                Keep copied work directories after the run.
  -h, --help                     Show this help.

Examples:
  ./compare-lockfile.sh --repo /opt/procesador --age "14 days"
  ./compare-lockfile.sh --repo . --order nightly-first
  ./compare-lockfile.sh --repo . --age "60 days" --fresh-lock
  ./compare-lockfile.sh --repo . --cooldown-baseline ignore --cooldown-fallback

Outputs:
  <out-dir>/cargo-cooldown/Cargo.lock
  <out-dir>/nightly-min-publish-age/Cargo.lock
  <out-dir>/Cargo.lock.diff
  <out-dir>/package-version-diff.tsv
  <out-dir>/timing.tsv
  <out-dir>/logs/*.log
USAGE
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

log() {
  printf '[%s] %s\n' "$(date +'%H:%M:%S')" "$*"
}

quote_toml_string() {
  local value=$1
  value=${value//\\/\\\\}
  value=${value//\"/\\\"}
  printf '"%s"' "$value"
}

repo=$PWD
age='14 days'
toolchain='nightly-2026-06-21'
out_dir=''
fresh_lock=0
keep_workdirs=0
install_cargo_cooldown=0
cooldown_baseline='floor'
cooldown_incompatible='deny'
order='cooldown-first'

while (($#)); do
  case "$1" in
    --repo)
      repo=${2:-}
      shift 2
      ;;
    --age)
      age=${2:-}
      shift 2
      ;;
    --toolchain)
      toolchain=${2:-}
      shift 2
      ;;
    --out-dir)
      out_dir=${2:-}
      shift 2
      ;;
    --order)
      order=${2:-}
      shift 2
      ;;
    --fresh-lock)
      fresh_lock=1
      shift
      ;;
    --keep-workdirs)
      keep_workdirs=1
      shift
      ;;
    --install-cargo-cooldown)
      install_cargo_cooldown=1
      shift
      ;;
    --cooldown-baseline)
      cooldown_baseline=${2:-}
      shift 2
      ;;
    --cooldown-fallback)
      cooldown_incompatible='fallback'
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown option: $1"
      ;;
  esac
done

repo=$(realpath "$repo")
[[ -f "$repo/Cargo.toml" ]] || die "Cargo.toml not found in $repo"
[[ "$cooldown_baseline" == "floor" || "$cooldown_baseline" == "ignore" ]] || \
  die "--cooldown-baseline must be floor or ignore"
[[ "$order" == "cooldown-first" || "$order" == "nightly-first" ]] || \
  die "--order must be cooldown-first or nightly-first"

command -v cargo >/dev/null || die "cargo is required"
command -v rustup >/dev/null || die "rustup is required"
command -v python3 >/dev/null || die "python3 is required"
command -v diff >/dev/null || die "diff is required"

if ! rustup toolchain list | awk '{print $1}' | grep -Eq "^${toolchain}(-|$)"; then
  log "Installing Rust toolchain ${toolchain}"
  rustup toolchain install "$toolchain"
fi

toolchain_cargo_dir=$(dirname "$(rustup which --toolchain "$toolchain" cargo)")
[[ -x "$toolchain_cargo_dir/cargo" ]] || die "cargo not found for toolchain ${toolchain}"

if ! command -v cargo-cooldown >/dev/null; then
  if ((install_cargo_cooldown)); then
    log "Installing cargo-cooldown with cargo install --locked"
    cargo install --locked cargo-cooldown
  else
    die "cargo-cooldown not found in PATH. Re-run with --install-cargo-cooldown or install it manually."
  fi
fi

if [[ -z "$out_dir" ]]; then
  out_dir=$(mktemp -d "${TMPDIR:-/tmp}/compare-lockfile.XXXXXX")
else
  mkdir -p "$out_dir"
  out_dir=$(realpath "$out_dir")
fi

work_root="$out_dir/work"
cooldown_work="$work_root/cargo-cooldown"
nightly_work="$work_root/nightly-min-publish-age"
logs_dir="$out_dir/logs"
mkdir -p "$work_root" "$logs_dir" "$out_dir/cargo-cooldown" "$out_dir/nightly-min-publish-age"
printf 'name\texit_code\telapsed_ms\telapsed_seconds\n' >"$out_dir/timing.tsv"

copy_repo() {
  local src=$1
  local dst=$2
  mkdir -p "$dst"
  if command -v rsync >/dev/null; then
    rsync -a \
      --exclude '/target' \
      --exclude '/.git' \
      --exclude '/.direnv' \
      "$src"/ "$dst"/
  else
    cp -a "$src"/. "$dst"/
    rm -rf "$dst/target" "$dst/.git" "$dst/.direnv"
  fi
}

write_cooldown_config() {
  local dst=$1
  cat >"$dst/cooldown.toml" <<EOF
[cooldown]
incompatible-publish-age = "$cooldown_incompatible"
lockfile-baseline = "$cooldown_baseline"
fallback-accept = "auto"

[registry]
global-min-publish-age = $(quote_toml_string "$age")
EOF
}

write_nightly_config() {
  local dst=$1
  mkdir -p "$dst"
  cat >"$dst/config.toml" <<EOF
[unstable]
min-publish-age = true

[resolver]
incompatible-publish-age = "deny"

[registry]
global-min-publish-age = $(quote_toml_string "$age")
EOF
}

make_cargo_home() {
  local dst=$1
  local source_home=${CARGO_HOME:-$HOME/.cargo}
  mkdir -p "$dst"

  for entry in registry git credentials credentials.toml; do
    if [[ -e "$source_home/$entry" && ! -e "$dst/$entry" ]]; then
      ln -s "$source_home/$entry" "$dst/$entry"
    fi
  done
}

run_in_workdir() {
  local name=$1
  local cargo_home=$2
  local workdir=$3
  local log_file="$logs_dir/$name.log"
  local time_file="$logs_dir/$name.time"
  local start_ns
  local end_ns
  local elapsed_ms
  local status
  shift 3

  log "Running $name"
  start_ns=$(date +%s%N)
  set +e
  (
    cd "$workdir"
    if [[ -x /usr/bin/time ]]; then
      /usr/bin/time -v -o "$time_file" env \
        CARGO_HOME="$cargo_home" \
        CARGO_TERM_COLOR=never \
        "$@"
    else
      env \
        CARGO_HOME="$cargo_home" \
        CARGO_TERM_COLOR=never \
        "$@"
    fi
  ) >"$log_file" 2>&1
  status=$?
  set -e
  end_ns=$(date +%s%N)
  elapsed_ms=$(((end_ns - start_ns) / 1000000))

  awk -v name="$name" -v status="$status" -v ms="$elapsed_ms" \
    'BEGIN { printf "%s\t%d\t%d\t%.3f\n", name, status, ms, ms / 1000 }' \
    >>"$out_dir/timing.tsv"

  if ((status != 0)); then
    die "$name failed with exit code $status; see $log_file"
  fi
}

copy_repo "$repo" "$cooldown_work"
copy_repo "$repo" "$nightly_work"

if ((fresh_lock)); then
  rm -f "$cooldown_work/Cargo.lock" "$nightly_work/Cargo.lock"
fi

write_cooldown_config "$cooldown_work"

cooldown_cargo_home="$out_dir/cargo-home-cooldown"
nightly_cargo_home="$out_dir/cargo-home-nightly"
make_cargo_home "$cooldown_cargo_home"
make_cargo_home "$nightly_cargo_home"
write_nightly_config "$nightly_cargo_home"

run_cargo_cooldown() {
  run_in_workdir "cargo-cooldown" "$cooldown_cargo_home" "$cooldown_work" \
    env "PATH=${toolchain_cargo_dir}:$PATH" \
    cargo cooldown update
}

run_nightly_min_publish_age() {
  run_in_workdir "nightly-min-publish-age" "$nightly_cargo_home" "$nightly_work" \
    cargo +"$toolchain" update -Zmin-publish-age
}

case "$order" in
  cooldown-first)
    run_cargo_cooldown
    run_nightly_min_publish_age
    ;;
  nightly-first)
    run_nightly_min_publish_age
    run_cargo_cooldown
    ;;
esac

[[ -f "$cooldown_work/Cargo.lock" ]] || die "cargo-cooldown did not produce Cargo.lock; see $logs_dir/cargo-cooldown.log"
[[ -f "$nightly_work/Cargo.lock" ]] || die "nightly Cargo did not produce Cargo.lock; see $logs_dir/nightly-min-publish-age.log"

cp "$cooldown_work/Cargo.lock" "$out_dir/cargo-cooldown/Cargo.lock"
cp "$nightly_work/Cargo.lock" "$out_dir/nightly-min-publish-age/Cargo.lock"

if diff -u \
  "$out_dir/cargo-cooldown/Cargo.lock" \
  "$out_dir/nightly-min-publish-age/Cargo.lock" \
  >"$out_dir/Cargo.lock.diff"; then
  locks_equal=1
else
  locks_equal=0
fi

python3 - "$out_dir/cargo-cooldown/Cargo.lock" "$out_dir/nightly-min-publish-age/Cargo.lock" \
  >"$out_dir/package-version-diff.tsv" <<'PY'
import sys
from collections import defaultdict

def parse_lock(path):
    packages = defaultdict(set)
    current = None
    with open(path, encoding="utf-8") as f:
        for raw in f:
            line = raw.strip()
            if line == "[[package]]":
                if current and current.get("name") and current.get("version"):
                    packages[current["name"]].add(current["version"])
                current = {}
                continue
            if current is None or " = " not in line:
                continue
            key, value = line.split(" = ", 1)
            if key in {"name", "version"}:
                current[key] = value.strip('"')
    if current and current.get("name") and current.get("version"):
        packages[current["name"]].add(current["version"])
    return packages

left = parse_lock(sys.argv[1])
right = parse_lock(sys.argv[2])

print("crate\tcargo-cooldown\tnightly-min-publish-age")
for name in sorted(set(left) | set(right)):
    if left.get(name, set()) != right.get(name, set()):
        l_versions = ",".join(sorted(left.get(name, set()))) or "-"
        r_versions = ",".join(sorted(right.get(name, set()))) or "-"
        print(f"{name}\t{l_versions}\t{r_versions}")
PY

if (( ! keep_workdirs )); then
  rm -rf "$work_root"
fi

printf '\n'
if ((locks_equal)); then
  log "Result: Cargo.lock files are identical"
else
  log "Result: Cargo.lock files differ"
fi

printf 'Output directory: %s\n' "$out_dir"
printf 'Full diff:        %s\n' "$out_dir/Cargo.lock.diff"
printf 'Package diff:     %s\n' "$out_dir/package-version-diff.tsv"
printf 'Timing:           %s\n' "$out_dir/timing.tsv"
printf 'Logs:             %s\n' "$logs_dir"

if (( ! locks_equal )); then
  exit 2
fi
