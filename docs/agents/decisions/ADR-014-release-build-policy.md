# ADR-014: Bound release identity and retain test safety checks

Date: 2026-11-19. Tracks A and B agreed through Q-023.

## Decision

Release tags must match the workspace package version and a reviewed main-ancestor
source commit. The workspace remains 0.1.0; v0 and alpha/beta/rc artifacts are
prereleases. Stable v1 requires the real S12 review and protected release approval.
No tag/live release is created by this implementation.
A separate coordinated version contract may advance the workspace and internal
requirements/lockfile to 1.0.0-rc.1 after the current API and infrastructure are
ready. The RC label remains a prerelease and creates no migration guarantee.

Run the seven reusable CI checks before packaging. Pin the compiler to 1.93.0,
build/link in the immutable Rust image and use --locked. Publish source/lock/tool
metadata, deterministic tar/checksum files, target-specific CycloneDX inventories
and Sigstore bundles. Scan the candidate and published immutable OCI image before
signing/release publication. Runtime package security updates intentionally
change image content; deploy verified digests. Independent full binary rebuild
byte equality remains an explicit verification task.

Optimize profile.test at level 1 while explicitly retaining debug assertions
and overflow checks. No test assertions, protocol budgets, deadlines, MSRV or
normal dev/release build profiles are weakened.

## Consequences

Tag/version mismatch, dirty sources, wrong binary/BOM identity, unsafe checksum
paths and overwriting outputs fail before publication. The pipeline has no long-
lived signing key. External audit/soak and protected-environment configuration
remain maintainer gates. Incompatible consensus/ledger upgrades need their own
contracts; release tooling does not silently migrate them. See docs/release.md
and docs/operations/upgrade.md for exact supported formats and verification.

## Evidence

- https://doc.rust-lang.org/cargo/reference/profiles.html — verified 2026-11-19:
  test inherits dev; opt-level/assertions/overflow checks are independent.
- https://github.com/sigstore/cosign/blob/v3.1.4/README.md — verified 2026-11-19:
  keyless bundle verification binds identity and issuer.
- https://docs.github.com/en/actions/reference/security/oidc — verified 2026-11-19:
  least-privilege short-lived OIDC signing identity.
