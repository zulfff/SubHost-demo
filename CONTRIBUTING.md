# Contributing to SubHost

Hey, thanks for wanting to help out! Really appreciate it.

## Getting Started

First things first, fork the repo and clone it:

```bash
git clone https://github.com/zulfff/SubHost-demo.git
cd SubHost-demo
```

Make sure you got Rust installed (need 1.75+):

```bash
rustc --version
```

If not, grab it from [rustup.rs](https://rustup.rs). The formatting and lint
commands also need the `rustfmt` and `clippy` components:

```bash
rustup component add rustfmt clippy
```

## How to Contribute

### 1. Create a branch

```bash
git checkout -b my-awesome-feature
```

### 2. Do your thing

Write code, break things, fix them, you know the drill.

Before you commit, run one heavy Cargo command at a time. On a machine with about
4 GB RAM, use one compiler job:

```bash
cargo fmt --all -- --check
cargo check --workspace -j 1

# Test and lint the package you changed; subhost-cli is an example.
cargo test -p subhost-cli -j 1
cargo clippy -p subhost-cli --all-targets -j 1 -- -D warnings
```

Replace `subhost-cli` with each package affected by your change. Do not run
`cargo test --workspace` blindly on a low-memory machine: some test targets pull
large EVM/WASM dependency graphs. The CI workflow shows the current safe
multi-package test set.

### 3. Commit it

```bash
git add .
git commit -m "what you did in plain english"
git push origin my-awesome-feature
```

### 4. Open a Pull Request

Go to GitHub, hit "New Pull Request", and tell us:
- What you changed and why
- Any issues it fixes
- Screenshots if there's UI stuff

## Code Style

We try to keep it simple:

- Run `cargo fmt --all -- --check` before committing
- Fix clippy warnings, don't ignore them
- Document your public functions with `///` comments
- Use `thiserror` or `anyhow` for errors

### Commit message format (optional but nice)

```
feat: add new thing
fix: fix broken thing
docs: update readme
refactor: clean up mess
perf: make it faster
test: add more tests
```

## Where We Need Help

These areas could use some love:

- **Crypto/ZK stuff** - optimizations, new curves
- **P2P networking** - libp2p is tricky, help welcome
- **Consensus** - HotStuff and DAG improvements
- **EVM compatibility** - revm integration work
- **Benchmarks** - performance testing
- **Documentation** - always behind on this

## Found a Bug?

Open an issue and tell us:
1. What you were trying to do
2. What you expected to happen
3. What actually happened
4. How to reproduce it
5. Your setup (OS, Rust version, etc)

The more detail, the faster we can fix it.

## Questions?

- Open a GitHub Discussion
- Comment on relevant issues
- Or just open an issue and ask

## One Rule

Don't be a jerk. Help others learn, be patient with mistakes, assume good intentions.

---

That's pretty much it. Looking forward to your PR!
