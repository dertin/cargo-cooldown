# cargo-cooldown

`cargo-cooldown` is a lightweight wrapper for Cargo that shields local workspaces from freshly published crates on crates.io. It enforces a configurable cooldown window before new releases can enter your dependency graph, buying time for review and reducing a common supply chain risk. **`cargo-cooldown` is a proof of concept for local development.** Use it in day-to-day workflows where you refresh dependencies and immediately rebuild or run the project. CI pipelines and release automation should continue to run plain Cargo against committed `Cargo.lock` files.

## Why it exists

Attackers can push brand new crates or updates that satisfy permissive semver ranges. Installing them right away means your project might consume malicious code before the ecosystem can react. By delaying adoption, `cargo-cooldown` keeps you on the most recent release that is older than the cooldown window you configure. Socket covers this threat model in their report on [crates.io phishing campaigns](https://socket.dev/blog/crates-io-users-targeted-by-phishing-emails).

## Quick start

1. Install:
   ```bash
   cargo install --locked cargo-cooldown
   ```
2. Explore the CLI and the flags that mirror Cargo’s selectors:
   ```bash
   cargo cooldown --help
   ```
3. Expect the tool to pin your graph if a dependency is too fresh. It will search for the newest compliant version, run `cargo update --precise`, refresh metadata, and then re-invoke your command. Tune the behaviour via environment variables like `COOLDOWN_MINUTES` or with a `cooldown.toml` file; see the configuration section for the full reference.

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
- `COOLDOWN_REGISTRY_INDEX` (default `registry+https://github.com/rust-lang/crates.io-index, registry+sparse+https://index.crates.io/`): comma separated list of registry sources. Values without the `registry+` prefix are normalized automatically. Dependencies from other registries are left untouched.

For repeatable settings you can also create a `cooldown.toml` file. Place it in the workspace root to scope it to a project, or in `~/.cargo/cooldown.toml` to apply it globally. Following the convention used by Cargo configuration, keys should be written in `snake_case`; uppercase keys mirroring the environment variables remain supported for compatibility. Environment variables always win over file values, so scripts can override temporary tweaks without editing the config. Paths such as `allowlist_path` or `cache_dir` can be expressed relative to the file location.

```toml
cooldown_minutes = 1440
mode = "warn"
offline_ok = true
registry_index = "https://mirror.example/index"
```

The demo workspace under `examples/demo/` ships with a baseline `cooldown.toml`; the helper script `examples/test.sh` layers environment variables on top for each scenario, illustrating the precedence in practice.

## CLI flags

`cargo-cooldown` uses [`clap`](https://docs.rs/clap/latest/clap/) together with [`clap-cargo`](https://docs.rs/clap-cargo/latest/clap_cargo/) so you can reuse familiar Cargo selectors before passing control to the underlying command. Flags such as `--manifest-path`, `--package`, `--workspace`, `--exclude`, `--features`, `--all-features`, and `--no-default-features` are parsed locally and then forwarded to the Cargo invocation. Everything after the first positional argument is treated as the command to execute.

```bash
cargo cooldown --manifest-path examples/demo/Cargo.toml --package demo build
cargo cooldown --features "demo,extra" test -- --nocapture
```

## Examples

The `examples/` directory contains material to explore the tool:

- `demo/`: a small workspace with crates.io dependencies you can build with `cargo cooldown build` to watch downgrades in action.
- `cooldown-allowlist.toml`: sample allowlist showing global and per crate overrides as well as exact exceptions.
- `test.sh`: convenience script with ready made invocations that toggle the most relevant environment variables.

You can try the full flow by running:

1. `cd examples/demo`
2. `COOLDOWN_MINUTES=1440 cargo cooldown build`
3. Inspect the output, tweak the allowlist or environment variables, and run again to see how the graph changes.

## Feedback

`cargo-cooldown` is still an experiment, and we’re learning alongside you. Tried it out? Let us know what feels helpful, what gets in the way. Real-world stories and issue reports make the project better.