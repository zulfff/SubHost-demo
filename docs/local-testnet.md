# Four-validator local testnet

Run four validator nodes with distinct BLS producer keys, one shared chain-1337
genesis and a three-of-four quorum. Each node has its own ledger and voting
journal. The example binds every RPC/P2P listener to loopback and uses a known
development sender; use it only for local test funds.

```sh
cargo run -p subhost-node --example local-testnet -- --smoke
```

This creates `/tmp/subhost-four-validators`, starts RPC ports 18545–18548 and
P2P ports 30340–30343, sends two signed transactions through RPC and waits for
receipts on every validator. It checks equal committed tips and state roots,
prints the resulting height, then stops. Running it again resumes the same keys,
journals and ledger, using the next sender nonce. Keys are created once with
mode `0600`; a conflicting or incomplete existing setup is refused.

To keep the network running, omit `--smoke`. Ctrl-C stops the four nodes. Configure
separate data and ports when running another local cluster:

```sh
cargo run -p subhost-node --example local-testnet -- \
  --data-dir /tmp/my-subhost-testnet --rpc-base-port 19545 --p2p-base-port 31340
```

## Run validators as separate processes

After the smoke command has created the default directories and exited, build
the CLI and start validator 1 in its own terminal:

```sh
cargo build -p subhost-cli
target/debug/subhost node --validator \
  --data-dir /tmp/subhost-four-validators/validator-1 \
  --listen 127.0.0.1:18545 --p2p-listen /ip4/127.0.0.1/tcp/30340 --no-mdns
```

In three more terminals, use `validator_number=2`, `3` and `4`:

```sh
validator_number=2
target/debug/subhost node --validator \
  --data-dir "/tmp/subhost-four-validators/validator-$validator_number" \
  --listen "127.0.0.1:$((18544 + validator_number))" \
  --p2p-listen "/ip4/127.0.0.1/tcp/$((30339 + validator_number))" \
  --bootstrap-peer /ip4/127.0.0.1/tcp/30340 --no-mdns
```

Send transactions to validator 1. Transactions are not gossiped to other
producers, and simultaneous proposals can stall this linear driver. Other nodes
verify the proposal and vote. A returned transaction hash is pending until its
receipt appears; two of four votes leave the ledger unchanged.

Followers retry the hub endpoint if it is initially unavailable or restarts,
with backoff capped at 30 seconds. A peer missing the latest next block can
receive its certified commit. There is no historical backfill for a longer gap.

## Restart and backup

Stop a validator before restarting it in the same directory. `consensus.lock`
holds an OS lock while the process runs; the lock file can remain after exit.
Do not delete it to bypass a live process, or run the same producer key from
another directory. Restart verifies `consensus-state.bin` and restores the open
proposal, already-sent vote and verified tally. It reconciles a certified commit
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

Timeout/view change, leader rotation, historical sync, fork choice, peer scoring
and per-peer limits remain pending. Gossip has a 256 KiB transport bound; a
gas-valid block too large for that envelope can stall. Keep test transactions
small. The journal retains the latest QC but does not authenticate historical
QCs on ledger replay. See [consensus limits](consensus.md); this is a local
prototype testnet, not production BFT or a finality guarantee.
