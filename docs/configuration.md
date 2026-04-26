# Configuration

`cargo-cooldown` uses `cooldown.toml`. Allow rules live in the same file under
the `allow` section.

Create one interactively:

```bash
cargo cooldown init
```

## Minimal File

```toml
cooldown_minutes = 1440
mode = "strict"
lockfile_policy = "changed"
```

Meaning:

- `cooldown_minutes`: releases newer than this window are considered fresh
- `mode`: fail or warn when fresh versions remain
- `lockfile_policy`: protect or re-check versions already in `Cargo.lock`

## Resolution Order

Configuration is loaded in this order:

1. environment variables
2. active member `cooldown.toml`, when exactly one workspace member is targeted
3. workspace root or crate root `cooldown.toml`
4. `$CARGO_HOME/cooldown.toml`

Environment variables always win.

Recommended layout:

- single crate: `<crate-root>/cooldown.toml`
- workspace: `<workspace-root>/cooldown.toml`
- optional workspace override: `<member-dir>/cooldown.toml`

Member overrides apply only to unambiguous member runs, such as:

```bash
cargo cooldown check --package member
cargo cooldown check --manifest-path member/Cargo.toml
```

They do not apply to workspace-wide runs such as `--workspace`,
`--package a --package b`, or `--exclude`.

## Supported Keys

`cooldown.toml` supports:

- `cooldown_minutes`
- `mode`
- `lockfile_policy`
- `now`
- `ttl_seconds`
- `cache_dir`
- `http_retries`
- `verbose`
- `skip_registries`
- `allow`

Environment variables:

- `COOLDOWN_MINUTES`
- `COOLDOWN_MODE`
- `COOLDOWN_LOCKFILE_POLICY`
- `COOLDOWN_NOW`
- `COOLDOWN_TTL_SECONDS`
- `COOLDOWN_CACHE_DIR`
- `COOLDOWN_HTTP_RETRIES`
- `COOLDOWN_VERBOSE`
- `COOLDOWN_SKIP_REGISTRIES`

Unknown keys and invalid values fail configuration loading.

Set `verbose = true` or `COOLDOWN_VERBOSE=1` for debug logs. Normal output stays
compact and ends with one Cargo-style summary of lockfile changes.

## `mode`

`mode` controls what happens after cooldown tries to make the graph old enough.

Values:

- `strict` (default): fail and restore the original `Cargo.lock` if any fresh
  version still cannot be replaced with an older version accepted by Cargo
- `best_effort`: keep the best valid lockfile Cargo accepted and warn about
  remaining fresh versions
- `off`: skip cooldown entirely

Example:

```toml
mode = "best_effort"
```

or:

```bash
COOLDOWN_MODE=best_effort cargo cooldown update
```

## `lockfile_policy`

`lockfile_policy` controls which locked versions cooldown is allowed to try to
cool.

Values:

- `changed` (default): protect versions that were already present in the
  initial `Cargo.lock`
- `all`: also check versions that were already present in the initial
  `Cargo.lock`

With `cargo cooldown update`, the initial lockfile means the file that existed
before `cargo update` ran.

Example:

```toml
lockfile_policy = "all"
```

or:

```bash
COOLDOWN_LOCKFILE_POLICY=all cargo cooldown update
```

Important: `lockfile_policy = "all"` is not a force downgrade mode. Cooldown
still asks Cargo to validate the final graph. If Cargo rejects every older
assignment, the fresh version remains unresolved.

## Policy Combinations

Use this table as the main mental model:

| Config | Human meaning |
| --- | --- |
| `lockfile_policy = "changed"` + `mode = "strict"` | Default. Keep the pre-run lockfile as the floor. Cool only versions added or changed by this run. Fail if any new fresh version remains. |
| `lockfile_policy = "changed"` + `mode = "best_effort"` | Same lockfile protection as the default, but keep the best valid result and warn if some fresh versions remain. |
| `lockfile_policy = "all"` + `mode = "strict"` | Try to cool every eligible locked registry package, including packages already in `Cargo.lock`. Fail if any fresh version still cannot be cooled. |
| `lockfile_policy = "all"` + `mode = "best_effort"` | Try to cool every eligible locked registry package, keep Cargo's best valid result, and warn about the remaining fresh versions. |

Why can a fresh version remain?

- the current `Cargo.toml` requires a fresh version range
- a transitive crate uses an exact dependency
- features or target-specific dependencies activate a newer package
- a group of crates has no older combination that Cargo accepts
- an allow rule or skipped registry exempts the package
- `lockfile_policy = "changed"` protects the pre-run lockfile floor

## Allow Rules

Allow rules reduce the cooldown window for selected crates.

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

Rules:

- `[[allow.exact]]`: allow one exact `(crate, version)` pair
- `[[allow.package]]`: use a shorter cooldown for one crate name
- `minutes = 0`: exclude that crate from cooldown
- `[allow.global]`: define a shorter default cooldown for all registry crates

`allow.global` and `allow.package` only reduce the effective cooldown. They do
not increase it above `cooldown_minutes`.

Workspace merge behavior:

- `allow.global`: member value replaces workspace value
- `allow.package`: member entries override workspace entries with the same crate
- `allow.exact`: member and workspace entries are unioned and deduplicated

## `skip_registries`

Skip a registry by logical name or effective URL:

```toml
skip_registries = ["crates-io", "sparse+https://example.com/index/"]
```

Environment variable form:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io,sparse+https://example.com/index/
```

Skipped registries are not inspected, fetched through fallback HTTP, or
downgraded. Their packages still participate in Cargo's resolver constraints.

## `cargo cooldown init`

Use `cargo cooldown init` from the project root to create `cooldown.toml`
interactively.

- in a crate root, it creates one `cooldown.toml`
- in a workspace root, it can create one shared file plus optional member
  override files
- it refuses to overwrite existing `cooldown.toml` files

This is cargo-cooldown's setup wizard. It does not forward to Cargo's
`cargo init`.
