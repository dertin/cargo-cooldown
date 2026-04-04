# Overview

`cargo-cooldown` guards a Cargo workflow by preventing freshly published
registry releases from entering the resolved graph until they are old enough.

At a high level:

1. snapshot the initial `Cargo.lock`;
2. read Cargo's current resolved graph;
3. inspect reachable registry packages that are not skipped or allowlisted;
4. by default, apply cooldown only to new or version-changed lockfile entries;
5. try older compatible versions with `cargo update --precise`;
6. restore the original `Cargo.lock` if the cooldown run fails after Cargo has
   already rewritten it;
7. run the requested Cargo command once the graph is acceptable.

Key behavior:

- Cargo's own registry configuration and resolved graph are the source of truth;
- the local registry index cache is used first, with per-crate HTTP fallback
  only when `pubtime` is missing;
- unchanged lockfile entries are skipped by default;
- `lockfile_policy = "all"` (or `COOLDOWN_LOCKFILE_POLICY=all`) restores the
  previous "cool every eligible locked package" behavior;
- packages from `skip_registries` never participate in cooldown.

More detailed reference docs:

- [Configuration](configuration.md)
- [Registries](registries.md)
- [How Resolution Works Today](resolution-flow.md)
- [Migration Guide](migration-guide.md)
- [Troubleshooting](troubleshooting.md)
