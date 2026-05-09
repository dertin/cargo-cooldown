# Releasing

`cargo-cooldown` publishes prebuilt binaries from GitHub Actions so CI users can
install the tool with `cargo-binstall` instead of compiling it from source.

## Release Flow

1. Update `Cargo.toml` and the changelog for the new version.
2. Publish the crate to crates.io.
3. Push a matching git tag, for example `v0.3.1`.
4. Run the `Release` workflow manually with that tag.

The release workflow is intentionally manual. It checks out the tag provided in
the workflow input, rejects tags whose version does not match `Cargo.toml`, and
builds:

- `x86_64-unknown-linux-gnu`
- `aarch64-unknown-linux-gnu`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

Unix targets are uploaded as `.tgz` archives. Windows is uploaded as `.zip`.
The archive names and internal paths match the `[package.metadata.binstall]`
configuration in `Cargo.toml`.

## Security Model

Release assets are built by GitHub-hosted runners and published with GitHub
Artifact Attestations. This is the preferred default for this project because it
uses GitHub OIDC and Sigstore without storing a long-lived private signing key
in repository secrets.

Consumers can verify a downloaded artifact with:

```bash
gh attestation verify cargo-cooldown-x86_64-unknown-linux-gnu-vX.Y.Z.tgz \
  -R dertin/cargo-cooldown
```

`cargo-binstall` also has native signature verification, but currently that path
uses `minisign` metadata and signatures. Add it only if `cargo binstall
--only-signed cargo-cooldown` becomes a hard requirement; that needs either a
carefully protected signing secret or a release process that injects an
ephemeral public key before publishing the crate.

## Manual Trigger

From the GitHub UI:

1. Open `Actions`.
2. Select `Release`.
3. Choose `Run workflow`.
4. Keep the workflow source on `main`.
5. Enter the release tag, for example `v0.3.1`.

From the GitHub CLI:

```bash
gh workflow run release.yml --ref main -f tag=v0.3.1
```

The tag must already exist on GitHub and should point at the commit whose
`Cargo.toml` version matches the tag without the leading `v`.
