# Inter-agent protocol

Two agents work this repository at the same time, in the same working tree. The
checkout **is** the channel: questions, replies, and follow-ups are local files they
write and read directly. This document defines how they ask, answer, disagree, and
record decisions.

The short version for daily use is `AGENTS.md`. This file is the full protocol.

---

## 1. Why the workspace

Both agents run in this same working tree, so the checkout itself is the channel: a
question and its replies are plain files on disk, written and read directly.

- No branch, commit, push, or PR for coordination. Discussion is not a deliverable.
- The thread files are gitignored, so they never appear in a diff, a PR, or CI.
- The exchange stays readable for the maintainer and survives a lost session, because
  it lives on disk until the maintainer clears it.
- No shared mutable file is edited by both sides, so there are no conflicts.

Do not paste the exchange into a PR description and do not re-create it in git
history. `git log` is for code; `docs/agents/` is the chat.

Only the ADR is committed, and it rides with the code change it describes.

---

## 2. Directory layout

```text
docs/agents/
├── README.md                 this protocol
├── questions/
│   ├── open/                 awaiting the first reply
│   ├── review/               replied; awaiting the author
│   ├── agreed/               closed with agreement
│   └── escalated/            needs the maintainer
├── replies/                  one file per replying track, appended to as you talk
└── decisions/                ADR-<nnn>-<slug>.md, written after agreement
```

The **question author owns the question file**. The **replying track owns its reply
file**. Neither ever edits the other's file. Moving a question between directories
is the only status change, and only the author does it.

`ls docs/agents/questions/open/` is the inbox. There is no index file to conflict on.

`questions/**` and `replies/**` are listed in `.gitignore`. They are workspace state,
not repository content: never stage them, even with `-f`. Only `README.md` and
`decisions/ADR-*.md` are tracked from this tree.

---

## 3. Lifecycle

```text
 open/  ──reply──▶  review/  ──author accepts──▶  agreed/  ──▶  ADR (if it changes code)
                          │
                          └──no agreement──▶  escalated/  ──▶  maintainer decision
```

1. **Open.** The author creates
   `questions/open/Q-<nnn>-<short-slug>.md` from the template below.
2. **Reply.** Each addressed track writes `replies/Q-<nnn>-<track>.md`. The author
   moves the question to `review/` once at least one reply exists.
3. **Settle.** If the reply agrees (or the author accepts the counter-proposal), the
   author moves the question to `agreed/`, updates `decision:` in front matter, and
   writes an ADR if the outcome changes a contract.
4. **Escalate.** If the tracks still disagree after one round, move it to
   `escalated/`, stop the blocked task, and ask the maintainer in chat with both
   options and a recommendation. The task does not resume until the decision is
   recorded.

The whole loop happens in the working tree, in minutes, with plain edits: no branch,
no commit, no PR, no review queue. A follow-up is another `## Follow-up <date>`
section appended to the same reply file: the thread reads top to bottom like a chat.

A question with `blocks: none` does not stop anyone; it is a review request with a
`review_by` stage. A question with `blocks: [S1.2]` stops whoever owns `S1.2`.

---

## 4. Question template

File: `docs/agents/questions/open/Q-001-block-signature-scheme.md`

```md
---
id: Q-001
title: Which signature scheme signs a block?
author: track-a
addressed_to: [track-b]
blocks: [S1.2, S1.3]      # ROADMAP task ids, or "none"
review_by: S1.2           # stage by which a reply is needed
status: open
opened: 2026-11-18
confidence: high
---

## Context

What decision is needed, why now, and which files/contracts it affects.

## Opinion

What the author thinks should happen, in one or two sentences.

## Options

| Option | Pros | Cons |
| --- | --- | --- |
| A | … | … |
| B | … | … |

## Evidence

- `crates/.../lib.rs:123`: what the code does today.
- https://docs.rs/...: what the upstream docs say (verified 2026-11-18).

## Decision rule

What would change the author's mind. Be concrete.
```

## 5. Reply template

File: `docs/agents/replies/Q-001-track-b.md`

```md
---
id: Q-001
reply_by: track-b
position: agree | disagree | needs-info
date: 2026-11-18
---

## Answer

One paragraph, direct.

## Reasoning

Why. Reference code paths and documents, not vibes.

## Evidence

- https://… (verified 2026-11-18)
- `crates/…/lib.rs:123`

## If disagree

The alternative you want, what it costs, and what would make you accept the
author's option instead.

## Follow-up 2026-11-18

Appended later, same file, newest at the bottom. Short is fine: this is a
conversation, not a report. Say what changed your mind, or restate the one thing
that still blocks agreement.
```

A reply that says only "agree" is not a reply. State the reason or the objection.

---

## 6. When to open a question

Open one when at least one is true:

- You are about to change a **shared contract**: a type in `subhost-core`, a ledger
  format or version, a network message, a CLI flag that the other track documents, a
  file split, or a public function the other track calls.
- You need a decision the other track owns (key format, storage API, deployment
  shape).
- You believe the other track's plan is wrong or will not compile against reality.
- You are blocked and the answer is not in the docs, the code, or the internet.

Do **not** open one to ask something `ROADMAP.md`, `SECURITY.md`, the crate docs, or
an upstream document already answers. Do the research first and put the links in
the question.

---

## 7. Research rules

1. Official documentation first: docs.rs, the upstream repository, RFCs, RustSec.
2. Then high-quality secondary sources. Mark the difference.
3. Write down the exact claim you verified, not just a link.
4. Date every source. The ecosystem moves; a 2024 assumption may be wrong.
5. If sources disagree, say so and pick the safer option.
6. Record the conclusion in `## Evidence`, so the next agent does not repeat the
   search.
7. Never paste credentials, private keys, or personal data.

---

## 8. Tone and disagreement

- Argue about the code, not the author.
- Prefer "this rejects X, see `lib.rs:88`" over "this is wrong".
- Every objection needs an alternative and a cost.
- If you were wrong, say so in the thread and move on. The record is the point.
- A resolved disagreement produces a better design than silent agreement.

---

## 9. Example: what a real exchange looks like

`Q-001` asks whether blocks use BLS or ed25519. Track A writes the question with
the trade-offs and its opinion (BLS, because consensus QCs are BLS). Track B replies
`disagree`: the CLI already manages ed25519 wallet keys, so a second key type means
a second keystore. Track B proposes BLS with an explicit `producer-key` file command.
Track A accepts the counter-proposal, moves the question to `agreed/`, and writes
`decisions/ADR-001-block-signature.md`. Both then implement against the ADR.

That is the whole mechanism: one question, one reply, one recorded decision.
