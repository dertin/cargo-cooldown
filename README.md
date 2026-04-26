# cargo-cooldown

`cargo-cooldown` is a Cargo wrapper that delays adoption of freshly published
crate versions. It inspects the lockfile graph, finds registry packages that are
younger than the configured cooldown window, and pins them to the newest older
compatible release before re-running the requested Cargo command.

The tool is registry-aware:

- it reads release timestamps from Cargo's local registry index cache first;
- it falls back to the registry HTTP API only when `pubtime` is missing;
- it can skip specific registries entirely with `skip_registries`.

## Quick start

Install:

```bash
cargo install --locked cargo-cooldown
```

Inspect the CLI:

```bash
cargo cooldown --help
```

Run a command through the cooldown guard:

```bash
COOLDOWN_MINUTES=1440 cargo cooldown check
```

Refresh the lockfile under cooldown:

```bash
cargo cooldown update
```

Initialize `cooldown.toml` in the current project root:

```bash
cargo cooldown init
```

`cargo cooldown init` is the cooldown configuration wizard. To create a new
Cargo package, use plain `cargo init`.

Skip a registry completely:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io cargo cooldown build
```

## Configuration

Supported environment variables:

- `COOLDOWN_MINUTES`
- `COOLDOWN_MODE`
- `COOLDOWN_LOCKFILE_POLICY`
- `COOLDOWN_NOW`
- `COOLDOWN_TTL_SECONDS`
- `COOLDOWN_CACHE_DIR`
- `COOLDOWN_HTTP_RETRIES`
- `COOLDOWN_VERBOSE`
- `COOLDOWN_SKIP_REGISTRIES`

Supported `cooldown.toml` keys:

- `cooldown_minutes`
- `mode`
- `lockfile_policy`
- `now`
- `ttl_seconds`
- `cache_dir`
- `http_retries`
- `verbose`
- `skip_registries`

`cooldown.toml` accepts the lowercase keys shown above. Unknown keys and
invalid values fail configuration loading.

Set `verbose = true` when you want `DEBUG` logs for cooldown internals. Normal
user-facing output stays compact: cooldown prints one final summary block with
Cargo-style version lines that merge the net `Cargo.lock` changes from `cargo
update` with any cooldown adjustments, plus any fresh versions that had to
remain. On interactive terminals, cooldown also shows a resolver progress bar
and colored Cargo-style status labels.

Cooldown uses one verified lockfile batch path by default. It reads local
registry dependency metadata, solves compatible dependency components, rewrites
the selected `Cargo.lock` entries, and then asks Cargo to validate the final
graph. Duplicate package names are eligible because cooldown tracks packages by
current locked version and validates the result with Cargo.

Define allow rules in `cooldown.toml`:

```toml
[allow.global]
minutes = 1440

[[allow.exact]]
crate = "serde"
version = "1.0.218"

[[allow.exact]]
crate = "serde_json"
version = "1.0.145"

[[allow.package]]
crate = "tokio"
minutes = 60

[[allow.package]]
crate = "openssl"
minutes = 0
```

Rule semantics:

- `[allow.global]` sets a default cooldown override for every registry crate
- each `[[allow.package]]` entry applies to one crate name, and you can define
  many different crates with different cooldowns
- `allow.global` and `allow.package` only reduce the effective cooldown window;
  they do not increase it above `cooldown_minutes`
- `minutes = 0` in `[[allow.package]]` excludes that crate from cooldown
- each `[[allow.exact]]` entry allows one exact `(crate, version)` pair, and
  you can list as many pairs as you need

Configuration is resolved in this order:

1. environment variables
2. `cooldown.toml` in the active member, when the run targets a unique member
3. `cooldown.toml` in the workspace root or crate root
4. `$CARGO_HOME/cooldown.toml`

`skip_registries` means "do not process these registries for cooldown at all".
Those packages are left untouched, but they still contribute semver constraints
to the overall graph.

If a registry is not skipped and `cargo-cooldown` cannot determine release age
from the local index or the registry API, the cooldown step fails in `strict`
mode and becomes a warning in `best_effort` mode.

`mode` accepts exactly these values:

- `strict`
  requires the final lockfile to satisfy cooldown for every newly introduced or
  updated registry version; if resolver-constrained fresh versions remain,
  cooldown fails and restores the original `Cargo.lock`
- `best_effort`
  cools as much of the graph as possible, keeps any remaining
  resolver-constrained fresh versions, and reports them at the end
- `off`
  disables cooldown entirely

`cargo-cooldown` supports two different workflows:

- `cargo cooldown build|check|test|run`
  uses the current lockfile and cools any newly resolved versions that appear
  during that command
- `cargo cooldown update`
  snapshots the current `Cargo.lock`, runs `cargo update`, and then cools only
  the versions that are new relative to that pre-update baseline when
  `lockfile_policy = "changed"`; versions that were already locked before the
  update act as the minimum allowed version. With `lockfile_policy = "all"`,
  every eligible locked package is checked against the cooldown window.

`cargo cooldown init` is reserved for the cooldown configuration wizard rather
than forwarding to Cargo's own `init`.

In a workspace, the recommended layout is:

- one shared `cooldown.toml` at the workspace root
- optional `member/cooldown.toml` overrides only for runs that target that
  member uniquely

## Docs

- [Overview](docs/overview.md)
- [Resolution Flow](docs/resolution-flow.md)
- [Registries](docs/registries.md)
- [Configuration](docs/configuration.md)
- [Migration Guide](docs/migration-guide.md)
- [Testing](docs/testing.md)
- [Troubleshooting](docs/troubleshooting.md)

## Examples

- `examples/crates-io-smoke-workspace/` contains a small crates.io-backed
  workspace for manual runs and the default benchmark.
- `examples/crates-io-large-benchmark-workspace/` contains a larger
  crates.io-backed workspace for the aggressive 60-day benchmark.
- `examples/run-crates-io-smoke.sh` exercises a few non-deterministic scenarios
  against the current crates.io state.
- `examples/run-crates-io-benchmark.sh` is the shared wall-clock benchmark
  runner; it reports average time and registry API fallback usage.
- `cargo bench --bench crates_io_cooldown -- --scenario large-60d` runs the
  same benchmark runner through Cargo's benchmark command.
- `examples/README.md` maps every examples workspace to the scripts that use it.
- the deterministic integration suite lives in `./tests`.
  It generates its registry and workspace fixtures at runtime instead of relying
  on committed snapshots under `examples/fixtures`.

## Status

`cargo-cooldown` is intended for local development workflows where you refresh
dependencies and build immediately. CI pipelines and release automation should
continue to use plain Cargo against committed `Cargo.lock` files.
