# Overview

`cargo-cooldown` guards a Cargo workflow by delaying adoption of freshly
published registry releases.

The high-level flow is:

1. Ensure a lockfile exists.
2. Read `cargo metadata` and the resolved dependency graph.
3. Inspect registry packages that are subject to cooldown.
4. For each fresh package, pick the newest older compatible release.
5. Apply `cargo update --precise` until the graph reaches a cooled state.
6. Run the requested Cargo command.

The resolver aligns the registry view with Cargo itself:

- local registry index cache is the primary source of release time;
- fallback HTTP is per registry and per crate;
- registries can be skipped explicitly;
- packages from skipped registries are never cooled down.
