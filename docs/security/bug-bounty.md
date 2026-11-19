# Bug bounty scope and disclosure policy

S12.4. This document defines what security research is in scope for Subhost and
how vulnerabilities are reported, triaged, and disclosed. It complements
[`SECURITY.md`](../../SECURITY.md), which remains the authoritative, code-verified
statement of the current security posture.

> **Reward status.** There is **no funded bug bounty program** today. No monetary
> reward is offered or implied by this document. If the maintainer establishes a
> reward pool later, it will be announced separately with its own terms; until
> then, reports are handled as coordinated vulnerability disclosure, not as paid
> bounty submissions.

## Supported target

This is a pre-production codebase. Security fixes land on the default branch
(`main`). There is no supported release branch and no production network. Research
should target the current `main` tip; include the exact commit you tested.

## In scope

Every package under `crates/` and `explorer/`. Vulnerability classes of interest
include, non-exhaustively:

- **Consensus safety/liveness**: equivocation, quorum forgery, prepared-lock or
  timeout/view-change manipulation, journal rollback, anything that finalizes
  conflicting history or stalls an honest quorum.
- **Cryptography**: BLS/ed25519 signature forgery or verification bypass, rogue-key
  attacks, proof-of-possession flaws, weak randomness, nonce reuse.
- **Key management**: producer-key or wallet-keystore exposure, KDF bound bypass,
  credential-file handling, secret material in logs/errors/memory.
- **Sync and fork handling**: certificate authentication bypass, history
  fabrication, unbounded resource use via `GetBlocks`/`Blocks`, false or missed
  fork-incident halts.
- **RPC/auth**: authentication or sender-binding bypass, unauthorized state
  mutation, bind-policy bypass, injection.
- **Storage integrity**: ledger/journal/archive tampering accepted as valid,
  checksum or commitment bypass, crash-recovery corruption.
- **Denial of service** with a concrete, bounded reproducer against a default
  configuration (see out-of-scope for roadmap gaps).

## Out of scope

- Deliberate, documented properties listed under "Known security posture" in
  `SECURITY.md` (for example: opt-in JSON-RPC auth defaulting off, no in-process
  TLS, unauthenticated metrics/faucet, the single-key IBC light client,
  membership-only state proofs). These are known limitations, not undisclosed
  vulnerabilities; report them only if you find a way to escalate their impact
  beyond what is documented.
- Roadmap gaps already tracked: per-peer rate limiting and peer scoring (S7),
  encrypted mempool / Dandelion++ / threshold encryption, inactivity leaks,
  weak-subjectivity checkpoints, validator-set rotation, encrypted node-start
  producer key (S9.6).
- Findings already reported and tracked in `security-audit/` (e.g.
  SUBHOST-2026-001/002/003 and R-57-01). Duplicate reports of a known, unfixed
  finding are not new submissions.
- Social engineering, physical attacks, attacks on third-party services or
  dependencies without a demonstrated exploit in this codebase, and volumetric
  DDoS without a protocol-level amplifier.
- Issues requiring non-default, insecure operator configuration (e.g. exposing the
  explorer or a public RPC bind without a token against documented guidance).

## Reporting

Do **not** open a public issue for a suspected vulnerability. Report privately via
either channel:

- GitHub Security Advisories:
  <https://github.com/zulfff/SubHost-demo/security/advisories>
- Email: `security@subhost.xyz`

Include: the affected commit, exact file and line, impact, and concrete
reproduction steps (a minimal reproducer is ideal). Never include real private
keys, credentials, or production data. Use disposable fixture material for any
proof-of-concept.

## Triage and disclosure

- Acknowledgement target: within **five business days**.
- Triage assigns a severity (below) and a tracking identifier under
  `security-audit/`.
- We coordinate disclosure once a fix or mitigation is available. Please do not
  disclose publicly before we agree on a date; we aim to publish a fix and a
  credit (if you want one) together.
- Good-faith reports that turn out to be out of scope or duplicates are closed
  with an explanation, not penalized.

## Severity rubric

- **Critical**: consensus safety break, signature forgery, key extraction, or
  unauthorized finalization/state mutation reachable by an unprivileged peer.
- **High**: authentication/binding bypass, storage-integrity bypass, or a
  remotely triggerable persistent halt of honest validators.
- **Medium**: bounded DoS with a concrete reproducer, false fork-incident halts,
  or info-leak of secret material under realistic conditions.
- **Low**: hardening gaps, minor info leaks, or issues requiring unlikely
  preconditions.

## Safe harbor

We support good-faith security research. If you:

- make a good-faith effort to avoid privacy violations, destruction of data, and
  interruption or degradation of service;
- only interact with accounts you own or with explicit permission of the holder;
- do not exploit a finding beyond what is needed to demonstrate it; and
- report privately and give us reasonable time to fix before disclosing,

we will not pursue or support legal action against your research. This safe harbor
does not cover unlawful acts, extortion, or attacks on third parties.

## Rewards

None funded. See the reward-status note above. Recognition/credit in the advisory
and `security-audit/` record is offered for valid reports if the reporter wants it.