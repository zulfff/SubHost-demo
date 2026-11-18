# ADR-012: RPC fee history describes recorded legacy fees

- Status: accepted
- Date: 2026-11-18
- Settles: `Q-008`
- Tracks: B (RPC methods), A (future sync and consensus contract)

## Context

S5.2 adds block-by-hash queries, fee history, a priority-fee recommendation and
sync reporting before S3 implements catch-up and S6 implements a fee market.
The current ledger has no historical base-fee field. Inferring that field from
today's admission floor would make history depend on current configuration.

## Decision

Return zero pre-EIP-1559 base fees, including the next-block entry for a nonempty
history range. Compute reward percentiles from recorded transaction gas prices,
weighted by recorded consumed gas. Bound requests to 1024 retained blocks and
100 strictly increasing percentiles. The priority-fee recommendation equals the
current minimum gas price. Block queries and history operate on retained data.

Return `eth_syncing: false` while no catch-up worker exists, with an explicit
statement that this does not certify peer convergence or readiness. Return
`-32004` for logs and subscriptions until S10 supports those operations.

## Consequences

S5.2 changes no shared types or ledger format. S6 must supply actual historical
base fees before these RPC methods advertise EIP-1559 behavior; S3 must supply
real sync progress before `eth_syncing` can report it. Pending admission under S2
does not change these historical fee semantics. Existing auth middleware remains
in front of every method. See [RPC documentation](../../rpc.md) for parameter and
error contracts.
