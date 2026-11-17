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
genesis allocations only when the ledger file does not exist yet. A restarted node
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
- `validator` is `Address::ZERO` and `signatures` is empty. See section 3.

A producer never seals an empty block: if nothing is executable, submission fails
instead of writing a block.

### 2.5 Persist, then publish

Durability comes before visibility. The node builds the next block list and receipt
map in memory, writes the whole ledger snapshot, and only then swaps the new state,
blocks, and receipts into place and stores the new height. If persistence fails,
none of the swaps happen and no block event is emitted.

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

Today: any node that can reach its own JSON-RPC port, and any peer that can deliver a
valid block one height above the tip. There is no producer authorization.

- A locally produced block carries no validator identity and no signature.
- `accept_block` does not check who produced a block. It checks that the block is
  internally consistent and extends the tip.
- The JSON-RPC surface has no authentication, so the only thing that limits block
  production is who can reach the listening address. The node binds loopback by
  default.

This is the gap S1 closes. `ADR-001` fixes BLS12-381 as the scheme, a dedicated
producer key file, and a mandatory proof of possession. S1.2 signs
`Block::signing_payload()` on produce, S1.3 verifies one registered producer
signature on accept, and S1.4 wires the key file and genesis registration into the
node and CLI.

## 4. What the ledger guarantees

Durability and integrity:

- one versioned envelope per ledger file: magic, format version, BLAKE3 checksum, and
  payload;
- every write goes to a temporary file in the same directory, is fsynced, atomically
  renamed over the target, and the directory is fsynced;
- size is capped at 64 MiB; a larger snapshot is refused rather than truncated;
- every read is validated before it is trusted: size bound, magic, version,
  checksum, chain binding, then a full replay of the block and receipt commitments
  against the restored state;
- the replay checks contiguous heights from 1, parent links, chain ID, gas totals,
  unique blocks and transactions, every transaction signature, both commitments per
  block, exactly one `Success` receipt per transaction, and that the restored state
  root equals the newest block's `state_root`;
- a tampered or truncated file is rejected, not silently loaded.

Determinism:

- `state_root` is a Merkle root over address-sorted account leaves, so insertion
  order does not affect it;
- the chain ID is mixed into every leaf, binding the root to one chain;
- receipt commitments sort by height before persistence, so the file is byte-stable;
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
- no reorg: once a block is persisted it can only be replaced by deleting the ledger
  file and replaying from genesis;
- no pruning: every block is persisted in full on every write, so the 64 MiB cap
  bounds how much history a node can keep;
- no failed receipts: a transaction that fails to execute is not included, so every
  persisted receipt is `Success` with empty logs and no contract address;
- no authentication for JSON-RPC or metrics clients, and no producer authorization
  on gossip. The gossip transport encrypts and authenticates peer connections, but
  any peer may publish a block proposal.

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
