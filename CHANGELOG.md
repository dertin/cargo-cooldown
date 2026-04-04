# Changelog

## 0.3.0 - Unreleased

This is an intentionally breaking release.

Migration guide:

- [Migration Guide](docs/migration-guide.md)

### Added

- index-first release-time resolution from Cargo's local registry cache
- per-crate HTTP fallback when `pubtime` is missing
- multi-registry support for Cargo registries, mirrors, and source replacements
- explicit `skip_registries` / `COOLDOWN_SKIP_REGISTRIES`
- explicit `lockfile_policy` / `COOLDOWN_LOCKFILE_POLICY` to choose between
  cooling only changed lockfile entries or cooling all eligible entries
- new integration tests in `./tests`
- new documentation under `./docs`

### Changed

- resolver now works from a single release timeline per crate
- cooldown now follows Cargo's effective registry configuration instead of a
  separate registry routing layer
- cooldown now snapshots the initial `Cargo.lock` once per execution and, by
  default, skips registry versions that were already present in that baseline
- cooldown now respects Cargo workspace selectors so package-scoped runs only
  cool the selected workspace members and their dependency closure
- repeated outer-loop scans now reuse in-memory registry timelines and locked
  version age inspections within one cooldown execution
- missing release-time metadata is fail-closed in `enforce` mode and downgraded
  to warnings only in `warn` mode

### Breaking changes

- configuration and registry-scoping changes require migration from older
  setups; see the [Migration Guide](docs/migration-guide.md)

### Fixed

- reduced dependency on crates.io HTTP metadata when the local index already
  contains `pubtime`
- clearer distinction between registries that are skipped and registries that
  fail because metadata is incomplete
- `--manifest-path` is now honored during both cooldown inspection and
  `cargo update --precise` pinning, including runs started from another cwd
- Cargo-style selectors such as `--manifest-path`, `--package`, `--workspace`,
  `--exclude`, and feature flags are now parsed correctly even when passed
  after the forwarded Cargo subcommand
