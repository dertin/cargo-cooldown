# cargo-cooldown

`cargo-cooldown` is a Cargo wrapper that delays adoption of freshly published
registry crate versions. It lets Cargo resolve the graph, then replaces fresh
versions with the newest older compatible versions that Cargo still accepts.

Use it when you want dependency updates, but do not want your lockfile to pick
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
pre-command lockfile pass before Cargo downloads, compiles, tests, or runs code
from the dependency graph. With the default `lockfile_baseline = "floor"`,
versions already present in the initial `Cargo.lock` are treated as the
protected baseline; use `lockfile_baseline = "ignore"` when these commands
should also try to cool already-locked versions before Cargo consumes them.

Update dependencies under cooldown:

```bash
cargo cooldown update
```

`update` is the lockfile-refresh command: Cargo resolves the newest graph first,
then cooldown cools the updated lockfile before it is kept.

`cargo cooldown init` is cargo-cooldown's setup wizard. To create a new Cargo
package, use Cargo's own command:

```bash
cargo init
```

## Basic Config

`cooldown.toml` usually starts with:

```toml
cooldown_minutes = 1440
enforcement = "cargo_compatible"
cargo_compatible_accept = "prompt"
lockfile_baseline = "floor"
```

Meaning:

- `cooldown_minutes`: how old a release must be before cooldown accepts it
- `enforcement`: what to do if Cargo still requires fresh versions
- `cargo_compatible_accept`: whether to prompt before accepting fresh versions
  Cargo still requires
- `lockfile_baseline`: whether the initial `Cargo.lock` is used as a version
  floor

Config is loaded in this order, from strongest to weakest:

1. environment variables
2. active member `cooldown.toml`, when exactly one workspace member is targeted
3. workspace or crate root `cooldown.toml`
4. `$CARGO_HOME/cooldown.toml`

## `enforcement` vs `lockfile_baseline`

These settings answer different questions:

- `lockfile_baseline` controls whether cooldown may go below versions already
  present in the initial `Cargo.lock`.
- `enforcement` controls what happens if Cargo still requires fresh versions.

`lockfile_baseline = "ignore"` is not a force setting. Cargo still validates the
final graph, so cooldown never writes a lockfile that Cargo rejects.

| Configuration | Meaning |
| --- | --- |
| `lockfile_baseline = "floor"` + `enforcement = "cargo_compatible"` | `cargo cooldown init` default. Use the pre-run lockfile as the minimum version floor, keep the best Cargo-valid lockfile if some fresh versions remain, and warn. |
| `lockfile_baseline = "floor"` + `enforcement = "strict"` | Fail-closed policy. Use the pre-run lockfile as the minimum version floor. If any new fresh version remains, fail and restore the original `Cargo.lock`. |
| `lockfile_baseline = "ignore"` + `enforcement = "strict"` | Try to cool every eligible locked registry package, including versions already present before the run. If any fresh version still cannot be cooled, fail and restore the original `Cargo.lock`. |
| `lockfile_baseline = "ignore"` + `enforcement = "cargo_compatible"` | Most permissive update policy. Try to cool everything, keep Cargo's best valid result, and warn about any remaining fresh versions. |

A fresh version can remain when the current `Cargo.toml` graph requires it. That
can happen because of semver ranges, exact dependencies, feature-selected
dependencies, target-specific dependencies, or a group of crates that does not
have an older compatible combination.

`enforcement = "cargo_compatible"` is flexible only where Cargo requires a fresh
version. It still cools every package Cargo can accept and reports the remaining
fresh versions so you can review the supply-chain risk.

By default, `cargo_compatible` asks before accepting unresolved fresh versions.
Use `cargo_compatible_accept = "auto"` only when you want the previous
non-interactive behavior.

## Allow Rules

Allow rules live in `cooldown.toml`:

```toml
[[allow.exact]]
crate = "serde"
version = "1.0.218"

[[allow.package]]
crate = "tokio"
minutes = 60

[[allow.package]]
crate = "openssl"
minutes = 0
```

Use:

- `[[allow.exact]]` to allow one exact crate version
- `[[allow.package]]` to use a shorter cooldown for one crate
- `minutes = 0` to exclude one crate from cooldown

Allow rules only reduce the effective cooldown window. They do not make a crate
wait longer than `cooldown_minutes`.

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
