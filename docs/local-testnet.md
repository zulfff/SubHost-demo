# Four-validator local testnet

Run four validator nodes with distinct BLS producer keys, one shared chain-1337
genesis and a three-of-four quorum. Each node has its own ledger and voting
journal. The example binds every RPC/P2P listener to loopback and uses a known
development sender; use it only for local test funds.

```sh
cargo run -p subhost-node --example local-testnet -- --data-dir /tmp/subhost-phased-validators --smoke
```

This creates `/tmp/subhost-phased-validators`, starts RPC ports 18545–18548 and
P2P ports 30340–30343, submits two identical signed transactions to every potential leader through RPC and waits for
receipts on every validator. It checks equal committed tips and state roots,
prints the resulting height, then stops. Running it again resumes the same keys,
journals and ledger, using the next sender nonce. Keys are created once with
mode `0600`; a conflicting or incomplete existing setup is refused.

To keep the network running, omit `--smoke`. Ctrl-C stops the four nodes. Configure
separate data and ports when running another local cluster:

```sh
cargo run -p subhost-node --example local-testnet -- \
  --data-dir /tmp/my-subhost-phased-testnet --rpc-base-port 19545 --p2p-base-port 31340
```

## Run validators as separate processes

After the smoke command has created the default directories and exited, build
the CLI and start validator 1 in its own terminal:

```sh
cargo build -p subhost-cli
target/debug/subhost node --validator \
  --data-dir /tmp/subhost-phased-validators/validator-1 \
  --listen 127.0.0.1:18545 --p2p-listen /ip4/127.0.0.1/tcp/30340 --no-mdns
```

In three more terminals, use `validator_number=2`, `3` and `4`:

```sh
validator_number=2
target/debug/subhost node --validator \
  --data-dir "/tmp/subhost-phased-validators/validator-$validator_number" \
  --listen "127.0.0.1:$((18544 + validator_number))" \
  --p2p-listen "/ip4/127.0.0.1/tcp/$((30339 + validator_number))" \
  --bootstrap-peer /ip4/127.0.0.1/tcp/30340 --no-mdns
```

Views rotate independently from height. Until S7 transaction gossip lands,
submit the same authenticated transaction to every potential leader; the smoke
runner does this concurrently. Only the designated leader proposes. Peers verify
the block and leader envelope, form a Prepare quorum, then a separate final
Commit quorum. A returned hash remains pending until the receipt appears; two
out of four validators cannot write a block.

Followers retry the hub endpoint if it is initially unavailable or restarts,
with backoff capped at 30 seconds. A peer missing the latest next block can
receive its certified commit. There is no historical backfill for a longer gap.

## Restart and backup

Stop a validator before restarting it in the same directory. `consensus.lock`
holds an OS lock while the process runs; the lock file can remain after exit.
Do not delete it to bypass a live process, or run the same producer key from
another directory. Restart verifies `consensus-state.bin` and restores the open
proposal, already-sent votes in both phases, active view, prepared lock and authenticated timeout reports. It reconciles a certified commit
that was interrupted around the ledger append.

Preserve the newest journal, genesis and producer identity alongside the ledger.
The [backup runbook](operations/backup-restore.md) validates committed storage;
node bootstrap also validates voting state. Never replace the latest journal
with an older backup: it can forget votes released after the backup. A ledger
behind the newer signing state is refused until a reviewed recovery procedure
restores the missing history. A missing, damaged or incompatible journal is
refused. Validator directories from older builds without a journal need a new
local testnet directory; no automatic migration can prove that they never voted.

## Recovery checks

```sh
cargo test -p subhost-node --all-features
```

The real P2P regression starts a follower before its hub, verifies that two
validators cannot commit, adds a third to reach quorum, delivers the certified
block to a late fourth peer, restarts the hub and checks another common commit.
Recovery regressions cover failed journal writes before vote release, replay
of prepared commits, interruption after append without a duplicate append,
conflicting proposals after restart, damaged/missing journals and a rolled-back
ledger against newer voting state.

The default view timeout is 2000 ms. An unavailable leader is replaced using
a quorum of signed reports; timeout alone cannot unlock a prepared block.
Regressions cover three live validators, two-vote shortfall with view progress,
a silent initial leader, partition reconnection and different saved views.

Gossip uses a 4 MiB transport budget with framing headroom. Canonical per-block
budgets reserve repeated blocks/proofs in complete timeout frames and the
16 MiB signing journal, for sets of 1 through 64 validators. Gas-valid transactions
that cannot fit are refused before replacing pending entries.

Historical QC replay/native sync, transaction gossip, peer scoring and per-peer
limits remain later stages. The standard RFC 9380 BLS suite refuses legacy
proofs/signatures because the old mapping permitted public-PoP forgery. Use a
fresh genesis and fresh signing/history directories when upgrading, preserving
old directories and keys. Version-two `SUBVOTE2` journals deliberately refuse
legacy data. See [consensus limits](consensus.md); independent audit and the actual
testnet soak remain required before production assurance.
