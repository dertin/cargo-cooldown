# Overview

`cargo-cooldown` guards a Cargo workflow by delaying adoption of freshly
published registry releases.

The high-level flow is:

1. Snapshot the initial `Cargo.lock` baseline.
2. Ensure a lockfile exists.
3. Read `cargo metadata` and the resolved dependency graph.
4. Inspect registry packages that are subject to cooldown.
5. By default, cool only packages whose current `(registry, crate, version)`
   was not already present in the initial lockfile baseline.
6. For each fresh package that still participates in cooldown, pick the newest
   older compatible release.
7. Apply `cargo update --precise` until the graph reaches a cooled state.
8. Run the requested Cargo command.

The resolver aligns the registry view with Cargo itself:

- local registry index cache is the primary source of release time;
- fallback HTTP is per registry and per crate;
- registries can be skipped explicitly;
- unchanged lockfile entries are skipped by default;
- `lockfile_policy = "all"` (or `COOLDOWN_LOCKFILE_POLICY=all`) restores the
  previous "cool every eligible locked package" behavior;
- packages from skipped registries are never cooled down.
