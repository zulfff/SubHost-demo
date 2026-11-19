# Architecture

This document explains how a running Subhost node turns a signed transaction into a
committed block, what the ledger actually guarantees today, and which crate owns each
decision. It describes the code as it is, not the target state in `ROADMAP.md`.

## 1. Process shape

A node is one process assembled in `subhost-node`:

- `RpcState` holds the chain ID, the mempool, the account state, the block history,
  the receipt index, the ledger store, and a commit lock. Every field is shared
  behind an `Arc`, so cloning the state is cheap and all clones see one ledger.
- `RpcServer` serves the JSON-RPC surface over HTTP and WebSocket, optionally
  behind a bearer-token and per-connection rate-limit HTTP middleware. Both are
  off unless the operator configures them.
- A prepare/commit driver for a genesis validator set, verifying distinct `n - f` phase votes
  before commit. Observers hold no signing key. A node-local journal preserves
  the open proposal/votes and latest QC before votes are published.
- The optional metrics exporter, the optional libp2p gossip task, and a five-second
  observer that copies block height and pending count into metrics.
- `subhost-cli` parses operator flags, writes genesis files, and calls
  `Node::bootstrap`. It contains no ledger logic.

`Node::bootstrap` loads `genesis.json` when present, opens the ledger, and applies
genesis allocations only when no ledger exists yet. A restarted node
restores the ledger and logs the restored height instead of re-applying allocations.
The mempool is not persisted, so a restart loses pool entries. Transactions in
a journalled open proposal survive with their voting obligation. Bootstrap
reconciles certified commit intents with the ledger and refuses missing, corrupt
or incompatible validator voting state. The journal does not authenticate
historical QCs in the block ledger.

A node started with `--validator` refuses to start without a genesis file that
contains at least one initial validator. A node without that flag may observe validator genesis. Explicit
`--single-writer` permits an unsigned demo with no genesis file, using the fallback
chain ID and an empty allocation set.

## 2. Block lifecycle

### 2.1 Submit

`eth_sendTransaction` is the only way to submit a transaction. The method requires
`publicKey` and `signature`, derives the sender from the public key, rejects a
mismatch between that address and `from`, and verifies the ed25519 signature over
`Transaction::signing_payload`. It also enforces the configured chain ID, requires a
destination, and accepts only `Transfer`. The default gas limit is 21,000.

`RpcState::submit_transaction` is the Rust entry point behind that method. It assumes
its caller already verified the signature; `subhost-state` documents the same
boundary. Signature enforcement therefore lives at the RPC edge, while
`accept_block` and ledger replay verify every transaction again.

### 2.2 Admit to the pool

Submission works on a clone of the mempool. `Mempool::admit` enforces, in order:

- calldata at most 128 KiB, gas limit above zero, gas price at or above the minimum;
- no overflow in `gas_price * gas_limit + value`;
- replace-by-fee inside one `(sender, nonce)` slot, with equal or lower price
  refused;
- at most 64 pending transactions per sender;
- at most 10,000 pending transactions overall, evicting the lowest gas-price
  transaction when full and the highest nonce among equal prices, so the remaining
  per-sender sequence stays contiguous from the bottom.

With validators configured, an executable admission replaces the pool and returns
its hash before a block or receipt exists. The producer builds a proposal and
waits for quorum. Without validators, the pool is replaced only after the
synchronous block write succeeds. The mempool remains volatile in both modes.

### 2.3 Select

The producer sorts pending senders by address, then walks each sender's
nonce-contiguous prefix starting at that sender's current state nonce. It stops at
the first transaction that does not fit in the remaining 30,000,000 block gas or is
rejected by the state transition, so a block never skips a nonce for a sender.
Selection runs against a clone of the account state; the live state is untouched
until the commit succeeds.

### 2.4 Execute and seal

`build_block` builds and signs a proposal without writing the ledger, changing
balances or removing pool entries. Validator nodes use this path; demo chains
without validators use `seal_block` synchronously on submission. Both forms use:

- height is the tip height plus one, or 1 when the ledger is empty; there is no
  genesis block;
- `parent_hash` is the tip hash, or the zero hash for height 1;
- `state_root` is the Merkle root of the candidate state after execution;
- `tx_root` commits to the selected transaction hashes in block order;
- `receipt_root` commits to one receipt commitment per transaction, where the
  commitment binds the transaction hash, height, index, gas limit, and cumulative
  gas;
- `gas_used` is the sum of the declared gas limits, and `gas_limit` is the fixed
  block ceiling;
- `validator` is the configured producer address and `signatures` holds one BLS
  signature over `Block::signing_payload()`. A node without a configured authority
  keeps the single-node form: `validator` is `Address::ZERO` and `signatures` is
  empty. See section 3.

A producer never seals an empty block: if nothing is executable, submission fails
instead of writing a block.

### 2.5 Persist, then publish

Durability comes before visibility. The node builds the next block list and receipt
map in memory, appends the new block record to the ledger, writes a fresh state
snapshot when one is due (every `SNAPSHOT_INTERVAL_BLOCKS`), commits the manifest,
and only then swaps the new state, blocks, and receipts into place and stores the
new height. If persistence fails, none of the swaps happen and no block event is
emitted.

On success the node removes the mined transactions from the pool. Demo sealing
emits `BlockEvent::Sealed`; QC commits emit `BlockEvent::Accepted`. The validator
network loop retries proposals, verified votes and the latest certified block.
A failed QC write keeps the tally ready for retry while both committed tips stay
unchanged. See [consensus wiring](consensus.md).

### 2.6 Accept a peer block

`RpcState::commit_qc` requires a final Commit-domain certificate binding genesis
context, block hash and view, from `n - f` distinct registered validators. View
advances independently from height and must exceed the verified committed view.
Prepare certificates lock a block but cannot append it. Leader envelopes and
timeout reports are separately authenticated. `accept_block` refuses validator blocks
without a certificate and remains available for demo chains. Both commit paths
re-validate end to end before touching anything:

1. it must extend the local tip exactly: height `tip + 1`, parent equal to the tip
   hash, configured chain ID, the fixed block gas limit, and at least one
   transaction;
2. every transaction must match the chain ID, carry a valid signature, be unique
   inside the block, and fit in the declared block gas;
3. each transaction is applied to a clone of the account state;
4. declared gas, `tx_root`, `receipt_root`, and the resulting `state_root` must all
   match what the node computed itself;
5. receipts are derived locally from the block, never taken from the wire;
6. the ledger is persisted, then the state, blocks, receipts, and height are swapped
   in, and `BlockEvent::Accepted` is emitted.

Any failure rejects the whole block and leaves the ledger unchanged.

### 2.7 Ordering buffer

On demo chains, peer blocks that arrive before their parent are buffered by height. A block at or
below the next expected height is discarded as stale, and anything more than 64
blocks ahead of the local tip is refused so a peer cannot force unbounded buffering.
The first block seen for a height wins; a duplicate cannot replace it. Whenever the
local tip advances, buffered successors are applied in order. A rejected buffered
block is logged and dropped, because `accept_block` is deterministic and a retry
cannot succeed.

## 3. Who may produce a block

Today: JSON-RPC is unauthenticated by default, so any client that can reach the
port can submit a transaction; with the optional bearer-token policy enabled, only
clients with the token can. Any node whose genesis carries a validator set rejects
blocks that do not come from one of those validators or lack a valid quorum certificate. On a genesis with no validators,
the single-node path is unchanged and unsigned blocks are accepted.

- Bootstrap builds a `ValidatorRegistry` from `genesis.initial_validators` on every
  node that has a validator set, so a follower verifies peer blocks without holding
  a signing key. If any entry has a bad public key, a missing or invalid proof of
  possession, zero power, or a duplicate address, start-up fails before the ledger is
  opened.
- A node started with `--validator` also reads `<data_dir>/producer.key` (one
  hex-encoded canonical 32-byte BLS scalar, mode `0600`). It derives the public key,
  looks for the genesis validator whose `public_key` matches, and refuses to start if
  there are zero or several matches. A file that group or others can read is refused.
  That lookup is why there is no producer address argument: a key file cannot be
  paired with an address it does not match.
- A node with a `BlockAuthority` sets `header.validator` to the resolved address and
  attaches exactly one BLS signature over `Block::signing_payload()`. Construction
  also checks that the secret belongs to the registered public key.
- Proposal validation and `commit_qc` on a node with a registry require `header.validator` to be
  registered and exactly one signature from that validator over the payload, checked
  before the state transition. A missing, extra, foreign, malformed, or stale
  signature is rejected with `RpcError::UnauthorizedProducer`.
- A node without a registry, which means a genesis with no validators, does not check
  block signatures. That is the development path, and it is not a network guarantee.
- JSON-RPC authentication is opt-in: without a token, any client that can reach the
  port can submit signed transactions. `--rpc-rate-limit` is per connection, not per
  IP. The listener binds loopback by default and refuses a non-loopback bind unless
  `--rpc-allow-public` and a token are both set. There is no in-process TLS; use a
  TLS-terminating reverse proxy for public deployment.


`subhost init --validator` closes the bootstrap path: it derives the public key from
the producer key file, signs the proof of possession, and stores it in genesis. A
genesis assembled on another machine can carry a supplied proof instead. Every
validator entry therefore boots through the same registry check, whether it was
written by `init` or by hand.

## 4. What the ledger guarantees

Durability and integrity:

- `ledger/manifest.json` is the commit point: chain ID, head height and hash, the
  state snapshot, and every segment with its committed record count and byte length;
- history is an append-only `ledger/segments/NNNNNN.log`, with each record framed as
  `magic || version || length || BLAKE3 || payload` and fsynced before the manifest
  that references it;
- account state is `ledger/snapshot-<height>.bin`, written to a temporary file,
  fsynced, atomically renamed, and only then referenced by the manifest. A snapshot
  is written on the first persist and then every `SNAPSHOT_INTERVAL_BLOCKS` (1024)
  blocks, so the committed snapshot height may lag the head;
- the manifest is replaced the same way (temporary file, fsync, atomic rename,
  directory fsync), so a persist either moves the head or leaves the previous one;
- the previous snapshot is kept until the following commit, so a manifest that was
  never replaced still points at live data;
- bytes past the manifest's committed segment length are an interrupted append: they
  are never read and are truncated before the next append;
- a segment rolls to a new file at 64 MiB instead of refusing history, and one record
  is capped at 16 MiB;
- `ledger/manifest.json` also records `base_height`, the number of blocks pruning
  removed. `subhost db prune --keep-segments N` drops the oldest segment files, keeps
  the newest snapshot as the replay anchor, and refuses (without writing) when the
  retained history would no longer contain the block at the snapshot's own height;
- every read is validated before it is trusted: size bounds, magic, versions,
  checksums, manifest consistency (segment file names, record counts, snapshot name
  and height), and chain binding;
- the snapshot's state root must equal the `state_root` of the block at the
  snapshot's own height, so a snapshot cannot be replayed onto a different history;
- blocks after the snapshot are executed again with `State::apply_transaction`, then
  a replay checks contiguous heights over the retained history (from
  `base_height + 1`), parent links, chain ID, gas totals, unique blocks and
  transactions, every transaction signature, both commitments per block, exactly one
  `Success` receipt per transaction, and that the restored state root equals the
  newest block's `state_root`. The first retained block's parent link cannot be
  checked after a prune; the snapshot's root anchor is what ties the suffix to the
  removed prefix;
- a tampered, truncated, or half-written ledger is rejected, not silently loaded. A
  version 2 `node-state.bin` is still read; the first persist migrates it to segments
  and keeps it as `node-state.bin.v2-backup`;
- a persist validates before writing. When the store's verified-head cache matches
  the committed manifest, only the appended suffix is validated: its link to the
  head, every commitment, and a full `State::apply_transaction` replay of the suffix
  on the cached head state, so a transaction the committed state cannot apply (a
  replay, a skipped nonce) is refused even when its commitments are honest. The
  offered state root must equal the replayed root. Otherwise (no cache yet, a
  legacy file, a pruned or rolled-back manifest, an unchanged history) the whole
  offered history is validated as before. The cache is populated by a successful
  load and updated only after a manifest commit succeeds. An ephemeral store has
  no manifest, so its cache *is* the committed record: it anchors the suffix the
  same way, and an unchanged history replays zero blocks against it;
- a fee-enabled (version 2) history is re-executed and settled in **both** paths,
  not only the suffix: the full path replays from the committed snapshot anchor,
  derives each block's fee context from its parent header (`initial_context` at
  height 1), checks the recorded `base_fee_per_gas` against the derived one,
  aggregates with `settle_block_fees`, verifies per-block conservation against
  the fees actually debited, and credits the producer once at end of block before
  comparing roots. The policy is bound fail-closed through `manifest.json`'s
  `fee_policy` (Q-030): no recorded policy with v2 blocks, a differing supplied
  policy, a policy on a legacy ledger, or a persist dropping a recorded policy
  are all refused; `load` with no supplied policy uses the recorded one. A v2
  history with no committed anchor is refused, naming the fix: persist the
  genesis state first. Legacy v1 histories are unchanged: no execution in the
  full path, no settlement, no credit;

Determinism:

- `state_root` is a Merkle root over address-sorted account leaves, so insertion
  order does not affect it;
- the chain ID is mixed into every leaf, binding the root to one chain;
- receipts are grouped per block and sorted by transaction index before a record is
  written, so the segment is byte-stable;
- selection visits senders in address order, so the same pool and state produce the
  same block.

Execution:

- only `Transfer` executes; every other transaction type is rejected explicitly;
- a transaction must match the chain ID, carry the exact next nonce, and afford value
  plus fee before any balance moves;
- fees are debited from the sender and are not credited to any account, so they are
  burned; there is no block reward and no fee recipient;
- gas is the declared limit, not measured execution, because there is no execution
  engine to measure.

What the ledger does not guarantee:

- no finality and no fork choice: `accept_block` only appends one block to the local
  tip, and a competing branch is rejected because it does not extend that tip;
- no block sync for the legacy single-writer demo: a gap there is only filled by
  blocks arriving through gossip within the 64-block window. Validator-set nodes
  do fill a longer gap from a peer's retained certified history (S3.3, ADR-007);
- no reorg: `persist` refuses a snapshot that does not extend the committed head
  (`StorageError::NonAppendPersist`), so a competing branch can only replace the
  chain by deleting the ledger directory and replaying from genesis;
- no pruning and no bounded start-up I/O: a fresh snapshot is written every 1024
  blocks and start-up executes at most that many blocks on top of it, but every
  segment is still read and scanned, and old segments are never dropped, so
  start-up I/O and disk use still grow with history until S4.3–S4.4;
- pruning is offline only: `subhost db prune` runs with the node stopped, keeps the
  newest N segments, and needs the snapshot to stay inside the retained history. A
  pruned node is not an archive node: `eth_getBlockByNumber` returns `null` below its
  base height, `eth_getTransactionByHash` and `eth_getTransactionReceipt` return
  `null` for a pruned transaction, `earliest` is the oldest retained block, and
  there is no backfill below the retained window (S3.3 sync serves only the
  newest certified 4096 items, not pruned history);
- no failed receipts: a transaction that fails to execute is not included, so every
  persisted receipt is `Success` with empty logs and no contract address;
- JSON-RPC authentication is opt-in and the metrics endpoint has none: without
  `--rpc-auth-token-file` any client that can reach the port can submit
  transactions, and `--rpc-rate-limit` is per connection rather than per IP
  because jsonrpsee 0.24 does not expose the peer address to middleware. WebSocket
  tokens are checked only at the upgrade. There is no in-process TLS, so a public
  deployment needs a TLS-terminating reverse proxy. Producer authorization on
  gossip is enforced by nodes whose genesis carries a validator set; a genesis
  without validators accepts unsigned blocks, so that guarantee is not network-wide.
  The gossip transport encrypts and authenticates peer connections, but any peer
  may publish a block proposal.

## 5. Component ownership

| Crate | Owns | Does not own |
| --- | --- | --- |
| `subhost-core` | Types, canonical encoding, hashing, the block signing payload, genesis document and its structural validation | Ledger writes, execution, transport |
| `subhost-crypto` | BLS12-381 and ed25519 primitives, X25519, ChaCha20-Poly1305 | Key storage, policy, when to sign |
| `subhost-state` | Account state, nonce and balance rules, the state root, inclusion proofs | Signatures, block assembly, durability |
| `subhost-mempool` | Admission policy, per-sender ordering, replacement, eviction | Execution, persistence |
| `subhost-storage` | On-disk format, atomic write, load validation, receipt and block commitments | Deciding which block to write |
| `subhost-rpc` | Block selection, sealing, acceptance, the JSON-RPC surface, submission signature check | Consensus, peer identity, genesis policy |
| `subhost-consensus` | Validator registry, prepare/commit driver, authenticated leader/timeouts, prepared locks, votes/certificates, DAG, slashing | Ledger writes, execution, transport |
| `subhost-node` | Node assembly, genesis loading, consensus/transport orchestration, demo ordering buffer, shutdown | Ledger format, block validity |
| `subhost-network` | libp2p transport, framing, message size limits | Message meaning |
| `subhost-cli` | Operator flags, genesis file authoring, wallet and transaction commands | Ledger logic |
| `subhost-metrics`, `subhost-telemetry` | Counters and gauges; one structured logging setup | State transitions, tracing spans |
| `subhost-faucet` | Devnet funding over RPC | Consensus, block production |
| `subhost-explorer` | Read-only block and transaction views over RPC | Writing to the chain |
| `subhost-ibc`, `subhost-wallet`, `subhost-bench` | Light client, encrypted keystore, throughput harness | The node block path |

Decision boundary rule: a crate may validate its own inputs, but only
`subhost-rpc::block` decides whether a block enters the local ledger, and only
`subhost-storage` decides whether a byte sequence is a valid ledger file.

## 6. Where to look when something breaks

| Symptom | First place to look |
| --- | --- |
| Submission rejected with a signature error | `transaction_from_params` and `verify_transaction_signature` in `subhost-rpc/src/methods.rs` |
| Submission rejected as not executable | `select_transactions` and `State::apply_transaction` |
| Peer block rejected | `accept_block` in `subhost-rpc/src/block.rs`, then `validate_chain` in `subhost-storage` |
| Node will not start with a ledger on disk | `LedgerStore::load` validation order |
| JSON-RPC answers 401 or the node refuses to bind | `subhost-rpc/src/security.rs`: token file, `allow_public`, then the middleware layer |
| Block height stuck behind a peer | the ordering buffer in `subhost-node` and its 64-block window |
| State root mismatch after restart | `State::root`, account leaf sorting, and chain-ID binding |
