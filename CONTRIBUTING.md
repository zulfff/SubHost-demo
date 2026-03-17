# Contributing to SubHost

Hey, thanks for wanting to help out! Really appreciate it.

## Getting Started

First things first, fork the repo and clone it:

```bash
git clone https://github.com/zulfff/SubHost.git
cd SubHost
```

Make sure you got Rust installed (need 1.75+):

```bash
rustc --version
```

If not, grab it from [rustup.rs](https://rustup.rs).

## How to Contribute

### 1. Create a branch

```bash
git checkout -b my-awesome-feature
```

### 2. Do your thing

Write code, break things, fix them, you know the drill.

Before you commit, run these:

```bash
cargo check    # catch errors early
cargo test     # make sure tests pass
cargo fmt      # keep it pretty
cargo clippy   # catch silly mistakes
```

Tests gotta pass. No exceptions.

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

- Run `cargo fmt` before committing
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
