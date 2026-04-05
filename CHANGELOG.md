# Changelog

## 0.3.0 - 2026-04-04

This is an intentionally breaking release.

Migration guide:

- [Migration Guide](docs/migration-guide.md)

### Added

- multi-registry support for Cargo registries, mirrors, and source replacements
- index-first release-time resolution from Cargo's local registry cache
- per-crate HTTP fallback when `pubtime` is missing
- explicit `skip_registries` / `COOLDOWN_SKIP_REGISTRIES`
- explicit `lockfile_policy` / `COOLDOWN_LOCKFILE_POLICY` to choose between
  cooling only changed lockfile entries or cooling all eligible entries
- `cargo cooldown update` to refresh the lockfile first and then cool only the
  versions that changed relative to the pre-update baseline
- `cargo cooldown init` to scaffold `cooldown.toml` interactively for crates
  and workspaces
- new integration tests in `./tests`
- new documentation under `./docs`

### Changed

- configuration now lives in a single `cooldown.toml`, with allow rules under
  the embedded `allow` section
- resolver now works from a single release timeline per crate
- cooldown now follows Cargo's effective registry configuration instead of a
  separate registry routing layer
- cooldown now snapshots the initial `Cargo.lock` once per execution and, by
  default, skips registry versions that were already present in that baseline
- cooldown now respects Cargo workspace selectors so package-scoped runs only
  cool the selected workspace members and their dependency closure
- config discovery now starts from the effective Cargo root, with optional
  member overrides only for uniquely targeted workspace members
- repeated outer-loop scans now reuse in-memory registry timelines and locked
  version age inspections within one cooldown execution
- missing release-time metadata is fail-closed in `enforce` mode and downgraded
  to warnings only in `warn` mode

### Breaking changes

- `COOLDOWN_REGISTRY_INDEX`, `COOLDOWN_REGISTRY_API`, and
  `COOLDOWN_OFFLINE_OK` are replaced by the current registry-aware model; see
  the [Migration Guide](docs/migration-guide.md)
- `cooldown-allowlist.toml`, `allowlist_path`, and
  `COOLDOWN_ALLOWLIST_PATH` were removed in favor of one `cooldown.toml`
- default cooldown behavior now respects the initial `Cargo.lock` baseline
  unless `lockfile_policy = "all"` is enabled

### Fixed

- reduced dependency on crates.io HTTP metadata when the local index already
  contains `pubtime`
- clearer distinction between registries that are skipped and registries that
  fail because metadata is incomplete
- cooldown now restores the original `Cargo.lock` if Cargo re-resolves during
  inspection and the cooldown run ultimately fails
- `--manifest-path` is now honored during both cooldown inspection and
  `cargo update --precise` pinning, including runs started from another cwd
- Cargo-style selectors such as `--manifest-path`, `--package`, `--workspace`,
  `--exclude`, and feature flags are now parsed correctly even when passed
  after the forwarded Cargo subcommand
