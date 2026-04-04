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

### Registry API routing is discovered automatically

The resolver reads the active registry configuration that Cargo is already using
and discovers the fallback HTTP API from that registry index.

`COOLDOWN_REGISTRY_API` no longer exists.

### Best-effort behavior is controlled by `mode`

`COOLDOWN_OFFLINE_OK` no longer exists.

If you want cooldown failures to be downgraded to warnings, use:

```bash
COOLDOWN_MODE=warn
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
4. Add `skip_registries` or `COOLDOWN_SKIP_REGISTRIES` for any registry that
   should not participate in cooldown.
5. If you relied on best-effort behavior, switch to `COOLDOWN_MODE=warn`.

## Internal registries

Registries that are not skipped must provide enough release-time metadata for
cooldown to decide whether a locked version is fresh.

The resolver uses:

- `pubtime` from Cargo's local registry index cache as the primary source;
- per-crate HTTP fallback only when `pubtime` is missing.

If an internal registry does not provide enough release-time metadata and is not
skipped:

- `enforce` mode fails closed;
- `warn` mode emits a warning and continues;
- `off` mode disables cooldown entirely.

For registries such as CodeArtifact, the practical migration path is:

1. keep crates.io under cooldown;
2. add the internal registry to `skip_registries` if it does not expose enough
   metadata yet.

## Behavior changes to expect

- package-scoped runs now cool only the selected workspace members and their
  dependency closure;
- unchanged lockfile entries are skipped by default unless
  `lockfile_policy = "all"` is enabled;
- `--manifest-path` is honored during both cooldown inspection and
  `cargo update --precise` pinning;
- Cargo-style selectors such as `--manifest-path`, `--package`, `--workspace`,
  `--exclude`, and feature flags are accepted even when passed after the
  forwarded Cargo subcommand.

## Checklist

- no old `COOLDOWN_REGISTRY_*` variables remain in your environment;
- no `COOLDOWN_OFFLINE_OK` references remain in scripts or docs;
- registries that should be excluded are listed in `skip_registries`;
- flows that expect best-effort behavior use `COOLDOWN_MODE=warn`.
