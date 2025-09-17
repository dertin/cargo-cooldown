# cargo-cooldown

`cargo-cooldown` is a thin wrapper around Cargo that shields local workspaces from freshly published crates—the releases most likely to carry malware before the community spots them or the registry can yank them. It enforces a configurable cooldown window before new releases can be resolved, buying time to review them and reducing a common supply-chain attack vector. This repository is a proof of concept that explores how a cooldown guard could integrate with Cargo to improve developer security on local machines. It intentionally targets day-to-day development workflows—CI and release automation should continue relying on committed `Cargo.lock` files instead of this guard.

## Why it exists

Attackers can publish brand-new crates or update existing ones so that they satisfy popular semver ranges. Maintainers who update dependencies immediately risk integrating malicious code before the community has inspected it. `cargo-cooldown` delays adoption of those releases by pinning the newest *already vetted* version that is older than your required cooldown window. The idea is explored in [Socket's report on crates.io phishing campaigns](https://socket.dev/blog/crates-io-users-targeted-by-phishing-emails).

## Quick start

1. Install the wrapper next to your local toolchain:
   ```bash
   cargo install --locked --path .
   ```
2. Run your usual Cargo command through the guard. For example, to build a workspace while requiring a 24 hour buffer:
   ```bash
   COOLDOWN_MINUTES=1440 cargo-cooldown build
   ```
3. When a dependency version is too new, the wrapper resolves the latest eligible release and continues your command without modifying `Cargo.toml`.

## Behaviour notes

- When a dependency is too fresh, the guard walks backward through the dependency graph looking for the most recent release that still satisfies the semver requirements declared in your manifests. If something is pinned with an exact (`=`) constraint, the parent and any siblings are downgraded together so the family stays consistent.
- By default the guard watches both the crates.io git index (`registry+https://github.com/rust-lang/crates.io-index`) and its sparse mirror (`registry+sparse+https://index.crates.io/`). Alternate registries—such as those declared under `[registries]` in `.cargo/config.toml`—remain untouched unless you add them via `COOLDOWN_REGISTRY_INDEX`.

### How the resolver keeps the newest compatible versions

1. The guard runs `cargo metadata` to inspect the fully resolved graph, recording the exact `VersionReq` constraints that parents impose on their children. For each `PackageId` it then queries the crates.io HTTP API (with a lightweight on-disk cache) to fetch the publication timestamp so it can compute how many minutes old the currently locked release really is.
2. Any package whose current release is younger than the effective cooldown window (global default plus allowlist overrides) is marked as "fresh" and added to a queue. The queue is ordered so that packages with fresh dependents are handled first.
3. For each fresh package, the guard lists **all** published versions and filters them down to candidates that:
   - are not yanked;
   - satisfy every semver constraint observed in step 1;
   - are strictly older than the version currently in the graph;
   - satisfy the cooldown requirement (published before the cutoff timestamp).
4. If no candidates remain, the guard looks at the parents that imposed the blocking requirements, enqueues those parents, and retries. Only when **every** parent still fails to yield a compatible, sufficiently old version does the run abort with an actionable error.
5. When a candidate is available, the guard pins it via `cargo update -p crate@current --precise <candidate>`. Using the `crate@current` syntax ensures the precise instance is updated, even if multiple versions of the crate appear in the graph.
6. After each successful pin the guard re-runs `cargo metadata` and repeats the process until the entire graph contains only releases older than the cooldown window. The result is a `Cargo.lock` file that matches what you would have obtained by running `cargo update` at an earlier point in time, without ever touching your `Cargo.toml`.

> _Note:_ Today the guard relies on crates.io’s API to retrieve `created_at` timestamps. When the registry metadata shipped with Cargo exposes this information directly, the HTTP round-trip can be replaced with a local lookup for even faster runs.

## Configuration

`cargo-cooldown` is controlled through environment variables so you can adjust behaviour per command or via shells scripts.

- `COOLDOWN_MINUTES` (default: `0`): Minimum age for a crate release before it is considered safe.
- `COOLDOWN_MODE` (default: `enforce`): Set to `warn` to log violations without blocking, or `off` to bypass the guard temporarily.
- `COOLDOWN_ALLOWLIST_PATH`: Path to a TOML file that relaxes the cooldown for specific crates or versions. See `examples/cooldown-allowlist.toml` for the schema.
- `COOLDOWN_TTL_SECONDS` (default: `86400`): Cache lifetime for registry metadata.
- `COOLDOWN_CACHE_DIR`: Directory used to persist metadata across runs (falls back to the OS cache directory).
- `COOLDOWN_OFFLINE_OK` (default: `false`): Allow the guard to run using only cached data.
- `COOLDOWN_HTTP_RETRIES` (default: `2`, max: `8`): Retry budget for registry API calls.
- `COOLDOWN_VERBOSE` (default: `false`): Emit additional tracing to understand resolution decisions.
- `COOLDOWN_REGISTRY_API` (default: `https://crates.io/api/v1/`): Override the API endpoint if you mirror crates.io.
- `COOLDOWN_REGISTRY_INDEX` (default: both `registry+https://github.com/rust-lang/crates.io-index` and `registry+sparse+https://index.crates.io/`): Change which registries are guarded. Provide a comma-separated list to allow multiple entries. Values without the `registry+` prefix are normalised automatically.

## Examples and experimentation

The `examples/` directory contains helper material while you experiment locally:

- `demo/`: Small Rust project with crates.io dependencies that you can compile with `cargo-cooldown build` to watch the guard in action.
- `cooldown-allowlist.toml`: Demonstrates how to whitelist crates or versions when you cannot wait for the full cooldown.
- `run.sh`: Convenience script with sample invocations covering the most relevant environment variables.

Try the guard inside the bundled workspace:

1. `cd examples/demo`
2. `COOLDOWN_MINUTES=60 cargo-cooldown build`
3. Inspect the output and tweak environment variables or the sample allowlist to explore different behaviours.

To integrate the allowlist into another project, copy `examples/cooldown-allowlist.toml` next to that workspace's `Cargo.lock` and edit as needed.
