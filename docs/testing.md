# Testing

The authoritative automated suite lives in `./tests`.

## What is covered

- index-first resolution using local `pubtime`
- per-crate HTTP fallback when `pubtime` is missing
- default baseline behavior for unchanged lockfile entries
- opt-in `lockfile_baseline = "ignore"` behavior
- fail-closed behavior for registries without release-time metadata
- `cargo_compatible` enforcement behavior for the same condition
- `skip_registries` by name and by URL
- snapshot reachability for the metadata-derived resolver state
- batch solver coverage for independent, duplicate, optional, target-specific,
  and newly introduced transitive dependencies

## How the deterministic suite works

The integration harness starts a local sparse registry server and runs real
Cargo commands against it:

1. optionally generate a lockfile before the wrapper starts to create a baseline
2. run `cargo-cooldown`
3. inspect the resulting lockfile, verbose cooldown logs, and server request counts

This keeps the suite offline and deterministic while still exercising the
binary end-to-end.

The suite does not rely on committed fixture snapshots under `examples/fixtures`.
It synthesizes the registry, tarballs, cacheable index responses, and workspace
at runtime inside a temp directory. That keeps the test inputs aligned with the
current resolver instead of preserving stale committed lockfile snapshots.

When `COOLDOWN_VERBOSE=true`, the binary emits `DEBUG` logs for each inspected
crate and for the per-pass scan summary:

- `release_time_source=index_pubtime`
- `release_time_source=registry_api_fallback`
- `cooldown: scan_summary ...`

The deterministic integration tests assert those markers in the `pubtime` and
fallback scenarios so the timestamp source stays observable when cooldown
actually runs.

The unit suite also exercises the internal `CargoSnapshot` layer. Those tests
validate:

- reachability projection from `cargo metadata`
- conversion of stored requirement origins back to semver requirements

The integration suite includes multi-crate cooldown fixtures that verify fresh
crates are cooled by validated lockfile batches instead of one `cargo update
--precise` invocation per crate.

There is also one ignored integration benchmark for the same fixture. It prints
elapsed times for the batch solver path.

## Commands

Run everything:

```bash
cargo test
```

Run the deterministic integration suite only:

```bash
cargo test --test integration -- --nocapture
```

Run the batch solver integration benchmark:

```bash
cargo test --test integration benchmark_batch_solver -- --ignored --nocapture
```

Run the default crates.io benchmark using the small smoke workspace:

```bash
./examples/run-crates-io-benchmark.sh
```

The runner warms one real crates.io snapshot, measures cooldown wall-clock
time, reports fallback usage, and stores each measured `Cargo.lock` plus its
log under `target/cargo-cooldown-benchmarks/<run-id>/`.

Use the timing target when diagnosing resolver cost:

```bash
RUST_LOG=cargo_cooldown::timing=debug ./examples/run-crates-io-large-60d-benchmark.sh
```

Run the aggressive 60-day crates.io benchmark using a larger workspace:

```bash
./examples/run-crates-io-large-60d-benchmark.sh
```

That workload pulls a larger transitive graph than the small smoke workspace.
Use `COOLDOWN_MINUTES=131401` to push the same benchmark to roughly 3 months.

The same benchmark runner is available through Cargo's benchmark command:

```bash
cargo bench --bench crates_io_cooldown -- --scenario large-60d
```

This bench target uses a custom harness and delegates to the same script, so the
warm-up, environment variables, and artifact layout stay identical.

Coverage:

```bash
cargo llvm-cov --all-features --workspace --all-targets
```
