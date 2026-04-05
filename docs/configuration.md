# Configuration

`cargo-cooldown` uses a single configuration file: `cooldown.toml`. Allow
rules are embedded in that same file under the `allow` section.

## Resolution order

Configuration is loaded in this order:

1. environment variables
2. `cooldown.toml` for the active workspace member, when the run targets one
   member unambiguously
3. `cooldown.toml` in the workspace root or crate root
4. `cooldown.toml` in `$CARGO_HOME`

Environment variables always win.

Member overrides apply only when the runtime target is unique, for example:

- `cargo cooldown check --manifest-path member/Cargo.toml`
- `cargo cooldown check --package member`
- running from a member directory without `--workspace`

They do not apply to ambiguous or workspace-wide runs such as:

- `cargo cooldown check --workspace`
- `cargo cooldown check --package a --package b`
- runs that use `--exclude`

## Recommended layout

Single crate:

- `<crate-root>/cooldown.toml`

Workspace:

- `<workspace-root>/cooldown.toml`
- optional `<member-dir>/cooldown.toml` overrides for member-specific runs

The workspace root file should hold the shared policy. Member files should only
contain the values that genuinely differ from the workspace defaults.

## Supported environment variables

- `COOLDOWN_MINUTES`
- `COOLDOWN_MODE`
- `COOLDOWN_LOCKFILE_POLICY`
- `COOLDOWN_NOW`
- `COOLDOWN_TTL_SECONDS`
- `COOLDOWN_CACHE_DIR`
- `COOLDOWN_HTTP_RETRIES`
- `COOLDOWN_VERBOSE`
- `COOLDOWN_SKIP_REGISTRIES`

## Supported `cooldown.toml` keys

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

`skip_registries` can be written as:

```toml
skip_registries = ["crates-io", "sparse+https://example.com/index/"]
```

`COOLDOWN_SKIP_REGISTRIES` uses a comma-separated list:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io,sparse+https://example.com/index/
```

## Allow rules

Allow rules live in the same file:

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

What each rule means:

- `[allow.global]`
  applies one fallback cooldown override to every registry crate
- `[[allow.package]]`
  applies to one crate name per entry, and you can define as many entries as
  needed with different cooldown windows
- `[[allow.exact]]`
  applies to one exact `(crate, version)` pair per entry, and you can define as
  many pairs as needed

`allow.global` and `allow.package` only reduce the effective cooldown window.
They are combined with `cooldown_minutes` by taking the minimum value, so they
cannot make a crate wait longer than the project-wide default.

Examples:

- use `[[allow.exact]]` when only one concrete version should bypass cooldown
- use `[[allow.package]]` when a crate should always use a different cooldown
  window than the project default
- use `minutes = 0` in `[[allow.package]]` to exclude one crate from cooldown
  entirely

Example with multiple package-specific cooldowns:

```toml
cooldown_minutes = 1440

[[allow.package]]
crate = "tokio"
minutes = 60

[[allow.package]]
crate = "ring"
minutes = 10

[[allow.package]]
crate = "openssl"
minutes = 0
```

Merge behavior for workspace root plus member override:

- `allow.global`: the member value replaces the workspace value
- `allow.package`: entries are merged by crate name and the member overrides
  duplicates
- `allow.exact`: entries are unioned and deduplicated

## `lockfile_policy`

`lockfile_policy` controls whether versions that were already present in the
initial `Cargo.lock` should participate in cooldown:

- `changed` (default): only new or version-changed registry packages are cooled
- `all`: apply cooldown to every eligible registry package, including versions
  that were already present in the initial lockfile

Examples:

```toml
lockfile_policy = "all"
```

```bash
COOLDOWN_LOCKFILE_POLICY=all cargo cooldown check
```

`lockfile_policy = "changed"` is most visible with `cargo cooldown update`:

- versions already present in the pre-update `Cargo.lock` are left alone
- versions introduced or changed by that update become eligible for cooldown
- if cooling one of those newly updated versions would require degrading
  baseline-protected or otherwise cooldown-exempt dependencies, cooldown keeps
  the fresh version, warns, and continues best-effort on the rest of the graph

For `build`, `check`, `test`, or `run`, Cargo usually reuses the existing
lockfile. Those commands do not proactively refresh dependencies on their own.

## `cargo cooldown init`

Use `cargo cooldown init` from the project root to scaffold a `cooldown.toml`
interactively.

- in a crate root, it creates one `cooldown.toml`
- in a workspace root, it can create one shared `cooldown.toml` plus optional
  member override files

The command refuses to run from a non-root directory and does not overwrite an
existing `cooldown.toml`.

This command is cargo-cooldown's own setup wizard. It does not forward to
Cargo's `cargo init`. If you want to create a new Cargo package, run plain
`cargo init` first and then run `cargo cooldown init` from the resulting
project root.
