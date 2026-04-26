# Testing

The authoritative automated suite lives in `./tests`.

## What is covered

- index-first resolution using local `pubtime`
- per-crate HTTP fallback when `pubtime` is missing
- default baseline behavior for unchanged lockfile entries
- opt-in `lockfile_policy = "all"` behavior
- fail-closed behavior for registries without release-time metadata
- `best_effort` mode behavior for the same condition
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

That script warms one real crates.io snapshot first, then measures visible
wall-clock cooldown runs under `strict` mode, allows network access in measured
samples, reports whether any registry API fallback was observed, and copies
every measured `Cargo.lock` plus its cooldown log for manual validation. Use
`BENCH_OFFLINE=1 BENCH_PREFETCH_COOLDOWN=1` for an isolated offline run; the
preload phase is reported separately because it runs cooldown too. The runner
uses the normal Cargo cache by default; set `BENCH_ISOLATED_CARGO_HOME=1` to
measure from an empty temporary Cargo home.
Measured `Cargo.lock` files are copied under
`target/cargo-cooldown-benchmarks/<run-id>/`; override `BENCH_ARTIFACT_ROOT` or
`BENCH_RUN_ID` to choose a stable location/name.

Use the timing target when diagnosing resolver cost:

```bash
RUST_LOG=cargo_cooldown::timing=debug ./examples/run-crates-io-large-60d-benchmark.sh
```

The shared crates.io benchmark runner is:

```bash
./examples/run-crates-io-benchmark.sh
```

Run the aggressive 60-day crates.io benchmark using a larger workspace:

```bash
./examples/run-crates-io-large-60d-benchmark.sh
```

That workload intentionally pulls a much larger transitive graph from crates.io
so the cooldown batch solver does substantially more work than the small smoke
workspace. It defaults to visible wall-clock runs without cooldown preload. Use
`COOLDOWN_MINUTES=131401` to push the same benchmark to roughly 3 months.

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
