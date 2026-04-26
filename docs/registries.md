# Registries

`cargo-cooldown` processes registry dependencies only. `path` and `git`
dependencies are ignored by cooldown.

## Supported registry kinds

- crates.io
- alternate registries declared in Cargo config
- sparse registries
- mirrors and source replacements
- internal registries such as CodeArtifact, as long as they expose a Cargo
  registry index

## Source of truth

The effective registry is resolved from Cargo's own configuration instead of a
second routing config inside `cargo-cooldown`.

That means:

- crates.io follows Cargo's active protocol and `replace-with` rules;
- alternate registries use their configured index URL;
- the local registry cache location matches Cargo's own hashing and layout.

## `skip_registries`

`skip_registries` is an explicit opt-out.

If a registry matches `skip_registries`:

- `cargo-cooldown` does not read its local index;
- it does not perform fallback HTTP requests;
- it does not evaluate freshness;
- it does not downgrade packages from that registry.

Matching is supported by:

- logical name, such as `crates-io` or a registry name from Cargo config;
- effective registry URL, such as `sparse+https://example.com/index/`.

## Missing timestamps

For registries that are not skipped:

- local `pubtime` is preferred;
- fallback HTTP is attempted only when needed;
- if neither path yields a usable timestamp, `strict` enforcement fails closed;
- under `cargo_compatible` enforcement, the missing timestamp is reported as a
  warning and the run continues.

For registries like CodeArtifact that may not expose the required metadata for
cooldown, the intended workflow is to list them in `skip_registries`.
