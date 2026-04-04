# Testing

The authoritative automated suite lives in `./tests`.

## What is covered

- index-first resolution using local `pubtime`
- per-crate HTTP fallback when `pubtime` is missing
- fail-closed behavior for registries without release-time metadata
- `warn` mode behavior for the same condition
- `skip_registries` by name and by URL

## How the deterministic suite works

The integration harness starts a local sparse registry server and runs real
Cargo commands against it:

1. generate a lockfile with the fresh version
2. run `cargo-cooldown`
3. inspect the resulting lockfile, verbose cooldown logs, and server request counts

This keeps the suite offline and deterministic while still exercising the
binary end-to-end.

The suite does not rely on committed fixture snapshots under `examples/fixtures`.
It synthesizes the registry, tarballs, cacheable index responses, and workspace
at runtime inside a temp directory. That keeps the test inputs aligned with the
current resolver instead of preserving legacy lockfile snapshots.

When `COOLDOWN_VERBOSE=true`, the binary emits a direct stderr line for each
inspected crate:

- `release_time_source=index_pubtime`
- `release_time_source=registry_api_fallback`

The deterministic integration tests assert those markers in the `pubtime` and
fallback scenarios so the timestamp source stays observable.

## Commands

Run everything:

```bash
cargo test
```

Run the deterministic integration suite only:

```bash
cargo test --test integration -- --nocapture
```

Coverage:

```bash
cargo llvm-cov --all-features --workspace --all-targets
```
