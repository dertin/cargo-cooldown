# Examples

These examples intentionally use real crates.io packages, so they are
non-deterministic and are meant for manual smoke checks and performance probes.
The deterministic test suite builds local registries under `tests/`.

| Path | Purpose | Used by |
| --- | --- | --- |
| `crates-io-smoke-workspace/` | Small crates.io-backed workspace for fast manual checks and the default benchmark. | `run-crates-io-smoke.sh`, `run-crates-io-benchmark.sh` |
| `crates-io-large-benchmark-workspace/` | Larger dependency graph for the aggressive 60-day cooldown benchmark. | `run-crates-io-large-60d-benchmark.sh` |
| `run-crates-io-smoke.sh` | Runs build/check-style smoke scenarios against current crates.io state. | `crates-io-smoke-workspace/` |
| `run-crates-io-benchmark.sh` | Shared wall-clock benchmark runner. It warms crates.io, runs cooldown, reports fallback usage, and stores measured `Cargo.lock` artifacts. | configurable via `WORKSPACE_DIR`; defaults to `crates-io-smoke-workspace/` |
| `run-crates-io-large-60d-benchmark.sh` | Preset for the large workspace with a 60-day cooldown window. | `run-crates-io-benchmark.sh` |

`run-crates-io-benchmark.sh` is the canonical benchmark script. Preset scripts
only set workload-specific environment variables before calling it.
Benchmarks default to `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=fallback`
because current crates.io graphs can contain fresh resolver-constrained groups;
set `COOLDOWN_INCOMPATIBLE_PUBLISH_AGE=deny` when you intentionally want the
fail-closed policy to make the run fail on unresolved fresh versions.

The same runner is also exposed through Cargo's benchmark command:

```bash
cargo bench --bench crates_io_cooldown -- --scenario large-60d
```
