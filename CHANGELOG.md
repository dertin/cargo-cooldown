# Changelog

## 0.3.1 - 2026-05-15

### Added

- `cargo cooldown --help` now lists the documented `version`, `check`, `build`,
  `test`, `run`, and `update` commands while continuing to forward any other
  Cargo command through cooldown.
- `cargo cooldown --version` now prints the cargo-cooldown version directly.
- RFC-style min publish age config aligned with the
  [Cargo RFC for min publish age](https://github.com/rust-lang/rfcs/pull/3923):
  `[registry].global-min-publish-age`, `[registry].min-publish-age`,
  `[registries.<name>].min-publish-age`, `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE`,
  `CARGO_REGISTRY_MIN_PUBLISH_AGE`, and
  `CARGO_REGISTRIES_<name>_MIN_PUBLISH_AGE`.
- Policy environment overrides remain namespaced under
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE` and
  `COOLDOWN_FALLBACK_ACCEPT`.
- New canonical cargo-cooldown extension section:
  `[cooldown].incompatible-publish-age`,
  `[cooldown].fallback-accept`, and
  `[cooldown].lockfile-baseline`.
- `cargo cooldown init` now emits `[registry].global-min-publish-age` instead of
  `cooldown_minutes`, and emits cargo-cooldown-specific policy under
  `[cooldown]`. Generated configs default to the RFC-style fail-closed
  `incompatible-publish-age = "deny"` policy.
- `[[allow.package]]` now accepts RFC-style `min-publish-age = "N unit"`
  duration strings, including `min-publish-age = "0"` to exempt one crate from
  cooldown. The older `minutes` form remains supported for compatibility.

### Deprecated

- 0.3.0 compatibility aliases remain supported in 0.3.1 and are deprecated for
  planned removal in the next major 0.4.x line: `cooldown_minutes`,
  `COOLDOWN_MINUTES`, root `enforcement`, `COOLDOWN_ENFORCEMENT`, root
  `cargo_compatible_accept`, `COOLDOWN_CARGO_COMPATIBLE_ACCEPT`, and root
  `lockfile_baseline`.

### Fixed

- `cargo cooldown version` now only prints the cargo-cooldown version and no
  longer runs cooldown or forwards to `cargo version`.
- Cooldown now runs when the global min publish age is `0` but a crates.io or
  named registry min publish age override is positive, so registry-specific
  policy is enforced for both guard commands and `cargo cooldown update`.
- Overlapping named registry min publish age overrides are now resolved
  deterministically, with explicit `index` matches taking precedence over
  registry-name matches.
- Layered `[registries.<name>]` overrides now preserve an existing `index`
  when a higher-precedence config only changes `min-publish-age`.
- Layered `[registries.<name>]` overrides now preserve an existing
  `min-publish-age` when a higher-precedence config only changes `index`.
- Invalid registry names in min publish age overrides now produce an error
  instead of being silently ignored.
- Registry min publish age environment overrides now preserve underscores in
  registry names instead of always rewriting them to dashes.

## 0.3.0 - 2026-04-26

This is an intentionally breaking release.

Migration guide:

- [Migration Guide](docs/migration-guide.md)

### Added

- multi-registry support for Cargo registries, mirrors, and source replacements
- index-first release-time resolution from Cargo's local registry cache
- per-crate HTTP fallback when `pubtime` is missing
- explicit `skip_registries` / `COOLDOWN_SKIP_REGISTRIES`
- explicit `lockfile_baseline` / `COOLDOWN_LOCKFILE_BASELINE` to choose between
  using the initial lockfile as a version floor or ignoring that floor
- explicit `enforcement` / `COOLDOWN_ENFORCEMENT` to choose strict rollback,
  `cargo_compatible` warnings, or fully disabled cooldown
- explicit `cargo_compatible_accept` / `COOLDOWN_CARGO_COMPATIBLE_ACCEPT` to
  choose prompt-based review or automatic acceptance for unresolved fresh
  versions under `cargo_compatible` enforcement
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
- missing release-time metadata is fail-closed under `strict` enforcement and
  downgraded to warnings only under `cargo_compatible` enforcement
- `cargo_compatible` now prompts before accepting resolver-constrained fresh
  versions unless `cargo_compatible_accept = "auto"` is configured
- cooldown now resolves and cools lockfiles in a temporary workspace, holding
  the real root `Cargo.lock` with a backup plus sentinel until the final
  Cargo-valid lockfile is ready to publish
- `verbose = true` / `COOLDOWN_VERBOSE=true` now surfaces cooldown internals as
  `DEBUG` logs while keeping user-facing `INFO`/`WARN` output compact

### Breaking changes

Configuration, registry discovery, allow rules, lockfile baseline handling, and
enforcement names changed in this release. See the
[Migration Guide](docs/migration-guide.md) for the upgrade steps.

### Fixed

- reduced dependency on crates.io HTTP metadata when the local index already
  contains `pubtime`
- clearer distinction between registries that are skipped and registries that
  fail because metadata is incomplete
- cooldown now restores the original `Cargo.lock` if Cargo re-resolves during
  inspection and the cooldown run ultimately fails
- `lockfile_baseline = "floor"` now allows `cargo cooldown update` to pin a
  freshly updated crate back to an exact version from the initial baseline,
  even when that baseline version is still inside the cooldown window
- cooldown now treats blockers or parent constraints that were already
  exhausted earlier in the run as fallback skips instead of failing
  later with a generic fixed-point error
- cooldown now emits a single final warning when fresh versions remain,
  distinguishing baseline-carried versions from resolver-constrained ones, and
  `strict` enforcement now turns remaining resolver-constrained fresh versions
  into a rollback error instead of allowing them through
- cooldown now makes one bounded coordinated bundle attempt for small
  resolver-constrained groups, which helps cool tightly coupled crates such as
  `js-sys` / `wasm-bindgen*` / `web-sys` when individual pins cannot progress
- successful `cargo cooldown update` runs now keep the initial `cargo update`
  chatter hidden, so user-facing output stays focused on cooldown results
- `--manifest-path` is now honored during both cooldown inspection and
  `cargo update --precise` pinning, including runs started from another cwd
- Cargo-style selectors such as `--manifest-path`, `--package`, `--workspace`,
  `--exclude`, and feature flags are now parsed correctly even when passed
  after the forwarded Cargo subcommand
