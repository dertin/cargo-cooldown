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

Create a project config:

```bash
cargo cooldown init
```

Run a Cargo command through cooldown:

```bash
cargo cooldown check
```

Update dependencies under cooldown:

```bash
cargo cooldown update
```

`cargo cooldown init` is cargo-cooldown's setup wizard. To create a new Cargo
package, use Cargo's own command:

```bash
cargo init
```

## Basic Config

`cooldown.toml` usually starts with:

```toml
cooldown_minutes = 1440
mode = "strict"
lockfile_policy = "changed"
```

Meaning:

- `cooldown_minutes`: how old a release must be before cooldown accepts it
- `mode`: what to do if some fresh versions cannot be cooled
- `lockfile_policy`: whether already locked versions are protected

Config is loaded in this order, from strongest to weakest:

1. environment variables
2. active member `cooldown.toml`, when exactly one workspace member is targeted
3. workspace or crate root `cooldown.toml`
4. `$CARGO_HOME/cooldown.toml`

## `mode` vs `lockfile_policy`

These settings answer different questions:

- `lockfile_policy` controls what cooldown is allowed to try to downgrade.
- `mode` controls what happens if Cargo still requires fresh versions.

`lockfile_policy = "all"` is not a force mode. Cargo still validates the final
graph, so cooldown never writes a lockfile that Cargo rejects.

| Config | Human meaning |
| --- | --- |
| `lockfile_policy = "changed"` + `mode = "strict"` | Default. Cool only versions introduced or changed by this run. If any new fresh version remains, fail and restore the original `Cargo.lock`. |
| `lockfile_policy = "changed"` + `mode = "best_effort"` | Protect the pre-run lockfile, cool what changed, keep the best valid lockfile if some fresh versions remain, and warn. |
| `lockfile_policy = "all"` + `mode = "strict"` | Try to cool every eligible locked registry package, including versions already present before the run. If any fresh version still cannot be cooled, fail and restore the original `Cargo.lock`. |
| `lockfile_policy = "all"` + `mode = "best_effort"` | Most permissive update mode. Try to cool everything, keep Cargo's best valid result, and warn about any remaining fresh versions. |

A fresh version can remain when the current `Cargo.toml` graph requires it. That
can happen because of semver ranges, exact dependencies, feature-selected
dependencies, target-specific dependencies, or a group of crates that does not
have an older compatible combination.

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
