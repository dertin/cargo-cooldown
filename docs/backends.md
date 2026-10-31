# Resolution backends

Set `[cooldown].backend` to `auto` (default), `filtered`, `native`, or `legacy`.
`COOLDOWN_BACKEND` has the highest precedence, after user/workspace/member files.
Invalid values fail configuration loading.

| Backend | Behavior |
| --- | --- |
| `auto` | Prefer native when its Cargo version and complete policy are supported; otherwise filtered, otherwise legacy. A known unsupported sparse metadata condition, or a failed fallback resolution after age filtering, retries legacy. Network and validation failures remain failures. |
| `filtered` | Use our own sparse-index policy before one Cargo resolution. Unsupported input fails with a diagnostic. No native cooldown is used. |
| `native` | Require Cargo 1.100+ and a policy proven equivalent to native resolution. Unsupported policies or versions fail. |
| `legacy` | Keep the post-resolution cooling algorithm from 0.3.5, including API timestamp fallback. |

## Current support boundary

Filtered supports public sparse registries configured in Cargo's hierarchy,
full/targeted updates, initial lockfiles, and preparation for check/build/test/run
when the lockfile is absent. It preserves package/version allowances, registry
age overrides, skipped registries and the initial lockfile baseline. Dependency
resolution, including conflicts, features and target-specific dependencies, stays
with Cargo. The candidate's registry identities and checksums are checked against
the same filtered index data before publication. Subsequent compilation uses the
original sources.

Offline/frozen/locked invocations, precise updates, custom Cargo `--config`/`-Z`,
source replacement/vendor, Git indexes/dependencies, private-registry credential
configuration, and guard commands with existing lockfiles retain legacy handling.
Custom CA bundles (`http.cainfo`, `http.proxy-cainfo`, `CARGO_HTTP_CAINFO`, or
`CARGO_HTTP_PROXY_CAINFO`) also select legacy in auto mode, leaving certificate
loading to Cargo. Forced filtered resolution rejects them before changing the
lockfile; native is also ineligible because its validation can use our HTTP client.
Cargo configuration containing `include` also requires legacy so included settings
are interpreted by Cargo.
An index without required `pubtime` data also uses legacy in auto mode. Forcing
filtered reports missing timestamps rather than inventing dates. Cross-registry
index dependencies must name configured registries.

With `incompatible-publish-age = "fallback"`, supported commands first try a
fully compliant filtered resolution. Success needs no fallback acceptance. If
resolution fails after versions were excluded by age, `auto` invokes legacy with
the original policy and acceptance settings. For updates, it restores the initial
lockfile in the same temporary lockfile and retains lockfile coordination across
the retry. Other unsupported cases create a new temporary lockfile.
Forced `filtered` reports that legacy is required instead of switching engines.
Network and validation failures abort without publishing; they do not trigger
age relaxation. If legacy is needed, the extra filtered attempt adds work.

Native delegation is intentionally conservative: no package/exact/global allow
rules, simulated clock, skipped registries, or registry-specific wrapper overrides;
no conflicting native age configuration; and no existing baseline, except explicit
`generate-lockfile` with baseline ignore. Existing fresh floors are not equivalent:
native full updates can downgrade a fresh locked version that our floor preserves.
Cargo's version is queried through the same `cargo` launcher used for resolution,
including `RUSTUP_TOOLCHAIN` inherited from `cargo +toolchain cooldown ...`.
Rustup directory overrides are resolved before entering isolation and pinned for
both Cargo and its compiler/metadata subprocesses. Pre-stabilization
1.100 nightlies are conservatively excluded; beta/stable 1.100 and later minors
are eligible.

## Native prefilter with the legacy guard

On Cargo 1.100+, `auto` can also accelerate a full `update` that needs legacy,
including private registries and custom CA bundles. This is a native prefilter
against the existing temporary lockfile, followed by the full legacy policy guard;
it does not make forced `native` accept unsupported policies.

The prefilter uses each registry's effective cooldown, capped by the shortest
package allowance. The guard then enforces longer package/registry windows.
Skipped registries and zero-age overrides remain exempt. Cargo handles registry
authentication and TLS. Exact-version allowances, simulated time, existing
`floor` baselines, targeted/precise updates, Cargo overrides, source replacements,
configuration includes and environment-supplied index URLs retain the original
resolver. A zero package allowance can eliminate useful prefiltering entirely.
An existing lockfile is eligible with `lockfile-baseline = "ignore"`.

If the native attempt fails, its candidate is discarded and the initial lockfile
is restored before running the original resolver in the same isolation. The
configured `deny`/`fallback` policy and fallback acceptance still govern the guard
and publication. A native success never bypasses that guard. Forced `legacy`
disables the optimization, including on new Cargo versions.

For `update`, the legacy guard inspects all workspace features because the
lockfile includes inactive optional dependencies too. Configured allowances,
baseline floors and fallback behavior still apply; `deny` no longer overlooks
fresh optional dependencies just because their features are inactive. Guards for
build/check/test continue to use the requested feature selection.

Cargo's stabilization is recorded in [Cargo PR #17335](https://github.com/rust-lang/cargo/pull/17335).
The filtered engine uses Cargo's documented
[source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html)
mechanism; replacement entries retain the upstream checksum and dependency data.

The wrapper neutralizes `resolver.incompatible-publish-age` through the Cargo
child environment outside the explicit native prefilter/delegation so file configuration cannot discard
versions allowed by wrapper exceptions. Explicit command-line Cargo overrides
remain a legacy compatibility case.

## Lifecycle and caching

The HTTP server binds only to 127.0.0.1. It uses a stable port derived from the
workspace and CARGO_HOME, falling back to a new owned port if occupied. It is not
a persistent daemon. Eight owned workers reuse inbound/outbound connections;
normal completion and errors stop and join the workers. Cargo's loopback requests
bypass proxies. Upstream HTTP honors `CARGO_HTTP_PROXY`, then Cargo's `[http].proxy`;
without either, reqwest uses its default proxy environment and TLS configuration.
An empty Cargo proxy disables upstream proxying. Upstream HTTP failures other than
404, or an invalid index, abort publication even if Cargo could backtrack to an
older graph.

Original index documents are stored under `COOLDOWN_CACHE_DIR/sparse-originals-v1`
(or the platform cargo-cooldown cache). Cache records include their upstream URL,
ETag and Last-Modified, and are atomically replaced. Each run revalidates originals
and recomputes filtering using that run's clock. A SHA-256 ETag of the resulting
filtered document lets Cargo reuse its parsed index on `304 Not Modified`. Filtered data is never reused as
an authoritative age cache. Cargo may cache the proxy index; final validation
still checks the current policy. Native validation reads Cargo's refreshed sparse
cache under its package-cache lock and applies the same timestamp and checksum
checks; unavailable cache entries fall back to HTTP.

The lockfile isolation holds coordination before reading the baseline,
preserves external path mapping and lockfile permissions, and checks original bytes
before atomic publication. Native eligibility is checked again against the baseline
observed after coordination; a newly published baseline rejects forced native and
makes auto retry legacy. Dry runs and failed resolutions do not publish. An
ungraceful process termination can still leave the existing coordination marker;
confirm no cooldown process remains before removing it. No server survives the
owning process, and the visible original lockfile remains intact until publication.

`COOLDOWN_VERBOSE=true` reports discovery and selection timings and Cargo invocation
counts. Filtered/native runs also report isolation, preparation, resolution,
validation and publication timings, resolution count, upstream requests and decoded
response-body bytes. TLS/header bytes are not counted as response-body bytes.
