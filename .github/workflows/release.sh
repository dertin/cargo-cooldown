#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <version>" >&2
  echo "Example: $0 0.3.1" >&2
  exit 2
fi

version="$1"
tag="v${version}"

if [[ ! "$version" =~ ^[0-9]+[.][0-9]+[.][0-9]+$ ]]; then
  echo "Version must look like X.Y.Z; got ${version}" >&2
  exit 2
fi

cargo_manifest_version() {
  local pkgid
  pkgid="$(cargo pkgid)"
  pkgid="${pkgid##*#}"
  printf '%s\n' "${pkgid##*@}"
}

manifest_version="$(cargo_manifest_version)"
if [ "$manifest_version" != "$version" ]; then
  echo "Cargo.toml version ${manifest_version} does not match ${version}" >&2
  exit 1
fi

current_branch="$(git branch --show-current)"
if [ "$current_branch" != "main" ]; then
  echo "Release must run from main; current branch is ${current_branch}" >&2
  exit 1
fi

git fetch origin main --tags

local_head="$(git rev-parse HEAD)"
remote_head="$(git rev-parse origin/main)"
if [ "$local_head" != "$remote_head" ]; then
  echo "Local main is not aligned with origin/main" >&2
  exit 1
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "Working tree must be clean before release" >&2
  exit 1
fi

if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "Tag ${tag} already exists locally" >&2
  exit 1
fi

if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
  echo "Tag ${tag} already exists on origin" >&2
  exit 1
fi

cargo publish --locked --dry-run

git tag -s "$tag" -m "cargo-cooldown ${tag}"
if ! git push origin "$tag"; then
  git tag -d "$tag" >/dev/null 2>&1 || true
  echo "Failed to push ${tag}; removed the local tag and did not publish the crate." >&2
  exit 1
fi

if ! cargo publish --locked; then
  echo "cargo publish failed after ${tag} was pushed. Inspect crates.io before retrying or deleting the tag." >&2
  exit 1
fi

gh workflow run release.yml --ref main -f "tag=${tag}"

run_id=""
for _ in {1..30}; do
  run_id="$(
    gh run list \
      --workflow release.yml \
      --event workflow_dispatch \
      --branch main \
      --commit "$remote_head" \
      --json databaseId,displayTitle \
      --jq ".[] | select(.displayTitle == \"Release ${tag}\") | .databaseId" \
      --limit 20 |
      head -n 1
  )"
  if [ -n "$run_id" ]; then
    break
  fi
  sleep 2
done

if [ -z "$run_id" ]; then
  echo "Could not find the release workflow run for ${tag}" >&2
  exit 1
fi

gh run watch "$run_id" --exit-status
