# Troubleshooting

## `incompatible-publish-age = "deny"` Blocked Fresh Versions

Meaning:

- the listed versions are still newer than the configured cooldown window
- cooldown tried older versions that Cargo would accept
- Cargo still required those fresh versions
- the deny policy restored the original `Cargo.lock`

This is different from `[cooldown].lockfile-baseline`.

- `lockfile-baseline = "ignore"` lets cooldown try to downgrade packages that
  were already in the initial lockfile.
- `incompatible-publish-age = "deny"` still fails if any fresh version remains
  after those attempts.

Options:

- use `incompatible-publish-age = "fallback"` to keep Cargo's best
  valid lockfile and review the fresh versions in an interactive prompt
- add `allow.package` or `allow.exact` for releases you intentionally accept
- reduce `[registry].global-min-publish-age` if the window is too strict
- inspect the dependency path that forces the fresh package

Useful commands:

```bash
cargo tree -i icu_normalizer
cargo tree -i rustls
cargo tree -i getrandom
```

To test a specific downgrade in a disposable copy:

```bash
cargo update -p icu_normalizer --precise 2.0.0
```

Cargo's error usually names the manifest or transitive dependency that prevents
the downgrade.

## A Fresh Version Remains With `lockfile-baseline = "ignore"`

`lockfile-baseline = "ignore"` removes the initial lockfile protection. It does
not override Cargo's resolver.

A fresh version can remain when:

- the current `Cargo.toml` requires a fresh semver range
- a transitive crate depends on an exact fresh version
- enabled features or target-specific dependencies require a newer package
- several crates must move together, but no older compatible set exists
- the package is covered by an allow rule
- the package comes from a skipped registry

## A Package Did Not Downgrade

Possible reasons:

- no older compatible release exists before the cutoff
- Cargo rejected every older candidate
- an allow rule applies
- the package comes from a skipped registry
- `lockfile-baseline = "floor"` protects the version from the initial lockfile

## "missing release timestamp"

Meaning:

- the package comes from a registry that participates in cooldown
- the local index did not provide `pubtime`
- fallback HTTP did not provide a usable timestamp either

Options:

- add the registry to `skip_registries`
- use `incompatible-publish-age = "fallback"` if warnings are
  acceptable
- set `fallback-accept = "auto"` only if unresolved fresh versions
  should be accepted without an interactive prompt
- ensure the registry exposes either `pubtime` or a compatible API

## "registry ... does not provide cached metadata"

The local Cargo registry cache does not contain the crate entry and fallback
could not supply it. This usually means the registry was never fetched locally
or does not expose fallback metadata in a compatible way.

## A Skipped Registry Still Affects The Resolver

This is expected. `skip_registries` prevents cooldown processing for that
registry, but its packages still shape Cargo's dependency graph.

## A Member Config Was Ignored

Per-member `cooldown.toml` overrides apply only when the run targets exactly one
workspace member.

They do not apply to:

- `--workspace`
- multiple `--package` values
- `--exclude`
- other ambiguous workspace selections
