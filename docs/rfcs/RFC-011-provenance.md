# RFC-011: Provenance — who wrote this, how, and how sure

- **Status:** **Frozen 2026-08-06 (Phase 3 exited)** — accepted and implemented the same day. Two amendments were made at implementation and are recorded in place: D3's permitted key set (the gate found two violations the RFC had not anticipated) and §5's negative control (built the way the shipped gate-selftest builds them, not as a new admin function).
- **Phase:** 3
- **Owner:** project author
- **Created:** 2026-08-06 · **Accepted:** 2026-08-06 · **Frozen:** 2026-08-06

## 1. Context

The plan makes RFC-011 a Phase 3 RFC and describes it as "the provenance model: authors, sources, confidence". RFC-004 A4 leans on it harder: `revision.meta` is declared "a stopgap contract: RFC-011 supersedes it and MUST define the migration of accumulated meta."

Most of what that sentence anticipated has already happened, and this RFC is correspondingly small. Saying so is the point: an RFC padded to match the size of its slot is worse than one that names what is genuinely undecided.

**Already shipped, and not re-decided here.** `revision.author`, `revision.source`, `revision.message` and `revision.created_at` exist (RFC-003 D3). `block_revision.confidence` and `.bind` exist and are written (RFC-005 H2, RFC-004 Part B). `knowledge.history` exposes author, source, message and created_at; `knowledge.blame` exposes author, source and confidence per block. Provenance is stored *and readable*. RFC-005 D11 already moved `meta.minted` / `.removed` into typed `block_revision` columns.

**What is actually broken.** `author` defaults to `current_user` and **nothing can set it**. In a vault whose whole premise is many agents sharing one brain, every revision is attributed to a database role — which is usually one role for all of them. The column is present, populated, and useless for the question it exists to answer. `message` has the same shape: the column exists and no code path can fill it.

**What this RFC must therefore decide:** how a writer says who it is, what a `message` is for and why it is not being added to signatures today, what remains of `revision.meta`, and what a consumer is allowed to conclude from a confidence score. Nothing here changes storage.

---

## 2. Decision

### D1. `author` is an assertion, carried in a session GUC

```sql
SET pgmind.author = 'agent:planner@run-8f21';
```

`pgmind.author` is a `Userset` string GUC, exactly like `pgmind.vault_id` (RFC-003 D1). When set and non-empty, every revision written by that session records it as `revision.author`. When unset, `author` keeps its current default of `current_user`.

- **It is ambient, not a parameter.** Author is a property of *who is connected*, not of one write, so it belongs where `vault_id` already lives. Threading it through ten function signatures would say the opposite.
- **It is testimony, not proof, and the documentation MUST say so in those terms.** Postgres authenticated the *role*; the agent name is a claim that role makes about itself. Any session that can write can claim any author. This is the same honesty `^id` markers get in RFC-004 A5 — user assertion carried in the text, honored as written, never mistaken for identity the system derived.
- **It MUST NOT be validated, namespaced, or made to look authenticated.** No registry of known agents, no `agent:` prefix enforcement, no foreign key to a principals table. Each of those makes a claim look verified without verifying it, which is worse than the plain string.
- Length is capped at **200 characters** — characters, not bytes, since a limit that cuts a multi-byte name in half produces a different author. Over the cap raises **PM017 `pgmind_invalid_author`**, extending the PM class of RFC-004 A6 and RFC-005 D10. *Named precisely at implementation; this line said "PM001's class of loud failure", which is not a code an implementer can raise.* The check runs before the INSERT, so a rejected write leaves no revision row behind. Silent truncation is the alternative and it is worse: it writes a wrong author and the failure is unobservable afterwards.
- RFC-007 wires MCP sessions to set this GUC per agent connection. That is plumbing, and it is RFC-007's; the mechanism is decided here so RFC-007 has something to wire *to*.

### D2. `message` is per-write, and deliberately has no way in yet

A message describes one edit, not a session, so the GUC shape of D1 is wrong for it, and a "consume-once GUC" that clears itself after the next write is exactly the hidden behaviour Law 9 forbids. It belongs as an optional trailing parameter on the mutating functions.

**It is not being added today, and the column stays NULL.** Ten signatures would change, and function signatures are precisely what RFC-007 freezes at Phase 5; changing them twice is worse than changing them once, and no consumer reads `message` yet. What this RFC decides is the *model*, so the eventual parameter cannot be got wrong:

- `message` MUST be caller-supplied or absent. It MUST NOT be inferred, auto-generated, defaulted to the verb, or synthesized from a diff. **An empty message is more honest than a manufactured one** — a field that is always populated stops carrying information, and a reader who sees "updated block" learns nothing while believing they learned something.
- `message` is prose for humans. Nothing in pgmind may parse it, key on it, or change behaviour because of its contents.

Until RFC-007, `history().message` is NULL for every revision, and the documentation says *unfilled*, not *unsupported*.

### D3. `revision.meta` stays, scoped — the migration RFC-004 A4 promised is not needed

RFC-004 A4 anticipated that RFC-011 would supersede `meta` and migrate what had accumulated in it. Most of that already happened by a different route: RFC-005 D11 retired `meta.minted` and `meta.removed` into typed `block_revision` rows, because those were per-block facts stuffed into a per-revision blob.

What remains in `meta` is genuinely op-shaped and has no typed home: the `split` / `merge` objects (which block became which, and where the marker landed) and Part B's `rebind` summary. These describe a *relationship between blocks created by one operation* — not an attribute of any single row — and inventing a table for a two-key object nothing joins against would be worse.

Normative, going forward:

- `meta` is for op-shaped provenance only. **Anything that is a fact about one block or one revision MUST get a typed column instead**, and an addition to `meta` MUST state in its RFC why a column was rejected.
- The 200-entry cap and `"truncated": true` behaviour (A4) stands.
- The permitted key set is exactly: `op`, `carried`, `split`, `merge`, `rebind` (RFC-004 A4 and Part B), `target` (which block a block op addressed — the `block_revision` rows say which blocks *changed*, not which one the caller named), and `move` (`{from, to}` for `move_note`, the one place a rename is readable without joining the pre-image lane).
- **No migration of accumulated `meta` is required**, and this RFC discharges A4's obligation by narrowing it rather than performing it.

*Amended at implementation, 2026-08-06 — the gate below found this before a human did.* `delete_note` was writing `{"deleted": true}`, which is a per-revision fact that `verb = 'delete_note'` already carries: precisely what the first rule above forbids. It is removed rather than added to the permitted set. Rows written before this change still carry the key; the gate runs against fresh databases, so the discrepancy is invisible in CI and belongs to RFC-012's upgrade script, which MUST either strip it or widen the check for historical rows. Naming it here so it does not arrive as a surprise.

### D4. What `confidence` means, and what a consumer may conclude

`block_revision.confidence` is NULL for identity that was *known* and non-NULL for identity that was *inferred* (RFC-004 Part B). The value is the aligner's similarity score.

- **The boolean is the contract; the number is not.** A consumer deciding anything about correctness — whether to trust a citation, whether to re-embed, whether to show a warning — MUST branch on `confidence IS NOT NULL`, never on a threshold.
- **Scores are not comparable across versions.** The feature set behind them has already changed once (bigrams → unigrams ∪ bigrams, RFC-004 B2) and the threshold with it. A stored 0.62 from an older version does not mean what a fresh 0.62 means. Consumers MAY use the number for ranking and telemetry within one vault at one version; they MUST NOT persist decisions keyed to it.
- **It is not a probability.** It is Dice overlap. It does not estimate how often the binding is right, and no calibration exists to make it one.
- `bind` is the categorical companion and the one worth reading: `mint` / `ref` / `hash` / `carry` / `rebind` / `remove`.

### D5. What provenance in pgmind is not

Stated because each of these is a thing readers assume, and every one of them is false:

- **Not authentication.** D1.
- **Not tamper-evident.** Anyone with write access to schema `pgmind` can alter a revision row. There are no hash chains, no signatures. Adding them is a different RFC with a different threat model, and pretending otherwise would be the most damaging thing this document could do.
- **Not a compliance audit trail.** Excision (RFC-005 D7) can and must remove content from history, `retain` can compact it away, and `note.history_floor` records that some past is simply gone. A record that can be lawfully erased is not an audit log, whatever it looks like.
- **Not a per-block author.** `author` is recorded per revision. A block's "who last touched it" comes from its last changing revision, which `blame` already computes — one hop, no duplication.

---

## 3. Alternatives considered

- **`author` as a parameter on every mutating function** — rejected: it is a property of the connection, not the edit, and ten extra parameters would tell the reader otherwise. `vault_id` set the precedent, and inconsistency between two ambient facts would be its own bug.
- **An authenticated principals table with an FK from `revision.author`** — rejected for Phase 3: it makes a claim *look* verified while the verification would still be "whatever the session typed", since pgmind has no channel to authenticate an agent independently of its database role. A real answer needs RFC-007's connection model, and a fake one is worse than a plain string.
- **A consume-once `pgmind.message` GUC** — rejected: state that silently changes after an unrelated call is exactly the hidden behaviour Law 9 exists to forbid, and it would misattribute a message the moment a caller wrote twice.
- **Auto-generating `message` from the verb and the diff** — rejected on the grounds in D2: a field that is always full carries no signal, and the generated text would be a worse version of what `history()` already returns in typed columns.
- **Calibrating `confidence` into a probability** — rejected as premature: calibration needs labelled outcomes at a scale one 42-case corpus cannot supply, and an uncalibrated number presented as a probability invites exactly the threshold-keyed decisions D4 forbids.
- **Deferring this RFC to Phase 5 with RFC-007** — rejected: `author` is unusable *today*, every revision written before it lands is permanently attributed to a role rather than an agent, and that history cannot be repaired retroactively. The part that genuinely needs RFC-007 is the wiring, not the mechanism.

## 4. Consequences

*Easy after this.* Attributing revisions to agents; `blame` answering "which agent wrote this, and did it know or guess". RFC-007 wiring MCP sessions to a GUC that already exists and already flows.

*Harder after this.* `author` is free text forever, or until an RFC introduces a verified identity alongside it — not in place of it, since old rows cannot be re-verified.

*Impossible without a new RFC.* Treating provenance as evidence: tamper-evidence, signing, or any claim of immutability, all of which collide with RFC-005 D7's erasure by design.

*Reversal cost.* Low. Nothing here changes storage; withdrawing D1 means ignoring a GUC.

## 5. Benchmark gate

**`provenance-integrity`** in `eval/`, added to the Phase 3 set. Failable assertions, all of them zero-tolerance because each is a contract and not a measurement:

| assertion | why it can fail |
|---|---|
| Every revision in a seeded workload has a non-empty `author`. | The column is `NOT NULL DEFAULT current_user`; a write path that sets it explicitly can set it to `''`. |
| With `pgmind.author` set, `history().author` and `blame().author` return it, for every verb — including the block ops, `move_note`, `undelete_note` and `append_to_section`, not just `write`. | Attribution that works for one entry point and silently drops for nine is the likely bug, and it is invisible unless every verb is checked. |
| With the GUC unset or empty, `author` falls back to `current_user`. | An empty GUC must not produce an empty author. |
| An author over the cap is rejected loudly; no revision row is written. | Silent truncation writes a wrong author and the failure is unobservable afterwards. |
| Every block bound by Part B has `confidence IS NOT NULL AND bind = 'rebind'`; every deterministically carried block has `confidence IS NULL`. | If everything gets a confidence, confidence stops meaning anything — the exact failure D4 legislates against. |
| `revision.meta` contains no key outside the A4 schema plus `rebind`. | The rule in D3 is only real if something enforces it. |

**Negative control** (RFC-005 §5.0(b)). *Amended at implementation:* this RFC specified a `pgmind.break_provenance` admin function. The shipped `gate-selftest` suite breaks its subjects with direct SQL instead, and no fault-injection function has ever been built — deliberately, because an admin surface whose only purpose is corrupting data is real attack surface in a production extension, and the existing suite proves you do not need one. The control follows that precedent: it nulls a rebind's confidence, gives a deterministic carry a confidence, and empties an author, asserting the checker notices each. **The second of those is the one worth having** — an unmarked rebind is an obvious bug, while a confident deterministic carry silently destroys the distinction the column exists to draw.

The gate deliberately measures no *quality* of provenance. There is no honest metric for "is this attribution true", and inventing one would contradict D1.

## 6. Law compliance

- **Law 9 (feel like PostgreSQL, no hidden behaviour)** — the load-bearing law here. A session GUC that a caller sets and can read back is Postgres-shaped; a consume-once GUC or an auto-written message would not be, and both are rejected above for that reason.
- **Law 4 (parsing never yields identity)** — untouched: `confidence` describes a binding RFC-004 made, and this RFC only constrains how it may be read.
- **Law 8 (append-only with audited excision)** — D5 states the consequence honestly instead of claiming an immutability that erasure contradicts.
- **Law 11 (layers by contract)** — the fault-injection function is admin-only and refused unless `pgmind.allow_fault_injection` is on, like every other one.
- **Laws 1 and 2 (AI-free, no network I/O)** — untouched; nothing here calls anything.
