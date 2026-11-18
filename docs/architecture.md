# Architecture

This document explains how a running Subhost node turns a signed transaction into a
committed block, what the ledger actually guarantees today, and which crate owns each
decision. It describes the code as it is, not the target state in `ROADMAP.md`.

## 1. Process shape

A node is one process assembled in `subhost-node`:

- `RpcState` holds the chain ID, the mempool, the account state, the block history,
  the receipt index, the ledger store, and a commit lock. Every field is shared
  behind an `Arc`, so cloning the state is cheap and all clones see one ledger.
- `RpcServer` serves the JSON-RPC surface over HTTP and WebSocket. There is no
  authentication on that surface.
- The optional metrics exporter, the optional libp2p gossip task, and a five-second
  observer that copies block height and pending count into metrics.
- `subhost-cli` parses operator flags, writes genesis files, and calls
  `Node::bootstrap`. It contains no ledger logic.

`Node::bootstrap` loads `genesis.json` when present, opens the ledger, and applies
genesis allocations only when no ledger exists yet. A restarted node
restores the ledger and logs the restored height instead of re-applying allocations.
The mempool is not persisted, so a restart loses pending transactions.

A node started with `--validator` refuses to start without a genesis file that
contains at least one initial validator. A node without that flag may run with no
genesis file at all, using the fallback chain ID and an empty allocation set.

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

The live pool is replaced only after the block write succeeds, so a rejected or
failed submission leaves admission, replacement, and eviction undone.

### 2.3 Select

The producer sorts pending senders by address, then walks each sender's
nonce-contiguous prefix starting at that sender's current state nonce. It stops at
the first transaction that does not fit in the remaining 30,000,000 block gas or is
rejected by the state transition, so a block never skips a nonce for a sender.
Selection runs against a clone of the account state; the live state is untouched
until the commit succeeds.

### 2.4 Execute and seal

`seal_block` produces at most one block per submission:

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

On success the node removes the mined transactions from the pool, emits
`BlockEvent::Sealed`, and the network task relays only locally sealed blocks. A block
received from a peer is never relayed again.

### 2.6 Accept a peer block

`RpcState::accept_block` re-validates a block end to end before touching anything:

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

Peer blocks that arrive before their parent are buffered by height. A block at or
below the next expected height is discarded as stale, and anything more than 64
blocks ahead of the local tip is refused so a peer cannot force unbounded buffering.
The first block seen for a height wins; a duplicate cannot replace it. Whenever the
local tip advances, buffered successors are applied in order. A rejected buffered
block is logged and dropped, because `accept_block` is deterministic and a retry
cannot succeed.

## 3. Who may produce a block

Today: any node that can reach its own JSON-RPC port can submit a transaction, and
any node whose genesis carries a validator set rejects blocks that do not come from
one of those validators. On a genesis with no validators, the single-node path is
unchanged and unsigned blocks are accepted.

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
- `accept_block` on a node with a registry requires `header.validator` to be
  registered and exactly one signature from that validator over the payload, checked
  before the state transition. A missing, extra, foreign, malformed, or stale
  signature is rejected with `RpcError::UnauthorizedProducer`.
- A node without a registry, which means a genesis with no validators, does not check
  block signatures. That is the development path, and it is not a network guarantee.
- The JSON-RPC surface has no authentication, so the only thing that limits block
  production is who can reach the listening address. The node binds loopback by
  default.

The remaining gap is `init`: `subhost init --validator` does not write proofs of
possession yet, so a genesis it produces today is refused at start-up by the registry
check above. `ADR-001` fixes BLS12-381 as the scheme and a mandatory proof of
possession; the CLI half of S1.4 supplies it, and until then only a genesis written
with real proofs, as the node tests do, can boot a validator.

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
- every read is validated before it is trusted: size bounds, magic, versions,
  checksums, manifest consistency (expected file names, record counts, snapshot name
  and height), and chain binding;
- the snapshot's state root must equal the `state_root` of the block at the
  snapshot's own height, so a snapshot cannot be replayed onto a different history;
- blocks after the snapshot are executed again with `State::apply_transaction`, then
  a full replay checks contiguous heights from 1, parent links, chain ID, gas totals,
  unique blocks and transactions, every transaction signature, both commitments per
  block, exactly one `Success` receipt per transaction, and that the restored state
  root equals the newest block's `state_root`;
- a tampered, truncated, or half-written ledger is rejected, not silently loaded. A
  version 2 `node-state.bin` is still read; the first persist migrates it to segments
  and keeps it as `node-state.bin.v2-backup`.

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
- no block sync or backfill: a gap is only filled by blocks arriving through gossip
  within the 64-block window;
- no reorg: `persist` refuses a snapshot that does not extend the committed head
  (`StorageError::NonAppendPersist`), so a competing branch can only replace the
  chain by deleting the ledger directory and replaying from genesis;
- no pruning and no bounded start-up I/O: a fresh snapshot is written every 1024
  blocks and start-up executes at most that many blocks on top of it, but every
  segment is still read and scanned, and old segments are never dropped, so
  start-up I/O and disk use still grow with history until S4.3–S4.4;
- no failed receipts: a transaction that fails to execute is not included, so every
  persisted receipt is `Success` with empty logs and no contract address;
- no authentication for JSON-RPC or metrics clients. Producer authorization on
  gossip is enforced by every node whose genesis carries a validator set; a genesis
  with no validators is the development path and accepts unsigned blocks. The gossip
  transport encrypts and authenticates peer connections, but any peer may publish a
  block proposal.

## 5. Component ownership

| Crate | Owns | Does not own |
| --- | --- | --- |
| `subhost-core` | Types, canonical encoding, hashing, the block signing payload, genesis document and its structural validation | Ledger writes, execution, transport |
| `subhost-crypto` | BLS12-381 and ed25519 primitives, X25519, ChaCha20-Poly1305 | Key storage, policy, when to sign |
| `subhost-state` | Account state, nonce and balance rules, the state root, inclusion proofs | Signatures, block assembly, durability |
| `subhost-mempool` | Admission policy, per-sender ordering, replacement, eviction | Execution, persistence |
| `subhost-storage` | On-disk format, atomic write, load validation, receipt and block commitments | Deciding which block to write |
| `subhost-rpc` | Block selection, sealing, acceptance, the JSON-RPC surface, submission signature check | Consensus, peer identity, genesis policy |
| `subhost-consensus` | Validator registry, votes, quorum certificates, DAG, slashing | Nothing in the node path yet; it is not wired into `subhost-node` |
| `subhost-node` | Node assembly, genesis loading, gossip relay, ordering buffer, shutdown | Ledger rules, block validity |
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
| Block height stuck behind a peer | the ordering buffer in `subhost-node` and its 64-block window |
| State root mismatch after restart | `State::root`, account leaf sorting, and chain-ID binding |
