# Configuration

`cargo-cooldown` reads configuration from:

1. environment variables
2. `cooldown.toml` in the workspace root
3. `cooldown.toml` in `$CARGO_HOME`

Environment variables always win.

## Supported keys

Environment variables:

- `COOLDOWN_MINUTES`
- `COOLDOWN_MODE`
- `COOLDOWN_LOCKFILE_POLICY`
- `COOLDOWN_NOW`
- `COOLDOWN_ALLOWLIST_PATH`
- `COOLDOWN_TTL_SECONDS`
- `COOLDOWN_CACHE_DIR`
- `COOLDOWN_HTTP_RETRIES`
- `COOLDOWN_VERBOSE`
- `COOLDOWN_SKIP_REGISTRIES`

File keys:

- `cooldown_minutes`
- `mode`
- `lockfile_policy`
- `now`
- `allowlist_path`
- `ttl_seconds`
- `cache_dir`
- `http_retries`
- `verbose`
- `skip_registries`

`skip_registries` can be written as:

```toml
skip_registries = ["crates-io", "sparse+https://example.com/index/"]
```

`COOLDOWN_SKIP_REGISTRIES` uses a comma-separated list:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io,sparse+https://example.com/index/
```

## `lockfile_policy`

`lockfile_policy` controls whether versions that were already present in the
initial `Cargo.lock` should participate in cooldown:

- `changed` (default): only new or version-changed registry packages are cooled;
- `all`: apply cooldown to every eligible registry package, including versions
  that were already present in the initial lockfile.

Examples:

```toml
lockfile_policy = "all"
```

```bash
COOLDOWN_LOCKFILE_POLICY=all cargo cooldown check
```
