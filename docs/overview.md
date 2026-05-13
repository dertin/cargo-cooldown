# Overview

`cargo-cooldown` wraps Cargo and keeps very new registry releases out of the
resolved dependency graph until they are old enough for your configured policy.
It is meant to reduce exposure to supply-chain attacks that target freshly
published crate versions.

It does not replace Cargo's resolver. Cargo still decides which dependency
graphs are valid.

## What Happens

`cargo cooldown update` and `cargo cooldown check|build|test|run` protect
different moments in the dependency lifecycle.

- `cargo cooldown update` protects lockfile refreshes. It lets Cargo compute an
  updated graph, then cools that updated lockfile before publishing it.
- `cargo cooldown check|build|test|run` protect lockfile consumption. They run a
  pre-command lockfile pass before Cargo downloads, compiles, tests, or runs code
  from the resolved dependency graph.

This matters for supply-chain risk: freshly published crate source is not
downloaded or compiled just because a lockfile exists. The higher-risk moment is
when a later Cargo command consumes that lockfile and needs crate contents,
especially commands that compile dependencies or execute build scripts.

With the default `[cooldown].lockfile-baseline = "floor"`, guard-style commands treat
versions already present in the initial `Cargo.lock` as the protected baseline.
Use `[cooldown].lockfile-baseline = "ignore"` when `cargo cooldown check`,
`build`, `test`, or `run` should also try to cool already-locked versions before
Cargo consumes them.

For guard-style commands:

1. copy the workspace to a temporary directory when cooldown is enabled
2. hold the real root `Cargo.lock` with a backup plus sentinel
3. snapshot the temp copy of the current `Cargo.lock`
4. read Cargo metadata in the temp workspace
5. inspect reachable registry packages
6. replace fresh versions with older compatible versions when possible
7. ask Cargo to validate the resulting lockfile
8. publish the final temp `Cargo.lock` back to the real workspace
9. run the requested Cargo command when the graph is acceptable

The practical effect by command is:

| Command | What cooldown adds |
| --- | --- |
| `cargo cooldown check` | Runs the pre-command guard before Cargo downloads missing dependency sources and performs check-mode compilation. |
| `cargo cooldown build` | Runs the pre-command guard before Cargo compiles dependencies and runs dependency build scripts. |
| `cargo cooldown test` | Runs the pre-command guard before Cargo compiles test/dev-dependency graphs and runs tests. |
| `cargo cooldown run` | Runs the pre-command guard before Cargo builds and then executes the selected binary. |
| `cargo cooldown update` | Refreshes the lockfile in an isolated workspace, then cools the updated graph before publishing the final `Cargo.lock`. |

The current implementation applies the same pre-command guard to any forwarded
Cargo subcommand except `update` when cooldown is enabled. The documented
supply-chain workflows are `check`, `build`, `test`, `run`, and `update`; use
plain Cargo for commands that should not be preceded by a lockfile cooldown
pass.

For `cargo cooldown update`:

1. copy the workspace to a temporary directory
2. hold the real root `Cargo.lock` with a backup plus sentinel
3. snapshot the temp copy of the current `Cargo.lock`
4. run `cargo update` in the temp workspace
5. cool the updated temp lockfile
6. publish the final temp `Cargo.lock` back to the real workspace
7. restore the original lockfile if `incompatible-publish-age = "deny"` fails

## Important Ideas

- `[registry].global-min-publish-age` defines what "fresh" means.
- `[cooldown].incompatible-publish-age` decides whether remaining fresh versions
  are an error or a warning.
- `[cooldown].lockfile-baseline` decides whether versions already present in the
  initial `Cargo.lock` are protected.
- `skip_registries` excludes whole registries from cooldown processing.
- Allow rules intentionally reduce the cooldown window for selected crates.

## Generated Defaults

```toml
[cooldown]
incompatible-publish-age = "deny"
lockfile-baseline = "floor"

[registry]
global-min-publish-age = "14 days"
```

In human terms:

- protect the versions that were already locked before the command started
- cool versions that Cargo added or changed
- fail closed and restore the original lockfile if Cargo still needs a fresh
  version

Use `[cooldown].lockfile-baseline = "ignore"` when you also want to try cooling
versions that were already locked before the command started.

Use `[cooldown].incompatible-publish-age = "fallback"` when long
min-publish-age windows should cool what Cargo accepts but keep the best
Cargo-valid lockfile if the graph still needs fresh versions.

Use `[cooldown].fallback-accept = "auto"` only for workflows that should keep the
fallback result without asking.

## Why Fresh Versions Can Remain

A fresh version can remain even with `[cooldown].lockfile-baseline = "ignore"`
because Cargo may not accept any older graph. Common causes:

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
