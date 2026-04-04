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

Skip a registry completely:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io cargo cooldown build
```

## Configuration

Supported environment variables:

- `COOLDOWN_MINUTES`
- `COOLDOWN_MODE`
- `COOLDOWN_NOW`
- `COOLDOWN_ALLOWLIST_PATH`
- `COOLDOWN_TTL_SECONDS`
- `COOLDOWN_CACHE_DIR`
- `COOLDOWN_HTTP_RETRIES`
- `COOLDOWN_VERBOSE`
- `COOLDOWN_SKIP_REGISTRIES`

Supported `cooldown.toml` keys:

- `cooldown_minutes`
- `mode`
- `now`
- `allowlist_path`
- `ttl_seconds`
- `cache_dir`
- `http_retries`
- `verbose`
- `skip_registries`

`skip_registries` means "do not process these registries for cooldown at all".
Those packages are left untouched, but they still contribute semver constraints
to the overall graph.

If a registry is not skipped and `cargo-cooldown` cannot determine release age
from the local index or the registry API, the cooldown step fails in `enforce`
mode and becomes a warning in `warn` mode.

## Docs

- [Overview](docs/overview.md)
- [Resolution Flow](docs/resolution-flow.md)
- [Registries](docs/registries.md)
- [Configuration](docs/configuration.md)
- [Migration Guide](docs/migration-guide.md)
- [Testing](docs/testing.md)
- [Troubleshooting](docs/troubleshooting.md)

## Examples

- `examples/demo/` contains a small crates.io-backed workspace for manual runs.
- `examples/smoke-test-crates-io.sh` exercises a few non-deterministic scenarios
  against the current crates.io state.
- the deterministic integration suite lives in `./tests`.
  It generates its registry and workspace fixtures at runtime instead of relying
  on committed snapshots under `examples/fixtures`.

## Status

`cargo-cooldown` is intended for local development workflows where you refresh
dependencies and build immediately. CI pipelines and release automation should
continue to use plain Cargo against committed `Cargo.lock` files.
