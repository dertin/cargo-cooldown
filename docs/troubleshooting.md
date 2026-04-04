# Troubleshooting

## "missing release timestamp"

Meaning:

- the package came from a registry that participates in cooldown;
- the local index did not provide `pubtime`;
- fallback HTTP did not provide a usable timestamp either.

Options:

- add the registry to `skip_registries`;
- switch to `COOLDOWN_MODE=warn` if you want best-effort behavior;
- ensure the registry exposes either `pubtime` or a usable API.

## "registry ... does not provide cached metadata"

The local Cargo registry cache does not contain the crate entry and fallback
could not supply it. This usually means the registry was never fetched locally
or does not expose fallback metadata in a compatible way.

## A skipped registry still affects the resolver

This is expected. `skip_registries` prevents cooldown processing, but semver
constraints from those packages still shape which versions are valid elsewhere
in the graph.

## A package did not downgrade

Possible reasons:

- no older compatible version exists before the cutoff;
- the package is allowlisted;
- the package comes from a skipped registry;
- Cargo rejected the candidate because of blockers elsewhere in the graph.
