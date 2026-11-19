# SubHost-demo security finding: unmetered calldata plus full-history replay let one wei force permanent ledger growth and unbounded per-block work

- Target: `SubHost-demo`
- Commit: `fd0fc7e11ccb5fe8ebbb2948a68fd20cee4edab4` (branch `s5/rpc-metrics`)
- Date: 2026-11-19
- Finding ID: `SUBHOST-2026-003`
- Track: B, Platform (`subhost-rpc`, `subhost-storage`, `subhost-mempool`)
- Severity: High
- CVSS v4 base: `7.1` (`CVSS:4.0/AV:N/AC:L/AT:N/PR:L/UI:N/VC:N/VI:N/VA:H/SC:N/SI:N/SA:N`)
- CWE: CWE-400 (Uncontrolled Resource Consumption), CWE-770 (Allocation of Resources Without Limits or Throttling)
- Binary under test: `target/debug/subhost`, sha256 `5742ddcd9e34795984deb42c2c10ddb8174bdfeeb060c38d77db8769793d6cd7`
- Status: Partially fixed. Originally confirmed against the real node over
  JSON-RPC and against `RpcState` directly on `fd0fc7e`. Fix 2 (suffix-only
  append validation) merged in main `fa87215` (PR #50) and re-verified
  2026-11-19; fix 1 (calldata pricing) and fix 3 (automatic retention) remain
  open. See Re-verification below.

## Summary

`eth_sendTransaction` accepts a caller-supplied `data` payload up to the 128 KiB
mempool cap, and never charges gas for it. The 256 KiB request-body cap trims the
HTTP ceiling to slightly less than that, which does not change the issue. The
caller also chooses `gas_limit`, and any nonzero value is accepted. A request with
`gasPrice = 1` and `gas = 1` therefore pays a 1 wei fee for a transaction that can
carry the full payload. `State::apply_transaction` never reads `data` for a
`Transfer`, so the bytes are pure payload that no execution path consumes.

Each accepted submission seals a block. `RpcState::persist` hands the entire
retained block history to `LedgerStore::persist`, and the segmented store calls
`validate_snapshot_from` on every append. That function re-hashes every prior
transaction and re-verifies every prior ed25519 signature before the new record is
written. Block N therefore costs O(N), the total cost to reach N blocks is
O(N^2), and every byte of calldata stays on disk because nothing prunes history
automatically.

The two defects compound. The calldata makes each retained block large, and the
replay makes each new block re-read that size. A caller with a funded account can
hold the producer's commit path in ever-growing work and grow the ledger without a
meaningful fee.

Measured on this commit:

- 200 blocks, each a 1 wei transfer with 131,072 bytes of `data`: 200 wei total
  fee, 26,330,852 ledger bytes, 81,436 kB peak RSS. The submit call rose from
  4,453 us at block 1 to 255,334 us at block 200 (`57.3x`).
- 1,000 blocks, each a 1 wei transfer with empty `data`: 1,000 wei total fee,
  579,652 ledger bytes, 6,692 kB peak RSS. The submit call rose from 1,519 us at
  block 1 to 58,311 us at block 1000 (`38.4x`).
- Real HTTP run, 40 transfers with 130,000 bytes of `data`, `gas=0x1` and
  `gasPrice=0x1`: 40 wei total fee, 5,223,809 ledger bytes, 5,200,042 bytes on disk
  that match the `0xAA` marker (including all 5,200,000 payload bytes), request
  latency from 0.256155 s for nonce 0 to 2.859132 s for nonce 39.

## Affected code

- `crates/subhost-rpc/src/methods.rs:20`: the HTTP body cap is 256 KiB, so the
  hex-encoded calldata ceiling over RPC is just under the 128 KiB mempool cap.
- `crates/subhost-rpc/src/methods.rs:94-102`: `hex_data` decodes `data` or `input`
  with no size or gas relation.
- `crates/subhost-rpc/src/methods.rs:215-218`: `gas` and `gasLimit` are taken from
  the request, defaulting to 21,000. Value 1 is accepted.
- `crates/subhost-rpc/src/methods.rs:234`: `data` is copied into the transaction.
- `crates/subhost-rpc/src/methods.rs:393-406`: `eth_sendTransaction` builds that
  transaction, verifies the signature, and calls `state.submit_transaction`.
- `crates/subhost-mempool/src/lib.rs:21,30`: the only data limit is
  `max_tx_data_bytes = 128 * 1024`.
- `crates/subhost-mempool/src/lib.rs:114-141`: `admit` enforces that limit,
  `gas_limit > 0`, `gas_price >= 1`, and a representable fee. It never relates
  `data.len()` to gas.
- `crates/subhost-state/src/lib.rs:197-240`: `apply_transaction` computes
  `fee = gas_price * gas_limit` and its `Transfer` branch ignores `tx.data`.
- `crates/subhost-rpc/src/block.rs:61-78`: `submit_transaction` seals a block on
  every accepted submission.
- `crates/subhost-rpc/src/block.rs:222-230`: `seal_block` clones the retained block
  list and calls `persist` before swapping state in.
- `crates/subhost-rpc/src/block.rs:389-405`: `persist` passes
  `blocks: blocks.to_vec()`, the whole retained history.
- `crates/subhost-storage/src/segmented.rs:616-658`: `persist` calls
  `validate_snapshot_from(snapshot, chain_id, base_height)` before any write.
- `crates/subhost-storage/src/lib.rs:336-348`: `validate_snapshot_from` calls
  `validate_chain_from` over all blocks and receipts.
- `crates/subhost-storage/src/lib.rs:398-440`: `validate_chain_from` re-computes
  each transaction hash and calls `tx.verify_signature()` for every retained
  transaction. `Transaction::signing_payload` serializes the full struct, including
  `data`, so the replay cost scales with calldata bytes.
- `crates/subhost-rpc/src/state.rs:157-158`: the restored retained history is held
  in memory and `base_height` is whatever the manifest recorded.
- `crates/subhost-cli/src/main.rs:993-994`: pruning is only reachable from the
  explicit `subhost db prune` subcommand. No node path calls it.

## Root cause

### 1. Calldata has a size cap but no price

The RPC edge parses `data` and the mempool checks it against a 128 KiB ceiling:

```rust
// crates/subhost-mempool/src/lib.rs:114-141
pub fn admit(&mut self, tx: Transaction) -> Result<Admission, MempoolError> {
    if tx.data.len() > self.config.max_tx_data_bytes {
        return Err(MempoolError::DataTooLarge { ... });
    }
    ...
    if tx.gas_limit == 0 {
        return Err(MempoolError::InvalidTransaction("gas limit must be > 0".into()));
    }
    if tx.gas_price < self.config.min_gas_price {
        return Err(MempoolError::GasPriceTooLow { ... });
    }
    if tx.total_cost().is_none() {
        return Err(MempoolError::InvalidTransaction(
            "gas_price * gas_limit + value overflows".into(),
        ));
    }
```

`gas_limit` is caller-supplied and only zero is rejected. Execution charges
`gas_price * gas_limit` and never touches the bytes:

```rust
// crates/subhost-state/src/lib.rs:220-237
let fee = tx
    .fee()
    .ok_or_else(|| StateError::InvalidTransaction("transaction fee overflows".into()))?;

match tx.tx_type {
    TransactionType::Transfer => {
        let to = tx.to.ok_or_else(|| { ... })?;
        let total = tx.total_cost().ok_or_else(|| { ... })?;
        self.ensure_balance(&tx.from, total)?;
        self.apply_transfer(tx.from, to, tx.value)?;
        self.debit(tx.from, fee)?;
    }
    other => return Err(StateError::UnsupportedTransactionType(other)),
}
```

So `gas_price = 1, gas_limit = 1` is a valid 1 wei transaction carrying 128 KiB.
There is no intrinsic gas cost, no per-byte gas, and no minimum gas limit for a
non-empty payload. On a chain that has no contract execution yet, the payload is
not even used.

### 2. Every append replays the entire retained history

`seal_block` builds the next block, then persists the full list:

```rust
// crates/subhost-rpc/src/block.rs:222-230
let mut next_blocks = read(&self.blocks).clone();
next_blocks.push(block.clone());
...
self.persist(&candidate_state, &next_blocks, &next_receipts)?;
```

```rust
// crates/subhost-rpc/src/block.rs:389-405
pub(crate) fn persist(&self, state: &State, blocks: &[Block], receipts: &HashMap<Hash, Receipt>) -> Result<(), RpcError> {
    ...
    self.store.persist(&LedgerSnapshot {
        chain_id: self.chain_id,
        state: state.snapshot(),
        blocks: blocks.to_vec(),
        receipts: ordered,
    })?;
```

The segmented store validates that whole snapshot before writing the append:

```rust
// crates/subhost-storage/src/segmented.rs:616-630
pub(crate) fn persist(...) -> Result<(), StorageError> {
    let previous = read_previous(chain_id, data_dir)?;
    let base_height = previous.as_ref().map_or(0, |previous| previous.base_height);
    let new_height = snapshot.blocks.last().map_or(0, |block| block.header.height);
    crate::validate_snapshot_from(snapshot, chain_id, base_height)?;
```

`validate_chain_from` then loops over every retained block and transaction,
recomputing hashes, rebuilding receipt commitments, and verifying signatures. That
is the cost that grows linearly with history on every single append.

`SECURITY.md:122-127` documents a full replay "on load". The append-time replay is
not documented, and it is what turns a linear storage cost into quadratic work.

### 3. Nothing prunes automatically

`base_height` stays 0 unless an operator runs `subhost db prune`. Every block is
retained, loaded into memory on start, and re-read on every subsequent persist. The
attacker's history is permanent until a human intervenes.

## Attack path

1. The attacker needs RPC access and a funded account. On the public testnet the
   RPC requires a bearer token, so this is an authorized user, which is why the
   CVSS privilege metric is Low rather than None. A single faucet drip is
   1,000,000,000,000,000,000 wei by default (`crates/subhost-faucet/src/lib.rs:65`),
   so the fee is not a budget constraint at 1 wei per transaction.
2. The attacker submits `eth_sendTransaction` with a valid signature, `to` set to
   any address, `value = 0`, `gasPrice = 0x1`, `gas = 0x1`, and `data` just under
   the HTTP body cap. The 130,000-byte payload used in the recorded run fits under
   the 262,144-byte request-body limit. The library driver submits through
   `RpcState::submit_transaction`, which accepts the full 131,072-byte mempool cap.
3. The node admits the transaction, seals block N, and calls `persist`. Before
   writing, it replays blocks 1 through N.
4. The attacker repeats with `nonce + 1`. Each accepted request adds a block and
   its calldata to disk, and makes the next request more expensive.
5. A restart replays the whole ledger and restores every retained block into
   memory, so recovery time also grows with the attacker's history. Without a
   manual prune, the cost never falls.

Nothing in the path requires a malformed request. Every field is valid, the
signature verifies, the nonce is correct, and the block passes all commitments.

## Reproduction

Both reproducers are in `poc/`. They build as a standalone crate with an empty
`[workspace]` table and path dependencies on the repository crates.

### Library level

```sh
./security-audit/2026-11-19-rpc-work-amplification/poc/reproduce.sh
```

The driver calls `RpcState::with_data_dir`, seeds one account, and submits
successive 1 wei transfers. `reproduce.sh` runs two cases.

Case A, empty calldata (`evidence/quadratic-empty-1000.txt`):

```text
block      1  submit       1519 us
block      5  submit       1029 us
block    251  submit      15083 us
block    501  submit      29492 us
block    751  submit      42729 us
block   1000  submit      58311 us

blocks=1000 data_len=0 first_us=1519 last_us=58311 growth=38.4x
attacker_fee_per_block_wei=1 total_fee_wei=1000
ledger_bytes=579652 height=1000
VmHWM:	    6692 kB
```

Case B, 131,072 bytes of calldata (`evidence/calldata-200.txt`):

```text
block      1  submit       4453 us
block      2  submit       5500 us
block      3  submit       6860 us
block      4  submit       7926 us
block      5  submit       9335 us
block    200  submit     255334 us

blocks=200 data_len=131072 first_us=4453 last_us=255334 growth=57.3x
attacker_fee_per_block_wei=1 total_fee_wei=200
ledger_bytes=26330852 height=200
VmHWM:	   81436 kB
VmRSS:	   55636 kB
```

Case A isolates the replay: the payload is empty, yet block 1000 costs 38.4 times
block 1. Case B adds the size amplification: 200 wei produced 26.3 MB of ledger
and 81 MB of peak RSS, and the last submission cost 255 ms.

### Real node over HTTP

```sh
./security-audit/2026-11-19-rpc-work-amplification/poc/reproduce-http.sh
```

`reproduce-http.sh` builds the CLI, generates a genesis for the address that
`emit` derives from a fixed seed, starts `subhost node` on loopback, and sends 40
signed requests. Each request carries 130,000 bytes of `0xAA`, `gas=0x1`,
`gasPrice=0x1`, `value=0x0`.

Observed output (`evidence/http-40.txt`):

```text
HTTP reproduction: 40 transfers, 130000 calldata bytes each, gas=0x1, gasPrice=0x1
nonce=0  ... seconds=0.256155
nonce=5  ... seconds=0.606607
nonce=10 ... seconds=0.905268
nonce=15 ... seconds=1.256176
nonce=20 ... seconds=1.628093
nonce=25 ... seconds=1.900236
nonce=30 ... seconds=2.282423
nonce=35 ... seconds=2.572878
nonce=39 ... seconds=2.859132
ledger_bytes=5223809
byte_0xaa_in_ledger=5200042
attacker_fee_total_wei=40
```

The 40 requests cost 40 wei and added 5,223,809 bytes to the ledger. The
transaction hashes in the shortened lines above are the same full hashes recorded
in the evidence file. `byte_0xaa_in_ledger` is 5,200,042. The attacker payload is
40 * 130,000 = 5,200,000 bytes, so every payload byte reached the disk; the extra
42 matches are incidental `0xAA` bytes in other binary fields of the ledger.

Latency rose from 0.256155 s at nonce 0 to 2.859132 s at nonce 39, an 11.2x
increase across 40 accepted transactions. The growth is monotonic at each sampled
point in the evidence file.

The byte count is direct evidence that the payload is written to disk, not merely
kept in memory. `byte_0xaa_in_ledger` counts the attacker's marker byte inside the
ledger directory. The evidence file also contains the two cached build lines and a
registry cache permission warning from the build environment; the reproduction
itself exited 0, as the final line records.

## Impact

- Availability of the write path. Every accepted transaction does O(history) work
  while holding `commit_lock` (`crates/subhost-rpc/src/block.rs:62`), so concurrent
  submissions queue behind the attacker's persist. Submission latency grows without
  bound: 38.4x over 1,000 empty blocks and 57.3x over 200 calldata blocks in the
  library run, and 11.2x over 40 calldata blocks over HTTP. A single authorized
  caller can keep the producer's commit path busy for arbitrarily long per request.
  On a single-producer testnet the producer is the block producer, so a monopolized
  commit path stops new blocks as well as new submissions.
- Permanent storage growth. Each calldata-carrying transaction adds about 130 KB to
  the ledger and no automatic prune removes it. From the measured 26,330,852 bytes
  for 200 wei, the arithmetic rate is 131,654 bytes per wei. One thousand wei is
  about 131.7 MB and one million wei is about 131.7 GB. Those figures are linear
  arithmetic from the measurement, not measured end states. The fee is not a
  barrier because a faucet drip is 1e18 wei.
- Memory growth. The retained history is held in memory and restored on start.
  The 200-block calldata run reached 81,436 kB peak RSS, versus 6,692 kB for
  1,000 empty blocks. Memory tracks retained calldata, which the attacker chooses.
- Restart cost. Startup replays the full ledger. After a large history, a restart
  is slow and memory-hungry, and the only remedy is a manual
  `subhost db prune`.
- Recovery. There is no automatic mitigation and no per-sender or per-block
  calldata budget. An operator must detect the growth, stop the node, and prune or
  repair the ledger.

No confidentiality or integrity impact. Signatures, nonces, balances, and block
commitments are all enforced correctly; the defect is that valid work is free.

## CVSS v4 justification

| Metric | Value | Reason |
| --- | --- | --- |
| AV | Network | The path is `eth_sendTransaction` over the JSON-RPC listener. |
| AC | Low | No race, no special state, and no malformed input. The same request works every time. |
| AT | None | No non-default configuration is required. Auth is checked as privilege, not as an attack requirement. |
| PR | Low | The public RPC requires a bearer token, so the caller is an authorized user. |
| UI | None | No victim action beyond running the node. |
| VC | None | No data is exposed. |
| VI | None | No ledger or state value is forged. |
| VA | High | Submission latency grows without bound, the commit path is monopolized, and disk can be exhausted until the node cannot persist blocks. |
| SC | None | The score does not claim impact on other components. |
| SI | None | No downstream integrity impact. |
| SA | None | No downstream availability impact claimed. |

Base score: `7.1` (High). If the deployment has no token and is reachable only on
loopback, the vector becomes
`CVSS:4.0/AV:L/AC:L/AT:N/PR:L/UI:N/VC:N/VI:N/VA:H/SC:N/SI:N/SA:N` = `6.8`
(Medium). If the RPC token is treated as high privilege, the network vector is
`CVSS:4.0/AV:N/AC:L/AT:N/PR:H/UI:N/VC:N/VI:N/VA:H/SC:N/SI:N/SA:N` = `6.9`
(Medium). The reported score uses the deployed testnet posture: remote access with
a token held by ordinary users.

The `PR:L` choice is deliberate. An unauthenticated remote attacker cannot reach
the method: `crates/subhost-rpc/src/security.rs:176-192` refuses a non-loopback
bind without both `allow_public` and a token. The finding does not depend on that
policy being absent.

## Fix

Three changes, in the order that removes the most risk.

1. Price calldata and enforce intrinsic gas, or refuse unused calldata. This chain
   has no contract execution, so the simplest correct fix is to reject a non-empty
   `data` field on a `Transfer` at both the RPC edge and `Mempool::admit`. If the
   field is meant to be forward compatible, charge for it:

   ```rust
   const TX_BASE_GAS: Gas = 21_000;
   const TX_DATA_GAS_PER_BYTE: Gas = 16;

   fn intrinsic_gas(tx: &Transaction) -> Option<Gas> {
       let bytes = u64::try_from(tx.data.len()).ok()?;
       TX_BASE_GAS.checked_add(bytes.checked_mul(TX_DATA_GAS_PER_BYTE)?)
   }
   ```

   Require `tx.gas_limit >= intrinsic_gas(tx)` in `Mempool::admit` and in
   `State::apply_transaction`, so the debited fee is at least
   `gas_price * intrinsic_gas(tx)`. A 130 KB payload then needs at least about
   2.1M gas instead of 1. Also add a per-block calldata cap so one block cannot
   exceed a fixed byte budget.

   Pricing alone does not bound disk on this chain, because `min_gas_price` is
   1 wei (`crates/subhost-mempool/src/lib.rs:29`). The retention bound in fix 3 is
   what puts a ceiling on stored bytes; the gas charge only removes the "1 wei for
   128 KiB" ratio and makes the cost visible to a future fee market.

2. Validate only the appended blocks on persist. `check_append_only`
   (`crates/subhost-storage/src/segmented.rs:708-733`) already pins the snapshot to
   the committed head by height and hash, and the prior history was validated when
   it was committed and again on load. `segmented::persist` should validate the new
   block's commitments and its state transition against the committed state, not
   re-run `validate_snapshot_from` over the whole list. Keep the full
   `validate_chain` replay in `LedgerStore::load`, where a fresh process cannot
   trust memory.

3. Bound retained history automatically. Add a height or byte retention policy to
   the node, or refuse to accept a block that would exceed a configured ledger
   size. Pruning must not depend on an operator remembering `subhost db prune`.

Regression tests to add:

- `eth_sendTransaction` with a non-empty `data` and `gas = 0x1` returns an error
  after fix 1. The same transaction is rejected by `Mempool::admit`.
- A 131,072-byte payload with a gas limit below `intrinsic_gas` is rejected, and a
  payload at the limit is accepted with the fee debited.
- Appending one block to a 1,000-block store does not re-verify the first 999
  blocks. Assert this with an instrumented validator counter, not with wall-clock
  timing. The load path must still reject a corrupted signature in the retained
  history.
- A ledger that exceeds the configured retention bound is pruned without an
  operator command.

## Scope and false-positive control

Reviewed and deliberately not reported:

- `SUBHOST-2026-001` (block gas cap bypass) and `SUBHOST-2026-002` (wallet scrypt
  cost) are separate findings and are not duplicated here. This finding does not
  depend on the block gas cap being bypassed: `gas_limit = 1` is inside the cap.
- Documented known posture is excluded: opt-in auth, no TLS, loopback default
  bind, unauthenticated metrics and faucet, WebSocket auth only at upgrade, and the
  accepted gossip and block-withholding gap. None of these is the issue.
- The 30M block gas limit is enforced by `select_transactions` and by the ledger
  validator. The issue is that calldata is not priced, not that the gas limit is
  ignored.
- The zero-balance mempool-fill candidate was discarded. `submit_transaction`
  returns `TransactionRejected` and drops the candidate pool when the submitted
  transaction is not in the sealed receipts, so an unfunded sender cannot park
  transactions.
- The faucet per-address Sybil drain is by design and not reachable from this
  surface.
- The token-file permission check (`SecretToken::from_file` does not require
  `0600`) is a hardening gap with no demonstrated impact, so it is not reported.
- The non-strict ed25519 `verify` call, the block-tag-ignoring
  `eth_getBalance` and `eth_getTransactionCount`, and the `safe` and `finalized`
  tags mapped to head were reviewed and have no theft or integrity impact.
- The per-connection rate limit does not mitigate. It is a token bucket per
  connection (`crates/subhost-rpc/src/security.rs:197-217`), and once history is
  large the attack needs well under one request per second. A limit high enough
  for normal interactive use is far above the attack rate.
- The O(history) persist alone would be a performance defect, not a
  vulnerability. It becomes an availability issue because 1 wei buys about 130 KB
  and nothing prunes, so the attacker controls the growth.

The reported behavior is reachable from the shipped binary on the stated commit.
Both the library driver and the real HTTP server accepted the transactions, and
the ledger on disk contains the attacker's bytes.

## Re-verification 2026-11-19 (PR #50 head `ab3e624`, merged main `fa87215`)

The unmodified library driver (`poc/src/main.rs`) was run against a baseline
worktree at `982d3e6`, the PR #50 head `ab3e624`, and current main `fa87215`
(which merges PR #50). Release profile, same machine. `reproduce.sh` hardcodes
`poc/target`, so the binaries were run from an explicit `CARGO_TARGET_DIR`.

Growth ratios (first run / durable rerun):

| Case | baseline 982d3e6 | PR50 ab3e624 | main fa87215 |
| --- | --- | --- | --- |
| A: 1000 blocks, empty data | 36.3x / 17.0x | 1.1x / 2.0x | 2.7x |
| B: 200 blocks, 131,072 B data | 45.8x / 57.7x | 7.8x / 6.6x | 8.7x |
| A ledger_bytes | 579,652 | 579,652 | 579,652 |
| B ledger_bytes | 26,330,852 | 26,330,852 | 26,330,852 |

Absolute latencies shift with machine load; the ratios and byte counts are the
stable signal and agree across runs.

- Fix 2 confirmed: `segmented::persist` validates only the appended suffix when
  the verified head matches the committed manifest, falling back to the full
  `validate_snapshot_from` otherwise. Case A growth collapses from 17-36x to
  1-3x.
- Fix 1 and fix 3 are NOT in the merged slice: `ledger_bytes` is byte-identical
  to baseline, so 1 wei still buys 131,072 bytes and nothing prunes
  automatically. The finding stays open on those two axes.
- Residual Case B growth (6.6-8.7x) tracks retained payload bytes, consistent
  with the RPC seal/persist path cloning the whole retained block list per
  block (`crates/subhost-rpc/src/block.rs:548`, `:584` at `ab3e624`), which the
  storage slice does not change. This is an inference from source and
  measurement shape, not an instrumented counter.

Evidence: `evidence/retest-baseline-982d3e6.log`,
`evidence/retest-pr50-ab3e624.log`, `evidence/retest-main-fa87215.log`.
Discussion: `docs/agents/replies/Q-024-track-b.md`.

## Delivery Gate

- PASS: Affected functions and exact lines identified.
- PASS: Root cause reproduced on commit `fd0fc7e`.
- PASS: Two independent measurements: in-process `RpcState` and the real node over
  JSON-RPC.
- PASS: Control case with empty calldata isolates the replay growth from the size
  amplification.
- PASS: Calldata is proven on disk by counting the `0xAA` marker in the ledger
  directory.
- PASS: CVSS v4 vector and score computed with the `cvss` 3.6 library, with the
  privilege and local-access alternates stated.
- PASS: Impact is availability and storage, with no unsupported theft claim.
- PASS: No known-posture item used as a finding.
- PASS: No duplicate of `SUBHOST-2026-001` or `SUBHOST-2026-002`.
- PASS: Every measured numeric claim cites an evidence file; every code constant
  cites a source line.
- PASS: No em dashes and no fabricated data.
- FAIL: None.

## Artifacts

- `poc/src/main.rs`: library driver for both measurement cases.
- `poc/src/bin/emit.rs`: signs one `eth_sendTransaction` body and prints the sender.
- `poc/reproduce.sh`: runs the empty-calldata and calldata cases.
- `poc/reproduce-http.sh`: starts the real node and sends the HTTP requests.
- `evidence/quadratic-empty-1000.txt`: 1,000 empty-data blocks.
- `evidence/calldata-200.txt`: 200 blocks with 131,072 bytes of calldata.
- `evidence/http-40.txt`: real HTTP run with 130,000-byte payloads.
- `evidence/source-snippets.txt`: the cited source excerpts and the documented
  posture they were checked against.
- `evidence/retest-baseline-982d3e6.log`, `evidence/retest-pr50-ab3e624.log`,
  `evidence/retest-main-fa87215.log`: 2026-11-19 re-verification runs (see
  Re-verification above).