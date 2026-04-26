# Overview

`cargo-cooldown` wraps Cargo and keeps very new registry releases out of the
resolved dependency graph until they are old enough for your configured policy.
It is meant to reduce exposure to supply-chain attacks that target freshly
published crate versions.

It does not replace Cargo's resolver. Cargo still decides which dependency
graphs are valid.

## What Happens

For `cargo cooldown check`, `build`, `test`, or `run`:

1. copy the workspace to a temporary directory when cooldown is enabled
2. hold the real root `Cargo.lock` with a backup plus sentinel
3. snapshot the temp copy of the current `Cargo.lock`
4. read Cargo metadata in the temp workspace
5. inspect reachable registry packages
6. replace fresh versions with older compatible versions when possible
7. ask Cargo to validate the resulting lockfile
8. publish the final temp `Cargo.lock` back to the real workspace
9. run the requested Cargo command when the graph is acceptable

For `cargo cooldown update`:

1. copy the workspace to a temporary directory
2. hold the real root `Cargo.lock` with a backup plus sentinel
3. snapshot the temp copy of the current `Cargo.lock`
4. run `cargo update` in the temp workspace
5. cool the updated temp lockfile
6. publish the final temp `Cargo.lock` back to the real workspace
7. restore the original lockfile if `strict` enforcement fails

## Important Ideas

- `cooldown_minutes` defines what "fresh" means.
- `enforcement` decides whether remaining fresh versions are an error or a
  warning.
- `lockfile_baseline` decides whether versions already present in the initial
  `Cargo.lock` are protected.
- `skip_registries` excludes whole registries from cooldown processing.
- Allow rules intentionally reduce the cooldown window for selected crates.

## Generated Defaults

```toml
enforcement = "cargo_compatible"
cargo_compatible_accept = "prompt"
lockfile_baseline = "floor"
```

In human terms:

- protect the versions that were already locked before the command started
- cool versions that Cargo added or changed
- ask before keeping the best Cargo-valid lockfile if the updated graph still
  needs a fresh version

Use `lockfile_baseline = "ignore"` when you also want to try cooling versions that
were already locked before the command started.

Use `enforcement = "strict"` when unresolved fresh versions should fail closed
and restore the original `Cargo.lock`.

Use `cargo_compatible_accept = "auto"` only for workflows that should keep the
Cargo-compatible result without asking.

## Why Fresh Versions Can Remain

A fresh version can remain even with `lockfile_baseline = "ignore"` because Cargo may
not accept any older graph. Common causes:

- the current manifests require a fresh version range
- a transitive crate uses an exact version dependency
- enabled features or targets activate a newer dependency path
- a coupled crate family has no older compatible combination

See [Troubleshooting](troubleshooting.md) for diagnosis commands.

## More Docs

- [Configuration](configuration.md)
- [Troubleshooting](troubleshooting.md)
- [Registries](registries.md)
- [Resolution Flow](resolution-flow.md)
- [Migration Guide](migration-guide.md)
