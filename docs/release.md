# Release and artifact verification

Release tooling is prepared for the current pre-production workspace. It does
not establish audit completion, a four-week network soak, EVM compatibility or
production readiness. Those remain the S12 release gates in [ROADMAP](../ROADMAP.md).

## Version and RPC compatibility policy

The workspace is still `0.1.0`. A release tag must exactly match its package
version: `v0.1.0` today, or `v1.0.0-rc.N` after a separately reviewed version
change. The pipeline rejects mismatched/noncanonical tags and dirty source trees.
Do not label an unchanged `0.1.0` binary as a v1 release candidate. v0 and
alpha/beta/rc releases are prereleases. A stable v1 tag is refused unless the
maintainer has completed S12 review and set the protected release environment's
`SUBHOST_STABLE_RELEASE_APPROVED` variable to `true`.

During v0, changes to transaction encoding, genesis proofs, consensus wire data,
signing journals and ledger versions may require fresh directories and coordinated
peer upgrades. Every incompatible change must be documented and versioned.
For v1, removal/renaming of an RPC method, an incompatible parameter/result type,
or changed success/error semantics requires a major version. Additive optional
methods/fields may use a minor version; fixes preserving the documented contract
may use a patch version. Ethereum-shaped method names do not imply full Ethereum
execution or transaction compatibility. See the RPC reference as methods land.

## Build identity and artifacts

The release workflow starts on a `v*` tag and first runs all seven reusable CI
jobs. The tagged commit must be an ancestor of `main`. Before enabling releases,
configure a GitHub environment named `release` with required maintainer reviewers
and a tag-only deployment policy. The workflow grants content/package/OIDC write
permissions only to the protected publish job; PR builds have read permissions.

Binary builds use this immutable compiler/linker image:

```text
rust:1.93.0-slim-bookworm@sha256:776861219cd851131c1cec3bbd7cbeb16b99a794048097eb69ad9682a8ed0d57
```

`cargo build --locked` builds Linux x86_64 GNU binaries with default features.
Optional OTLP or other features require a separately documented artifact variant
and matching SBOM. Source paths are mapped to `/workspace`. Cargo's test profile
uses `opt-level=1` for pairing-heavy regressions and explicitly retains debug
assertions and integer overflow checks; optimization does not disable validation.

To reproduce a reviewed source build with Docker, Python 3.11+, the pinned Rust
toolchain and cargo-cyclonedx available:

```sh
cargo install --locked --version 0.5.10 cargo-cyclonedx
bash scripts/release/build.sh v0.1.0 dist
```

The destination must not already exist. Output contains one archive with the
four shipped binaries (`subhost`, `subhost-faucet`, `subhost-explorer`,
`subhost-bench`), their CycloneDX inventories, `RELEASE-METADATA.json` and
`SHA256SUMS`. Metadata binds the source commit, version, compiler, target and
Cargo.lock checksum. Tar ownership, order and times are normalized; gzip time
is zero. SBOM serial IDs/timestamps are derived deterministically from source
identity. Repackaging identical inputs is tested to produce identical bytes.
Independent complete compiler rebuild reproducibility is not yet established;
compare checksum manifests from separate builders before making that claim.

Cargo-cyclonedx has no `--locked` option. The wrapper generates metadata offline
and refuses publication if Cargo.lock changes. It verifies each SBOM describes
the intended crate. A failed build or missing/wrong binary/BOM leaves no partially
published output directory and never overwrites a preexisting release directory.

## Verify downloads before installation

Download all release artifacts into one new directory. The pipeline signs
`SHA256SUMS` with Cosign 3.1.4, producing a Sigstore verification bundle. Verify
the exact workflow/tag identity and then every covered file:

```sh
bash scripts/release/verify.sh ./download v0.1.0
```

The verification identity is
`https://github.com/zulfff/SubHost-demo/.github/workflows/release.yml@refs/tags/v0.1.0`
with issuer `https://token.actions.githubusercontent.com`. For an authorized fork,
set `SUBHOST_RELEASE_REPOSITORY` to that fork's exact `owner/repository`.
Do not substitute an identity wildcard. The checksum verifier rejects path
traversal, duplicate entries, symlinked artifacts and files omitted from the
signed inventory. Install only after both signature and checksum verification.

## Container identity and operation

The image builder and runtime base images are digest-pinned. Runtime package
security updates are intentionally allowed during construction, so a rebuilt
container may differ; deploy the verified published image digest. The workflow
scans a local candidate before registry push, scans its final immutable digest,
then signs it and publishes the binary release. HIGH/CRITICAL vulnerabilities
with available fixes fail the gate; unfixed findings remain visible and require
review in the release/audit record rather than being claimed absent.
The image also ships the four Rust dependency inventories under
`/usr/share/subhost/sbom/`; CI checks their actual crate identities and inventory
contents after the image build.

Verify the image with the exact tag workflow identity before using its digest:

```sh
cosign verify ghcr.io/zulfff/subhost-demo@sha256:YOUR_VERIFIED_DIGEST \
  --certificate-identity 'https://github.com/zulfff/SubHost-demo/.github/workflows/release.yml@refs/tags/v0.1.0' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

The default image command shows help so deployment chooses a mode explicitly.
`docker-compose.release.yml` is an observer template for an existing corrected
validator genesis. It runs UID/GID 10001, read-only rootfs, drops capabilities,
restricts `/tmp`, and keeps RPC/metrics host ports on loopback. Prepare a data
directory owned by that account, including the trusted genesis; provide a
separate owner-only bearer-token file readable by UID 10001. Set `SUBHOST_IMAGE`
to the verified GHCR digest, `SUBHOST_DATA_DIR` and `SUBHOST_RPC_TOKEN_FILE`, then
review `docker compose -f docker-compose.release.yml config` before starting it.
Use a TLS reverse proxy for public RPC. Add trusted bootstrap peers according
to the current deployment/network contract. The container RPC healthcheck
checks responsive authenticated chain metadata; it does not assert quorum,
native-sync completion or finality. Node readiness uses the S8 runtime contract.

## Evidence

Verified 2026-11-19:

- [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html):
  test inherits dev; optimization/assertions/overflow checks are independent.
- [Cosign 3.1.4](https://github.com/sigstore/cosign/blob/v3.1.4/README.md):
  keyless bundle signing and verification against expected identity/issuer.
- [GitHub OIDC](https://docs.github.com/en/actions/reference/security/oidc):
  least-privilege OIDC permissions and protected environments.
- [cargo-cyclonedx 0.5.10](https://github.com/CycloneDX/cyclonedx-rust-cargo/tree/cargo-cyclonedx-0.5.10):
  JSON/target inventory and command option semantics.
