# cargo-cooldown

`cargo-cooldown` is a Cargo wrapper that delays adoption of freshly published
registry crate versions. It lets Cargo resolve the graph, then replaces fresh
versions with the newest older compatible versions that Cargo still accepts.

Use it when you want dependency updates, but do not want `Cargo.lock` to pick
up releases that were published too recently.

## Quick Start

Install:

```bash
cargo install --locked cargo-cooldown
```

For CI jobs that only need the `cargo-cooldown` binary, install from the
project's prebuilt GitHub release artifacts with `cargo-binstall`:

```bash
cargo install cargo-binstall --locked
cargo binstall cargo-cooldown --no-confirm
```

Releases include `SHA256SUMS` and GitHub Artifact Attestations. After
downloading an archive from a release, verify its provenance with:

```bash
gh attestation verify cargo-cooldown-x86_64-unknown-linux-gnu-vX.Y.Z.tgz \
  -R dertin/cargo-cooldown
```

Create a project config:

```bash
cargo cooldown init
```

Run a Cargo command through cooldown:

```bash
cargo cooldown check
```

`check`, `build`, `test`, and `run` are guard-style commands: cooldown runs a
pre-command `Cargo.lock` cooldown pass before Cargo downloads, compiles, tests,
or runs code from the dependency graph. With the default
`lockfile-baseline = "floor"` under `[cooldown]`, versions already present in
the initial `Cargo.lock` are treated as the protected baseline; use
`lockfile-baseline = "ignore"` under `[cooldown]` when these commands should
also try to cool already-locked versions before Cargo consumes them.

Update dependencies under cooldown:

```bash
cargo cooldown update
```

`update` is the `Cargo.lock` refresh command: Cargo resolves the newest graph
first, then cooldown cools the updated `Cargo.lock` before it is kept.

`cargo cooldown init` is cargo-cooldown's setup wizard. To create a new Cargo
package, use Cargo's own command:

```bash
cargo init
```

## Basic Config

`cooldown.toml` usually starts with:

```toml
[cooldown]
incompatible-publish-age = "deny"
lockfile-baseline = "floor"

[registry]
global-min-publish-age = "14 days"
```

Meaning:

- `[registry].global-min-publish-age`: how old a release must be before
  cooldown accepts it
- `[cooldown].incompatible-publish-age`: what to do if Cargo still requires
  fresh versions
- `[cooldown].lockfile-baseline`: whether the initial `Cargo.lock` is used as a
  version floor

Config is loaded in this order, from strongest to weakest:

1. environment variables
2. active member `cooldown.toml`, when exactly one workspace member is targeted
3. workspace or crate root `cooldown.toml`
4. `$CARGO_HOME/cooldown.toml`

## Cooldown Policy vs Cargo.lock Baseline

These settings answer different questions:

- `[cooldown].lockfile-baseline` controls whether cooldown may go below versions
  already present in the initial `Cargo.lock`.
- `[cooldown].incompatible-publish-age` controls what happens if Cargo still
  requires fresh versions.

`lockfile-baseline = "ignore"` is not a force setting. Cargo still validates
the final graph, so cooldown never writes a `Cargo.lock` that Cargo rejects.

| Configuration | Meaning |
| --- | --- |
| `lockfile-baseline = "floor"` + `incompatible-publish-age = "deny"` | `cargo cooldown init` default and RFC-aligned fail-closed policy. Use the pre-run `Cargo.lock` as the minimum version floor. If any new fresh version remains, fail and restore the original `Cargo.lock`. |
| `lockfile-baseline = "floor"` + `incompatible-publish-age = "fallback"` | Use the pre-run `Cargo.lock` as the minimum version floor, keep the best `Cargo.lock` that Cargo accepts if some fresh versions remain, and warn. Useful for long min-publish-age windows or benchmark runs. |
| `lockfile-baseline = "ignore"` + `incompatible-publish-age = "deny"` | Try to cool every eligible locked registry package, including versions already present before the run. If any fresh version still cannot be cooled, fail and restore the original `Cargo.lock`. |
| `lockfile-baseline = "ignore"` + `incompatible-publish-age = "fallback"` | Most permissive update policy. Try to cool everything, keep the best `Cargo.lock` that Cargo accepts, and warn about any remaining fresh versions. |

A fresh version can remain when the current `Cargo.toml` graph requires it. That
can happen because of semver ranges, exact dependencies, feature-selected
dependencies, target-specific dependencies, or a group of crates that does not
have an older compatible combination.

`incompatible-publish-age = "fallback"` is flexible only where Cargo
requires a fresh version. It still cools every package Cargo can accept and
reports the remaining fresh versions so you can review the supply-chain risk.

By default, `fallback` asks before accepting unresolved fresh versions.
Use `fallback-accept = "auto"` only when you want the previous
non-interactive behavior.

Use cargo-cooldown's own variables for policy overrides:

```bash
COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback
COOLDOWN_FALLBACK_ACCEPT=prompt
```

## Allow Rules

Allow rules live in `cooldown.toml`:

```toml
[[allow.exact]]
crate = "serde"
version = "1.0.218"

[[allow.package]]
crate = "tokio"
min-publish-age = "1 hour"

[[allow.package]]
crate = "openssl"
min-publish-age = "0"
```

Use:

- `[[allow.exact]]` to allow one exact crate version
- `[[allow.package]]` to use a shorter cooldown for one crate
- `min-publish-age = "0"` to exclude one crate from cooldown

Allow rules only reduce the effective cooldown window. They do not make a crate
wait longer than the configured min publish age.

## Registries

Cargo's registry configuration is the source of truth. `cargo-cooldown` reads
release timestamps from the local registry index first and uses registry HTTP
fallback only when local `pubtime` is missing.

Skip a registry completely:

```toml
skip_registries = ["crates-io", "sparse+https://example.com/index/"]
```

Skipped registries are not inspected or downgraded, but their packages still
shape Cargo's dependency graph.

Set registry-specific min publish ages in `cooldown.toml` without reading
Cargo's config files as policy:

```toml
[registry]
global-min-publish-age = "14 days"
min-publish-age = "5 days"

[registries.internal]
index = "sparse+https://example.com/index/"
min-publish-age = "0"
```

`index` is optional. When present, it matches the effective registry index URL;
otherwise cargo-cooldown resolves the registry name through the Cargo registry
configuration Cargo already uses. Duration values accept `0` or `N seconds`,
`minutes`, `hours`, `days`, `weeks`, or `months`; `months` means fixed 30-day
months.

## Workspaces

Recommended layout:

- one shared `cooldown.toml` at the workspace root
- optional `member/cooldown.toml` overrides only for member-specific runs

Member overrides apply only when the command targets exactly one member.

## Docs

- [Overview](docs/overview.md)
- [Configuration](docs/configuration.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Registries](docs/registries.md)
- [Resolution Flow](docs/resolution-flow.md)
- [Migration Guide](docs/migration-guide.md)
- [Releasing](docs/releasing.md)
- [Testing](docs/testing.md)

## Examples

- `examples/crates-io-smoke-workspace/`: small crates.io-backed smoke workspace
- `examples/crates-io-large-benchmark-workspace/`: larger benchmark workspace
- `examples/run-crates-io-smoke.sh`: manual smoke checks
- `examples/run-crates-io-benchmark.sh`: shared benchmark runner

Run the large benchmark:

```bash
cargo bench --bench crates_io_cooldown -- --scenario large-60d
```

## Status

`cargo-cooldown` is intended for local development workflows where you refresh
dependencies and build immediately. CI pipelines and release automation should
usually use plain Cargo against committed `Cargo.lock` files.
