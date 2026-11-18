# ADR-008: Phase-separated quorum commits and standard BLS

Date: 2026-11-19. Tracks A and B agreed through Q-014 and Q-015.

## Decision

Use a per-height prepare/commit protocol. A prepare quorum locks an authenticated,
execution-valid block. Only a distinct final Commit-domain quorum certificate
authorizes ledger append. Both require `n - f` distinct registered identities,
where `f = (n - 1) / 3`; genesis voting power does not multiply votes.

Views start at one and advance independently from block height. The sorted
validator at `(view - 1) % n` signs the leader envelope. Replacement proposals
require a timeout certificate containing `n - f` exact-target signed view-change
reports. Reports commit to the complete prepared block and proof. The highest
prepared certificate determines the replacement block; a timeout cannot erase
a prepared lock. The f+1-th highest authenticated target lets lagging replicas
join. Unsupported timeout-only views pause until an honest peer joins. Timeouts
back off up to `max(base, 30 seconds)`. First authenticated phase progress
(proposal, Prepare QC or target TC) starts a full interval; retransmissions cannot
extend it. Newly journalled phase outputs are sent immediately before a ticker
can retire their view.

Bind proposal, phase vote, prepare/final certificate and view-change signatures
to the canonical complete genesis hash. RPC commit verifies final phase, context,
newer committed view, producer, parent, height and execution. Read-only
`validate_certified_block` applies the same checks. Restored history has no
verified committed view until an actual tip QC is checked.

Replace legacy hash-and-multiply BLS hashing with RFC 9380 BLS12-381 G2
`XMD:SHA-256_SSWU_RO`, using separate BLS POP signature/proof domains. The old
construction allows arbitrary-message forgery by rescaling a public proof of
possession. There is no legacy verifier fallback. Scalars, public keys and
signatures retain their 32-, 48- and 96-byte encodings.

## Recovery and transport

Version-two `SUBVOTE2` signing checkpoints precede publication of every vote or
report, retaining both phases, view, prepared lock, reports, timeout evidence and
latest final QC. Commit intent precedes ledger append; recovery reconciles that
boundary. Never infer lost signing obligations from the ledger or restore an
older journal over newer signing state.

Canonical typed consensus payloads travel in additive gossip variants. Validator
mode refuses old unphased proposal/vote messages. Actual JSON envelopes fit the
4 MiB transport with framing headroom. Deterministic block budgets reserve
repeated blocks and proof signatures in timeout frames and the 16 MiB journal;
supported node sets contain 1 through 64 validators.

Nodes require explicit `single_writer` for unsigned demo genesis and reject it
with validator genesis or identity. Default timeout is 2000 ms. Track B owns
CLI/scripts consumption of the separately committed configuration contract.
Controlled shutdown stops RPC and joins services; cancellation aborts children.

## Consequences

All validator peers must upgrade together with fresh genesis proofs and fresh
signing/history directories. Preserve legacy data and keys for investigation;
do not reactivate legacy validator binaries. Automatic migration cannot prove
old signatures authentic or establish that discarded signing state never voted.

This is PBFT-style prepare/commit, not chained HotStuff. Independent crypto and
protocol review, constant-time signing assurance, native historical sync, network
abuse controls and actual audit/soak gates remain separate work. Conflicting
final certificates require accountable incident handling rather than automatic
rollback of finalized history (native S3, ADR-007).

## References

- https://www.rfc-editor.org/rfc/rfc9380 — G2 suite and Appendix J.10.1 vectors, verified 2026-11-18.
- https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-bls-signature — POP signature/proof domains, verified 2026-11-18.
- https://pmg.csail.mit.edu/papers/osdi99.pdf — prepare/commit locks and view-change proofs.
- https://github.com/cometbft/cometbft/blob/main/spec/consensus/consensus.md — authenticated locks and round progress, verified 2026-11-18.
