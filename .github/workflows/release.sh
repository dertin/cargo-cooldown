#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "Usage: $0 <version>" >&2
  echo "Example: $0 0.3.1" >&2
  exit 2
fi

version="$1"
tag="v${version}"

case "$version" in
  [0-9]*.[0-9]*.[0-9]*)
    ;;
  *)
    echo "Version must look like X.Y.Z; got ${version}" >&2
    exit 2
    ;;
esac

manifest_version="$(cargo pkgid | sed 's/.*#//')"
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

cargo publish --locked

git tag -s "$tag" -m "cargo-cooldown ${tag}"
git push origin "$tag"

gh workflow run release.yml --ref main -f "tag=${tag}"
gh run watch
