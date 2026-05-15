# Migration Guide

This guide covers the upgrade path from the 0.2.x configuration model to the
current registry-aware resolver.

## What changed

### 0.3.1 adds RFC-style min publish age config

The canonical 0.3.1+ global cooldown policy is:

```toml
[registry]
global-min-publish-age = "14 days"
```

Per-registry policy is now available in `cooldown.toml`:

```toml
[registries.internal]
index = "sparse+https://example.com/index/"
min-publish-age = "0"
```

cargo-cooldown still does not read `.cargo/config.toml` for policy values; it
only uses Cargo registry config to resolve actual registry names and indexes.

### 0.3.0 compatibility aliases

The 0.3.0 keys and variables still work in 0.3.1, but are deprecated and are
planned for removal in the next major 0.4.x line:

- `cooldown_minutes` -> `[registry].global-min-publish-age`
- `COOLDOWN_MINUTES` -> `CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE`
- root `enforcement = "strict"` -> `[cooldown].incompatible-publish-age = "deny"`
- root `enforcement = "off"` -> `[cooldown].incompatible-publish-age = "allow"`
- root `enforcement = "cargo_compatible"` -> `[cooldown].incompatible-publish-age = "fallback"`
- `COOLDOWN_ENFORCEMENT=strict|off|cargo_compatible` ->
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=deny|allow|fallback`
- root `cargo_compatible_accept` -> `[cooldown].fallback-accept`
- `COOLDOWN_CARGO_COMPATIBLE_ACCEPT` -> `COOLDOWN_FALLBACK_ACCEPT`
- root `lockfile_baseline` -> `[cooldown].lockfile-baseline`

If both the 0.3.0 form and the 0.3.1 form are present in the same layer with
different effective values, config loading fails. New configs and scripts
should use only the 0.3.1 names.

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
[cooldown]
lockfile-baseline = "ignore"
```

or:

```bash
COOLDOWN_LOCKFILE_BASELINE=ignore
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
min-publish-age = "1 hour"

[allow.global]
minutes = 1440
```

If an existing rule uses `minimum_release_age`, rename that key to
`min-publish-age`. The older `minutes` form still works for compatibility, but
new `allow.package` rules should use duration strings such as
`min-publish-age = "1 hour"` or `min-publish-age = "0"`.

Use lowercase or dashed TOML keys such as
`[registry].global-min-publish-age`; environment variable names are only
accepted from the environment.

For workspaces, put shared rules in the workspace root file and use
member-local `cooldown.toml` overrides only for uniquely targeted member runs.

### Registry API routing is discovered automatically

The resolver reads the active registry configuration that Cargo is already using
and discovers the fallback HTTP API from that registry index.

`COOLDOWN_REGISTRY_API` no longer exists.

### `[cooldown].incompatible-publish-age` replaces 0.2.x `mode`

`COOLDOWN_OFFLINE_OK` no longer exists.

0.2.x used:

- `mode = "strict"` / `COOLDOWN_MODE=strict`
- `mode = "best_effort"` / `COOLDOWN_MODE=best_effort`
- `mode = "off"` / `COOLDOWN_MODE=off`

This release uses:

- `[cooldown].incompatible-publish-age = "deny"` /
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=deny`
- `[cooldown].incompatible-publish-age = "fallback"` /
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback`
- `[cooldown].incompatible-publish-age = "allow"` /
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=allow`

If you previously used 0.2.x best-effort behavior and want to keep Cargo's best
valid lockfile while warning about fresh versions that Cargo still requires,
use:

```bash
COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback
```

This now prompts before accepting unresolved fresh versions. To keep the 0.2.x
non-interactive behavior, also set:

```bash
COOLDOWN_FALLBACK_ACCEPT=auto
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
7. Replace `mode` with `[cooldown].incompatible-publish-age` in
   `cooldown.toml`; rename `strict` to `deny`, `off` to `allow`, and
   `best_effort` to `fallback`.
8. Replace `COOLDOWN_MODE` with `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE` in scripts
   and CI.
9. If you relied on 0.2.x best-effort behavior, switch to
   `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback`.
10. If that flow must remain non-interactive, add
    `COOLDOWN_FALLBACK_ACCEPT=auto`.

## Internal registries

Registries that participate in cooldown must expose enough release-time
metadata for the resolver to decide whether a locked version is fresh.

The resolver uses Cargo's local index `pubtime` first and falls back to
per-crate HTTP only when `pubtime` is missing.

If an internal registry still cannot provide timestamps:

- `incompatible-publish-age = "deny"` fails closed;
- `incompatible-publish-age = "fallback"` emits a warning and
  continues;
- `incompatible-publish-age = "allow"` disables cooldown entirely.

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
  `[cooldown].lockfile-baseline = "ignore"` is enabled;
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
- 0.2.x `mode` keys were renamed to
  `[cooldown].incompatible-publish-age`;
- 0.2.x `COOLDOWN_MODE` references were renamed to
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE`;
- flows that expect 0.2.x best-effort behavior use
  `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback`;
- non-interactive flows that expect 0.2.x best-effort behavior also use
  `COOLDOWN_FALLBACK_ACCEPT=auto`.
