# Contributing

Thanks for wanting to help. This document is the short version of what a
reviewable change looks like here.

## Setup

```bash
git clone https://github.com/zulfff/SubHost-demo.git
cd SubHost-demo
```

`rust-toolchain.toml` pins the toolchain, so `rustup` installs the right compiler
and components (`rustfmt`, `clippy`) automatically on the first build. The minimum
supported Rust version is 1.89, and CI checks it separately. It is not 1.75:
transitive dependencies in `Cargo.lock` require edition 2024, which 1.75 cannot
parse, and `enum-ordinalize` (reached through `educe` -> `ark-ec`) requires 1.89.

`.cargo/config.toml` caps build parallelism at two jobs, which keeps a release
build inside about 4 GB of RAM. On a larger machine, override it with `--jobs` or
`CARGO_BUILD_JOBS`.

## Before you open a pull request

Run the same gates CI runs:

```bash
cargo fmt --all
cargo lint                        # clippy over the whole workspace, warnings denied
cargo test --workspace --all-features
```

CI additionally runs `cargo deny --all-features check`, a 1.89 MSRV check, a
release build, and `cargo doc` with warnings denied. All of them are required.

## What the code must satisfy

- **No warnings.** Clippy runs with `-D warnings` over every target and feature.
- **No `unsafe`.** The workspace sets `unsafe_code = "forbid"`.
- **No placeholders.** `todo!`, `unimplemented!`, and `dbg!` are denied. A function
  that cannot do its job must return an error instead of pretending to succeed.
- **Checked arithmetic on anything that represents value.** Balances, nonces, and
  gas use `checked_*`, not wrapping. An overflow is an error.
- **Tests for behaviour, including the failure paths.** A new validation rule needs
  a test proving the invalid input is rejected, not only a test showing valid input
  passes.
- **Document why, not what.** Comments should explain a non-obvious decision or a
  security property. Do not narrate the code.

## Honesty about scope

The README's status table is authoritative about what is implemented. If your
change makes something work, update that table. If it does not, do not describe it
as if it does. An aspirational comment or doc line is treated as a defect here.

Naming a module after a technique does not implement it. `subhost-network` carries
a `TransactionStem` message type but implements no Dandelion++ relay, and the
module documentation says so. Keep that pattern.

## Security-sensitive areas

Changes under these paths get closer review, and `.github/CODEOWNERS` enforces it:

- `crates/subhost-crypto`: signature and key-exchange primitives
- `crates/subhost-wallet`: key storage and derivation
- `crates/subhost-consensus`: quorum and slashing rules
- `crates/subhost-state`: balance and nonce rules
- `crates/subhost-storage`: the durable ledger format
- `crates/subhost-rpc`: the authenticated write path

If you change a signature payload, an address derivation, or the on-disk format,
say so explicitly in the pull request. Those changes invalidate existing wallets or
ledgers.

Do not open a public issue for a suspected vulnerability. Follow
[SECURITY.md](SECURITY.md).

## Commits and pull requests

Conventional-style prefixes are preferred: `feat:`, `fix:`, `docs:`, `refactor:`,
`perf:`, `test:`, `build:`, `ci:`.

The pull request template asks what changed, how you verified it, the security
impact, and whether anything breaks. Fill it in. "none" is a fine answer when it is
true.

Never commit a private key, wallet file, ledger, or `.env`. `.gitignore` covers the
usual paths, but check `git diff --cached` before you commit.

## Where help is most useful

- **Consensus in the node.** `subhost-consensus::driver` reaches agreement
  in-process over `2f + 1` BLS votes, but the node still produces blocks directly
  and never runs the driver. Wiring it in needs validator keys from genesis, vote
  gossip, timeouts, view changes, and leader rotation.
- **Fork choice and reorgs.** `accept_block` only accepts the exact child of the
  local tip. A competing block at the same height is rejected and dropped forever,
  so two producers fork with no recovery path.
- **Block sync.** Gossip delivers live blocks only. A node that was offline has no
  way to request missed history.
- **Multi-validator IBC light clients.** The current client trusts one ed25519 key.
  Validator-set rotation, Tendermint-style quorum certificates, and non-membership
  proofs are not implemented.
- **Sparse state commitment.** The Merkle tree proves membership. Proving that an
  account is absent requires a sparse or indexed tree.

## Reporting a bug

Use the issue template. Include the commit, `rustc --version`, the exact commands
that reproduce the problem, and the error output verbatim.