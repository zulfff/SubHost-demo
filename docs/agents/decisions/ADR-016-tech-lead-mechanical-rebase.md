# ADR-016: Tech-lead mechanical rebase of an unresponsive agent's PR

- Status: accepted (tech-lead decision under maintainer delegation, 2026-11-20)
- Deciders: tech lead (unseated), maintainer delegation ("own all decisions")
- Affected: PR #84 `s6.1b/mempool-persist-node` (raka, track-a)

## Context

PR #84 (opt-in mempool persistence consumer, Q-016/Q-038) was reviewed and
approved by nara with content unchanged, but its base (`56edbab`) went stale
under the #86/#78/#87/#89 merge wave and GitHub reported it DIRTY. The owning
session (raka) received five delivered messages over the wave and produced no
response or movement; its last readable output was empty. The PR blocks yoga's
`--mempool-persist` flag (R-84-01 Low), the S6.1b close-out, the Q-036 node
trigger slice, and the network accessor needed for the peer-count gauge.

The working agreement forbids rewriting another agent's branch. The tech lead
escalated two options to the maintainer (poke/restart the session, or authorize
a one-time mechanical rebase); no answer arrived, and the standing direction is
to keep the merge loop running until every mergeable slice lands.

## Decision

The tech lead performs a one-time **mechanical rebase** of #84 onto `origin/main`
(`3148a4e`), under these constraints:

1. Conflict resolution keeps both sides verbatim wherever the conflict is
   adjacent additions (CHANGELOG entries, node Cargo.toml/Cargo.lock deps,
   basic_tests.rs test blocks). No semantic rewrite of raka's code.
2. **Patch-identity verification**: the sorted added/removed content lines of
   the rebased diff against the new base must equal those of the original
   commit against its old base. Verified: identical (232 insertions,
   12 deletions, same as the original).
3. The rebase is recorded in this ADR, landed with the code change it enables,
   and noted in the PR. Authorship of the code commit stays raka's.
4. This is an exception for an unresponsive session, not a precedent for
   rewriting active agents' branches. The normal rule stands.

## Consequences

- #84 can merge on green gates; yoga's stacked flag rebases with
  `git rebase --onto origin/main 1641df7` afterwards (Q-038 plan).
- If raka's session recovers, it must not force-push over the rebased branch;
  the seat keeps ownership of follow-up work (node trigger, network accessor).
- Future stalls follow the same ladder: deliver messages, escalate, then a
  recorded, patch-verified mechanical intervention only when the session is
  demonstrably unresponsive and the slice blocks the critical path.