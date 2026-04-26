# Overview

`cargo-cooldown` guards a Cargo workflow by preventing freshly published
registry releases from entering the resolved graph until they are old enough.

At a high level:

1. snapshot the initial `Cargo.lock`;
2. read Cargo's current resolved graph;
3. inspect reachable registry packages that are not skipped or covered by allow rules;
4. by default, apply cooldown only to new or version-changed lockfile entries;
5. solve older compatible versions in a verified lockfile batch;
6. restore the original `Cargo.lock` if the cooldown run fails after Cargo has
   already rewritten it;
7. run the requested Cargo command once the graph is acceptable.

When the requested command is `cargo cooldown update`, the wrapper snapshots the
existing lockfile first, runs `cargo update`, and then applies cooldown against
that pre-update baseline.

Key behavior:

- Cargo's own registry configuration and resolved graph are the source of truth;
- the local registry index cache is used first, with per-crate HTTP fallback
  only when `pubtime` is missing;
- unchanged lockfile entries are skipped by default;
- `cargo cooldown update` protects pre-update lockfile versions by default, and
  `lockfile_policy = "all"` opts into checking those versions too;
- `lockfile_policy = "all"` (or `COOLDOWN_LOCKFILE_POLICY=all`) checks every
  eligible locked package, including unchanged lockfile entries;
- `mode = "strict"` is fail-closed and restores the original lockfile if any
  resolver-constrained fresh versions remain;
- `mode = "best_effort"` keeps the best lockfile it could produce and warns
  about any remaining resolver-constrained fresh versions;
- configuration lives in one `cooldown.toml`, including allow rules;
- workspaces use the workspace root config by default, with optional member
  overrides only for uniquely targeted members;
- `cargo cooldown init` scaffolds the recommended layout interactively;
- packages from `skip_registries` never participate in cooldown.

More detailed reference docs:

- [Configuration](configuration.md)
- [Registries](registries.md)
- [How Resolution Works Today](resolution-flow.md)
- [Migration Guide](migration-guide.md)
- [Troubleshooting](troubleshooting.md)
