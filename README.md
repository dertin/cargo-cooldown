# cargo-cooldown

`cargo-cooldown` is a lightweight wrapper for Cargo that shields local workspaces from freshly published crates on crates.io. It enforces a configurable cooldown window before new releases can enter your dependency graph, buying time for review and reducing a common supply chain risk. **`cargo-cooldown` is a proof of concept aimed at developer machines.** It is meant as a local utility for workflows where you refresh dependencies and immediately rebuild or run the project, so it shields developers in their own environment. CI pipelines and release automation should continue to run plain Cargo against committed `Cargo.lock` files.

## Why it exists

Attackers can push brand new crates or updates that satisfy permissive semver ranges. Installing them right away means your project might consume malicious code before the ecosystem can react. By delaying adoption, `cargo-cooldown` keeps you on the most recent release that is older than the cooldown window you configure. Socket covers this threat model in their report on [crates.io phishing campaigns](https://socket.dev/blog/crates-io-users-targeted-by-phishing-emails).

## Quick start

1. Install the wrapper next to your toolchain:
   ```bash
   cargo install --locked --path .
   ```
2. Run day-to-day commands like `build`, `check`, `test`, or `run` through `cargo-cooldown`, setting a cooldown in minutes:
   ```bash
   COOLDOWN_MINUTES=1440 cargo-cooldown build
   ```
   Avoid pairing it with `cargo update`: that command is meant to refresh `Cargo.lock`, so running it through the wrapper would undo the cooled down graph.
3. When a dependency is too young, `cargo-cooldown` looks for the newest eligible version, pins it with `cargo update --precise`, re-runs `cargo metadata`, and then retries your command. If no compatible version is old enough, it exits with guidance on how to proceed.

## How it works

1. `cargo-cooldown` ensures a `Cargo.lock` file exists, generating one with `cargo generate-lockfile` if needed.
2. It calls `cargo metadata` to read the full dependency graph and records every `VersionReq` that parents impose on their children.
3. For each crate sourced from a watched registry, it fetches publication metadata from the crates.io HTTP API through a small on-disk cache and computes the package age. Allowlist rules can lower the effective cooldown per crate or globally, but they never raise it above the baseline from `COOLDOWN_MINUTES`.
4. Every crate younger than the effective cooldown enters a queue. The queue gives priority to nodes that might drag others with strict `=` constraints so related packages can be updated together.
5. Candidate versions are filtered so they are not yanked, satisfy every observed semver requirement, are older than the current lockfile entry, and were published before the cutoff timestamp.
6. Each candidate is attempted via `cargo update -p crate@<current_version> --precise <candidate_version>`. If Cargo rejects the change, the blocking crates are added back to the queue unless they are exempt through the allowlist.
7. After a successful downgrade, the tool repeats the cycle until the graph contains only releases older than the cooldown window. When no acceptable candidate exists, the run aborts with a clear error so you can wait, loosen the requirement, or patch it manually.

> Note: today the publication timestamp comes from the crates.io API. Once that data is shipped with the index metadata, those network calls can be replaced with local lookups.

## Configuration

All behavior is driven by environment variables so you can tune it per invocation or in scripts:

- `COOLDOWN_MINUTES` (default `0`): minimum age, in minutes, for a release to be considered safe. The cooldown logic only runs when the value is greater than zero.
- `COOLDOWN_MODE` (default `enforce`): switch to `warn` to log violations without failing, or `off` to skip cooldown logic temporarily.
- `COOLDOWN_ALLOWLIST_PATH`: path to a TOML allowlist that relaxes cooldowns for specific crates or pins exact versions. If unset, the tool looks for `cooldown-allowlist.toml` in the workspace root.
- `COOLDOWN_TTL_SECONDS` (default `86400`): lifetime of cached registry responses.
- `COOLDOWN_CACHE_DIR`: directory used to store cache files. By default the OS cache directory is used with a `cargo-cooldown/` suffix.
- `COOLDOWN_OFFLINE_OK` (default `false`): when true, missing network calls are tolerated and only cached data is used.
- `COOLDOWN_HTTP_RETRIES` (default `2`, max `8`): retry budget for API requests.
- `COOLDOWN_VERBOSE` (default `false`): enable extra tracing output to see resolution decisions.
- `COOLDOWN_REGISTRY_API` (default `https://crates.io/api/v1/`): override the API base if you mirror crates.io.
- `COOLDOWN_REGISTRY_INDEX` (default `registry+https://github.com/rust-lang/crates.io-index, registry+sparse+https://index.crates.io/`): comma separated list of registry sources to guard. Values without the `registry+` prefix are normalized automatically. Dependencies from other registries are left untouched.

## Examples

The `examples/` directory contains material to explore the tool:

- `demo/`: a small workspace with crates.io dependencies you can build with `cargo-cooldown build` to watch downgrades in action.
- `cooldown-allowlist.toml`: sample allowlist showing global and per crate overrides as well as exact exceptions.
- `run.sh`: convenience script with ready made invocations that toggle the most relevant environment variables.

You can try the full flow by running:

1. `cd examples/demo`
2. `COOLDOWN_MINUTES=1440 cargo-cooldown build`
3. Inspect the output, tweak the allowlist or environment variables, and run again to see how the graph changes.

## Good practices

- Start with `COOLDOWN_MODE=warn` so you can review which crates would be downgraded before modifying `Cargo.lock` in a sensitive repository.
- Use an allowlist entry when you must adopt a critical update sooner, and document the reason so it can be revisited later.
- If you depend on alternate registries, make sure `COOLDOWN_REGISTRY_INDEX` explicitly lists each source you want to protect.

`cargo-cooldown` remains a prototype exploring how cooldown periods could fit into Cargo workflows. Feedback and real-world reports are welcome.
