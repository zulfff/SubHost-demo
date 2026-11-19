# Public four-validator testnet — activation prep and runbook

**Status: PREP, not activation.** Everything below can be prepared now, but the
activation button is gated (Q-037). Do not re-enable public routes or advertise
activation until the gate clears. The goal is same-day activation once it does.

Cross-references: [start-stop.md](start-stop.md), [key-rotation.md](key-rotation.md),
[certified-sync.md](certified-sync.md), [backup-restore.md](backup-restore.md),
[incident-checklist.md](incident-checklist.md), [alerts.md](alerts.md),
[release.md](../release.md). Contract threads: Q-012 (activation), Q-037
(calldata-pricing gate), Q-027/ADR-007 (certified sync / fork accountability).

## 1. The activation gate (Q-037) — must clear first

Activation is blocked until **both** are merged into the deployed revision and
independently verified:

1. **Calldata pricing — Q-037 ladder step L4 merged + verified** (the
   SUBHOST-2026-003 residual). Bytes are capped today but never priced; a
   128 KiB calldata transaction at `gas_limit = 1` pays the same as an empty one.
   The approved shape (option A) is a genesis-bound **flat 16 gas/byte** schedule
   with a version field, producing **v3 headers**: replay dispatches on header
   version (v1 no settlement, v2 `gas_limit`, v3 consumed gas) AND requires the
   recorded schedule; v3-without-schedule and v2-in-v3-history refuse; no bound
   schedule means nothing bumps. A transaction with `gas_limit < intrinsic(data)`
   is **invalid**, never clamped. The ladder is L0 (caps/ranking) → L1 (block.rs
   wiring, raka) → L2 (codec, bima) → L3 (dewi) → **L4 (behavioural wiring,
   bima)** → L5 (verification/RPC). The gate is specifically **L4 merged +
   verified**, not merely an interim mitigation. The option-C interim (tighter
   caps, fee-per-byte ranking) reduces but does not close the gap.
2. **S9.6 encrypted bootstrap loader (Track A)** — public validators must start
   from encrypted keystores, not plaintext `producer.key`. The keystore crate +
   CLI are merged (#59); encrypted **node start-up** (`bootstrap_with_producer_key`)
   is the pending Track A piece. A public validator holding a plaintext signing
   key at rest is not acceptable.

Verification means: the fixed-suite proofs in §5 pass on the exact deployed
revision, and the pricing change is confirmed by its own regressions (intrinsic
gas charged, under-intrinsic transactions invalid, settlement/replay agree).
Record the gate clearance in Q-012/Q-037 with the revision and evidence before
pressing the button.

## 2. Fresh four-validator genesis (template)

A fee/pricing-enabled public chain starts from a **fresh genesis** — never an
in-place upgrade of the demo chain, and never the broken scalar-hash suite.

- **Chain ID 31338** (Q-012), distinct from the chain-1337 demo and chain-1.
- **Four distinct BLS producer keys**, one per validator, each `0600`. Generate
  them fresh; do not reuse demo keys. Once the encrypted bootstrap lands, create
  them with `subhost producer create` (encrypted keystore) and keep the
  credential files owner-only and offline-backed-up.
- **Three-of-four quorum.** Two validators cannot commit; the proofs in §5
  exercise this.
- **Regenerated proofs of possession** under the corrected RFC 9380 suite. Legacy
  proofs/signatures are refused; there is no compatibility verifier.
- **Fee policy and (once Q-037 lands) the calldata schedule** bound into the
  genesis and `consensus_context()`. Two chains with different schedules can
  never share a genesis commitment.

Generation (local fixture pattern; adapt paths/ports for the public hosts):

```sh
# Builds a fresh 4-validator genesis + keys under a new directory, chain 31338.
# The example creates keys once at 0600 and refuses a conflicting setup.
cargo run -p subhost-node --example local-testnet -- \
  --data-dir /var/lib/subhost-testnet --smoke
```

For separate public hosts, generate the shared `genesis.json` once (it lists all
four validators with their public keys and PoP), then distribute the **same**
genesis to every node and relay. Each host keeps only its own producer key.
**Never commit genesis keys, producer keys, credential files or RPC tokens to
the repository.** The genesis document itself (public keys + PoP) is public;
the signing keys and tokens are not.

## 3. Deployment revision pin

Before activation, pin and record:

- The exact `main` commit deployed (the gate-cleared revision from §1).
- The release binary/image **digest and Sigstore identity**, verified per
  [release.md](../release.md). Save the old verified digest for rollback.
- The genesis file hash and chain ID (31338).

Every validator and relay runs the **same** pinned revision and genesis. A
mixed-revision mesh is not a supported activation state.

## 4. Redundant archives and private tokens

**Two certified-history relays** (redundant archives), per
[certified-sync.md](certified-sync.md):

```sh
cargo build --release -p subhost-cli --bin subhost --example sync-relay
# Relay A and Relay B: same genesis, separate data dirs and listeners.
target/release/examples/sync-relay \
  --genesis ./testnet/genesis.json --data-dir ./testnet/certified-history-a \
  --p2p-listen /ip4/<host>/tcp/<portA> --bootstrap-peer <mesh-peer>
```

- The relays store **no signing keys** and cannot produce a quorum; they only
  replay authenticated history. Two archives preserve certificate availability
  if one restarts or misses traffic.
- Archive v2 requires RFC 9380 signatures and final-commit certificates bound to
  the exact genesis. Preserve any legacy archives separately; never mix suites.

**Private RPC tokens** on every public-facing node:

- A non-loopback RPC bind requires `--rpc-allow-public` **and**
  `--rpc-auth-token-file`. Generate a distinct owner-only token per node; never
  commit it. There is no server-side TLS — terminate TLS at a reverse proxy.
- Bind the metrics exporter and `/health/*` to loopback or a private interface
  (unauthenticated); front with an authenticating proxy if exposed.

## 5. Running-proofs plan (on the deployed revision)

Run the fixed-suite gates against the **pinned deployed revision**, not just the
demo fixture. These exist as tests on `main` and must pass there:

| Proof | Test | What it shows |
| --- | --- | --- |
| Quorum shortfall + late/offline catch-up + hub restart | `subhost-node::testnet_tests::four_validator_testnet_recovers_from_late_bootstrap_and_hub_restart` | two-of-four cannot commit; a third reaches quorum; a late fourth catches up; the hub restarts to a common commit |
| Minority partition heal | `subhost-node::liveness_tests::two_minority_partitions_converge_after_the_network_link_is_restored` | partitions converge after the link is restored |
| QC forgery / shortfall | `subhost-consensus::qc_validation_rejects_forgeries_and_shortfalls` | forged/insufficient quorums rejected |
| Relay QC-forgery regression | `subhost-cli::sync_relay::tests::public_possession_proofs_cannot_forge_history_certificates` and `insufficient_or_forged_quorums_and_tampered_execution_never_enter_history` | public PoP cannot forge history certificates under the corrected suite |
| Catch-up never starts on forged far height | `subhost-node::basic_tests::a_forged_far_height_commit_never_starts_catch_up` | a forged high commit cannot trigger catch-up |

Additionally exercise on the live mesh: a fresh observer with only the public
genesis catches up from a relay; a real quorum shortfall (stop two validators)
halts finality and recovers when a third returns; an offline catch-up after a
restart. Record results with the revision in Q-012.

## 6. Re-enabling the public routes (the 503 button)

Public testnet/quorum routes are held at **503** by the security gating; only
chain-1 RPC is running. Re-enable only after §1–§5 are satisfied and recorded:

1. Confirm the gate clearance (§1) and the pinned revision (§3) in Q-012/Q-037.
2. Start the four validators and two relays on the pinned revision with the
   fresh genesis (§2), private tokens and redundant archives (§4).
3. Run the live-mesh proofs (§5); confirm `/health/ready` is 200 on every
   validator and the committed/finalized heights agree across the mesh.
4. Re-enable the public routes (remove the 503 hold) at the reverse proxy /
   publication layer. Verify an authenticated public RPC call succeeds and an
   unauthenticated one is refused.
5. Watch [alerts.md](alerts.md): height stall, no-quorum finality-stall proxy,
   finality lag, QC latency, ledger growth, pending-cleanup failures. A halted
   node (conflicting final evidence) pages via not-ready — follow
   [incident-investigation.md](incident-investigation.md), not a restart loop.
6. Announce activation with the actual enabled features and the verification
   evidence. Do not claim finality, sync or pricing guarantees the revision does
   not yet provide.

## 7. Soak (S12.3) and abort

- **Soak:** 30 days, four validators, **no manual ledger repair**, published
  incident log. A manual repair is a soak-blocking event — record it explicitly
  ([incident-checklist.md](incident-checklist.md)).
- **Abort/rollback:** if a safety incident occurs (conflicting final evidence,
  forged certificate accepted, key compromise), halt public routes back to 503,
  preserve evidence (journal, `fork-incidents.bin`, archives, logs), and follow
  the incident runbook. Roll back to the saved verified digest only with a
  compatible genesis; never roll finalized history back. The ledger is
  append-only — recovery is restore/resync, not rewrite
  ([ledger-repair.md](ledger-repair.md)).

## Prep checklist (do now, activation later)

- [ ] Genesis template + fresh-key generation procedure rehearsed on a staging
      mesh (chain 31338, four distinct keys, regenerated PoP).
- [ ] Revision-pin + digest/Sigstore verification procedure ready (§3).
- [ ] Two-relay redundant-archive config + private-token generation ready (§4).
- [ ] Running-proofs plan scripted against a candidate revision (§5).
- [ ] 503 re-enable steps + monitoring dashboards/alerts staged (§6).
- [ ] Soak incident-log template ready (§7).
- [ ] **Gate watch:** Q-037 calldata-pricing L4 merged+verified; S9.6 encrypted
      bootstrap loader merged+verified. Activation waits on these.