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

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

github_repo_from_remote() {
  local url
  url="$(git remote get-url origin)"
  case "$url" in
    git@github.com:*)
      printf '%s\n' "${url#git@github.com:}" | sed 's/\.git$//'
      ;;
    https://github.com/*)
      printf '%s\n' "${url#https://github.com/}" | sed 's/\.git$//'
      ;;
    *)
      echo "Unsupported origin remote: ${url}" >&2
      exit 1
      ;;
  esac
}

github_repo="$(github_repo_from_remote)"

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

trigger_github_release() {
  gh workflow run release.yml -R "$github_repo" --ref main -f "tag=${tag}"

  local run_id=""
  for _ in {1..30}; do
    run_id="$(
      gh run list \
        -R "$github_repo" \
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

  gh run watch "$run_id" -R "$github_repo" --exit-status
}

if git ls-remote --exit-code --tags origin "refs/tags/${tag}" >/dev/null 2>&1; then
  if gh release view "$tag" -R "$github_repo" >/dev/null 2>&1; then
    echo "Release ${tag} is already complete on GitHub." >&2
    exit 0
  fi

  echo "Tag ${tag} already exists on origin; triggering GitHub release workflow only." >&2
  trigger_github_release
  exit 0
fi

if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "Tag ${tag} exists locally but not on origin; push it or delete it first." >&2
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

trigger_github_release
