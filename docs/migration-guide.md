# Migration Guide

This guide covers the upgrade path from the older configuration model to the
current registry-aware resolver.

## What changed

### Registry scoping is now opt-out

The resolver now applies cooldown to every Cargo registry that appears in the
resolved graph unless you explicitly skip it.

Previously, registry processing could be limited with `COOLDOWN_REGISTRY_INDEX`.
That variable no longer exists.

Use:

```toml
skip_registries = ["crates-io", "sparse+https://example.com/index/"]
```

or:

```bash
COOLDOWN_SKIP_REGISTRIES=crates-io,sparse+https://example.com/index/
```

### Lockfile baseline is now respected by default

By default, versions that were already present in the initial `Cargo.lock` are
not re-cooled.

If you want the previous "cool every eligible locked package" behavior, use:

```toml
lockfile_policy = "all"
```

or:

```bash
COOLDOWN_LOCKFILE_POLICY=all
```

### Allow rules are embedded in `cooldown.toml`

`cooldown-allowlist.toml`, `allowlist_path`, and `COOLDOWN_ALLOWLIST_PATH` no
longer exist.

Move allow rules into `cooldown.toml`:

```toml
[[allow.exact]]
crate = "serde"
version = "1.0.218"

[[allow.package]]
crate = "tokio"
minutes = 60

[allow.global]
minutes = 1440
```

If an existing rule uses `minimum_release_age`, rename that key to `minutes`.
Use lowercase TOML keys such as `cooldown_minutes`; environment variable names
are only accepted from the environment.

For workspaces, put shared rules in the workspace root file and use
member-local `cooldown.toml` overrides only for uniquely targeted member runs.

### Registry API routing is discovered automatically

The resolver reads the active registry configuration that Cargo is already using
and discovers the fallback HTTP API from that registry index.

`COOLDOWN_REGISTRY_API` no longer exists.

### Best-effort behavior is controlled by `mode`

`COOLDOWN_OFFLINE_OK` no longer exists.

`mode` now accepts only:

- `strict`
- `best_effort`
- `off`

If you want cooldown failures to be downgraded to warnings and keep a
best-effort lockfile, use:

```bash
COOLDOWN_MODE=best_effort
```

If you want a registry to be excluded from cooldown entirely, use
`skip_registries`.

## Required migration steps

1. Remove `COOLDOWN_REGISTRY_API` from your environment, shell wrappers, and CI
   helpers.
2. Remove `COOLDOWN_REGISTRY_INDEX` from your environment and `cooldown.toml`
   files.
3. Remove `COOLDOWN_OFFLINE_OK` from your environment and `cooldown.toml`
   files.
4. Remove `cooldown-allowlist.toml`, `allowlist_path`, and
   `COOLDOWN_ALLOWLIST_PATH`.
5. Move allow rules into `cooldown.toml`.
6. Add `skip_registries` or `COOLDOWN_SKIP_REGISTRIES` for any registry that
   should not participate in cooldown.
7. If you relied on best-effort behavior, switch to `COOLDOWN_MODE=best_effort`.

## Internal registries

Registries that participate in cooldown must expose enough release-time
metadata for the resolver to decide whether a locked version is fresh.

The resolver uses Cargo's local index `pubtime` first and falls back to
per-crate HTTP only when `pubtime` is missing.

If an internal registry still cannot provide timestamps:

- `strict` mode fails closed;
- `best_effort` mode emits a warning and continues;
- `off` mode disables cooldown entirely.

For registries such as CodeArtifact, the practical migration path is usually:

1. keep crates.io under cooldown;
2. put the internal registry in `skip_registries` until it exposes compatible
   metadata.

See also:

- [Registries](registries.md)
- [Troubleshooting](troubleshooting.md)

## Behavior changes to expect

- package-scoped runs now cool only the selected workspace members and their
  dependency closure;
- unchanged lockfile entries are skipped by default unless
  `lockfile_policy = "all"` is enabled;
- configuration discovery now starts from the effective Cargo root instead of
  implicitly following the current directory;
- allow rules now live inside `cooldown.toml`;
- `--manifest-path` is honored during cooldown inspection, batch validation,
  and the forwarded Cargo command;
- Cargo-style selectors such as `--manifest-path`, `--package`, `--workspace`,
  `--exclude`, and feature flags are accepted even when passed after the
  forwarded Cargo subcommand.

## Checklist

- no old `COOLDOWN_REGISTRY_*` variables remain in your environment;
- no `COOLDOWN_OFFLINE_OK` references remain in scripts or docs;
- no `cooldown-allowlist.toml`, `allowlist_path`, or
  `COOLDOWN_ALLOWLIST_PATH` references remain;
- registries that should be excluded are listed in `skip_registries`;
- flows that expect best-effort behavior use `COOLDOWN_MODE=best_effort`.
