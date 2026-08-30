# Contributing

Thanks for wanting to help. This document is the short version of what a
reviewable change looks like here.

## Setup

```bash
git clone https://github.com/zulfff/SubHost-demo.git
cd SubHost-demo
```

`rust-toolchain.toml` pins the toolchain, so `rustup` installs the right compiler
and components (`rustfmt`, `clippy`) automatically on first build. The minimum
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
  that cannot do its job must return an error, not pretend to succeed.
- **Checked arithmetic on anything that represents value.** Balances, nonces, and
  gas are `checked_*`, not wrapping. An overflow is an error.
- **Tests for behaviour, including the failure paths.** A new validation rule needs
  a test proving the invalid input is rejected, not only that valid input passes.
- **Document why, not what.** Comments should explain a non-obvious decision or a
  security property. Do not narrate the code.

## Honesty about scope

The README's status table is authoritative about what is implemented. If your
change makes something work, update that table. If it does not, do not describe it
as if it does — an aspirational comment or doc line is treated as a defect here.

Naming a module after a technique does not implement it. `subhost-network` carries
a `TransactionStem` message type but implements no Dandelion++ relay, and the
module documentation says so. Keep that pattern.

## Security-sensitive areas

Changes under these paths get closer review, and `.github/CODEOWNERS` enforces it:

- `crates/subhost-crypto` — signature and key-exchange primitives
- `crates/subhost-wallet` — key storage and derivation
- `crates/subhost-consensus` — quorum and slashing rules
- `crates/subhost-state` — balance and nonce rules
- `crates/subhost-storage` — the durable ledger format
- `crates/subhost-rpc` — the authenticated write path

If you change a signature payload, an address derivation, or the on-disk format,
say so explicitly in the pull request: those changes invalidate existing wallets
or ledgers.

Do not open a public issue for a suspected vulnerability. Follow
[SECURITY.md](SECURITY.md).

## Commits and pull requests

Conventional-style prefixes are preferred: `feat:`, `fix:`, `docs:`, `refactor:`,
`perf:`, `test:`, `build:`, `ci:`.

The pull request template asks what changed, how you verified it, the security
impact, and whether anything breaks. Fill it in; "none" is a fine answer when it
is true.

Never commit a private key, wallet file, ledger, or `.env`. `.gitignore` covers
the usual paths, but check `git diff --cached` before you commit.

## Where help is most useful

- **Consensus driver.** The quorum primitives exist and are tested; nothing drives
  them into agreement yet.
- **Block propagation.** `subhost-network` can publish and receive gossip, but no
  node consumes it.
- **Multi-transaction blocks.** The producer seals one transaction per block.
- **IBC proof verification.** Packet bookkeeping is done; light-client and
  commitment proof verification is not.
- **State commitment.** The state root hashes a sorted account list; a Merkle
  structure would allow proofs.

## Reporting a bug

Use the issue template. Include the commit, `rustc --version`, the exact commands
that reproduce the problem, and the error output verbatim.
