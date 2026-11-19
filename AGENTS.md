# Agent working agreement

This repository is built by two agents in parallel plus the maintainer. Read this
file before you change anything. The full protocol lives in
`docs/agents/README.md`; the staged plan is in `ROADMAP.md`.

## 1. Know your track

| Track | Owns |
| --- | --- |
| **A - Protocol** | `subhost-core`, `subhost-consensus`, `subhost-network`, node block/consensus path, RPC block path |
| **B - Platform** | `subhost-storage`, RPC surface/auth/methods, `subhost-mempool`, `subhost-cli`, `subhost-metrics`, `subhost-telemetry`, `explorer`, `subhost-faucet`, CI/Docker/release |

State your track in every message you write to the other agent.

## 2. Hard gate: reply before you start

Before starting any task, run:

```sh
ls docs/agents/questions/open/
```

If an open question names your track in `addressed_to` and its `blocks:` field
lists the task you are about to start, you **must**:

1. Read the question and its evidence.
2. Write a reply at `docs/agents/replies/Q-<nnn>-<your-track>.md` using the
   template in `docs/agents/README.md`.
3. Wait for the author to mark it `agreed` (question moves to
   `docs/agents/questions/agreed/`) or to escalate it.

**Do not start a blocked task while the question is still in `open/` or `review/`.**
This gate is not optional. It is the only thing that keeps two agents from building
incompatible contracts in parallel.

### The channel is the workspace, not a branch

Both agents run in this same working tree, so a question or reply is just a local
file, not git work:

- Write the question or reply straight into the working tree, then move the question
  file between `open/`, `review/`, `agreed/`, `escalated/`.
- **Never** create a branch, commit, push, or open a PR for a question, a reply, or a
  follow-up. Discussion is not code.
- `docs/agents/questions/**` and `docs/agents/replies/**` are gitignored on purpose.
  Do not `git add` them, do not force-add them, and do not sweep them into a commit.
- Keep it conversational. Append to your own reply file as the exchange continues
  instead of opening a new document per thought; a few lines that state a position
  and a reason are enough.
- The only artifact of a discussion that reaches git is the ADR, and it lands with
  the code change that implements it, never in a PR of its own.

## 3. Research before you ask

- Use the internet before opening a question or guessing: official docs first
  (docs.rs, the crate repository, RustSec advisories, RFCs), then reputable
  secondary sources.
- Put the URL, what you verified, and the date in the `## Evidence` section.
- Never ask the other agent something a document already answers.
- Mark confidence honestly: `verified` vs `likely` vs `assumption`.
- Never paste secrets, private keys, tokens, or user data into a question or reply.

## 4. Share opinions proactively

Open a thread for anything that crosses the crate boundary, even when it does not
block you: a shared type, a schema or ledger version, a file split, a semantic
change, a dependency bump that changes the MSRV. Use `blocks: none` and
`review_by: <stage>`, and ask for a reply by the end of that stage.

You are expected to disagree in writing when you think the other track is wrong.
Silent compliance is worse than a resolved argument.

## 5. Editing rules

- One stage = one branch = one PR, branched from a freshly rebased `main`.
- `git add` explicit paths. **Never `git add -A`.**
- Keep `docs/agents/questions/**` and `docs/agents/replies/**` out of the index.
  Stage only code, docs, and `docs/agents/decisions/ADR-*.md`.
- Do not edit a file owned by the other track without a contract commit.
- Do not rewrite, rebase, or force-push another agent's branch or commits.
- No `todo!`, `unimplemented!`, `dbg!`, or `unsafe`.
- Prose: no em dashes in CHANGELOG/docs (R-02); use commas, colons, or plain hyphens.
- Add failure-path tests, update `CHANGELOG.md` and the relevant doc, keep CI green.
- If you defer something, write the limitation down in the PR and in `ROADMAP.md`.

## 6. Disagreement and escalation

- One round of replies is expected to settle a question.
- If you still disagree, do not proceed: move the question to
  `docs/agents/questions/escalated/`, stop the blocked task, and ask the maintainer
  in chat with both options, the trade-offs, and your recommendation.
- The maintainer's decision is written into
  `docs/agents/decisions/ADR-<nnn>-<slug>.md` and the task resumes.

## 7. Before you push

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny --all-features check
```

Then rebase onto `main`, resolve only your own conflicts, and confirm the seven CI
jobs are green. A red CI is never "someone else's problem".

Finally, check the index holds only code and docs:

```sh
git diff --cached --name-only | grep -E '^docs/agents/(questions|replies)/' || echo 'ok: no chat files staged'
```

The only allowed hit is the one-time commit that removes the threads from tracking.
