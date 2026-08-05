# RFC-005: Version Engine, Concurrency Semantics & Excision

- **Status:** Draft — proposed for acceptance to open Phase 3
- **Phase:** 3
- **Owner:** project author
- **Created:** 2026-08-05 · **Accepted:** — · **Frozen:** —

## 1. Context

Phase 2 shipped a vault that remembers only *now*. [RFC-003](RFC-003-vault-and-block-storage-layout.md) says so in its title — Law 8 deferred, current-state storage until this RFC — and its write path is honest about the consequence: a block that leaves a note is `DELETE`d, a merge retiree's rows are `DELETE`d, and `pgmind.revision` carries an author, a source and a timestamp but **no content at all**. Every `revision` row today is a receipt for a change nobody can reconstruct.

This RFC implements handbook **Law 8 — "Append-only with audited excision. Revisions are inserts; excision is explicit, logged, and policy-driven."** That law is already the qualified form of a claim the audit refused to let stand: [AUDIT C4](../../AUDIT.md) found that *every* shipped immutable store was forced to add erasure — Datomic excision, Dolt 2.0 GC, XTDB 2.x, TerminusDB squash — for storage economics and for the right to erasure. A version engine that cannot forget is not shippable, and one that forgets silently is worse. Both halves are normative here.

It also implements the second thing Phase 3 exists for: **many writers, zero silent clobbers** ([plan §16](../PRODUCT-PLAN.md)). Phase 2's write path takes a transaction-scoped advisory lock per `(vault, path)`, which serializes writers to the same note but tells a caller nothing: a client that read a note, thought about it, and wrote it back cannot learn that someone else changed it in between. Its edit silently wins. For a vault whose primary clients are agents running concurrently, that is the defect this phase must close.

**Scope boundary.** Identity — which block keeps which UUID — is [RFC-004](RFC-004-block-identity-and-rebinding.md)'s, and its Part B (heuristic rebinding) is a *separate* Phase 3 deliverable with its own gate; this RFC provides the columns Part B's confidence and binding-kind live in and nothing more. Rich provenance (authors, sources, confidence semantics) is RFC-011. Sync is RFC-006. What is decided here: what a revision physically stores, how history is read, what concurrent writers observe, how removal becomes history, how erasure works, and how history is bounded.

---

## 2. Decision

### D1. History is a ledger of pre-images, anchored at current state

Two candidate shapes lost to this one (§3). The decision:

- **Current state is the anchor.** `pgmind.block` and `pgmind.tile` continue to hold *now*, exactly as RFC-003 D2 defines them. `knowledge.read()` at head is unchanged and costs what it costs today — measured p95 **0.454 ms** ([capacity-model-v1](../../eval/published/capacity-model-v1.json)) — because reading the present must never pay for the past.
- **History stores pre-images.** A revision records what the two lanes held *before* it. Reconstructing `as_of(T)` starts from a known-good state and applies pre-images backwards to `T`.
- **Two history lanes mirror the two storage lanes** (RFC-003 D2), so every history row has an obvious current-state counterpart and `verify_history` can compare them.

Two invariants are normative, and both exist to protect erasure:

> **X1 (locality).** No history row may define its bytes by reference to another history row's bytes. Every stored payload is literal.
>
> **X2 (determinism without the parser).** Reconstruction MUST NOT invoke the markdown parser. Structure at revision `T` — which block ids exist, in what order, at what spans — is read from stored vectors, never re-derived.

X1 is what makes erasure a bounded, local rewrite instead of a chain re-base: overwrite the literal bytes and nothing else needs re-encoding. It is stated as an invariant rather than left implicit precisely so that a future "let's delta-encode the pre-images" optimization has to come back to this RFC first. X2 is what makes history survive pgmind itself: [RFC-002](RFC-002-markdown-type-ast-vault-syntax.md) §Consequences contemplates amending the parser or the hash, and a design that re-parses old bytes to recover old structure would silently re-interpret history at every such amendment.

### D2. Normative DDL

Three new tables, plus declared amendments to RFC-003's (§D11). All are registered with `pg_extension_config_dump` — RFC-003 D3 learned that extension-script tables are otherwise invisible to `pg_dump`.

```sql
-- H1: per-revision note-level pre-image. One row per revision, always.
CREATE TABLE pgmind.note_revision (
  revision_id   uuid PRIMARY KEY REFERENCES pgmind.revision(id),
  note_id       uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id      uuid NOT NULL,
  seq           bigint NOT NULL,          -- per-note, dense, ascending (D3)
  prev_preamble text,                     -- NULL ⇒ unchanged by this revision
  prev_props    jsonb,
  -- Pre-image scripts. ops is a packed int4[] of (KEEP a b | INS k) instructions;
  -- payloads hold only what INS introduces, literally (X1).
  tile_ops      int4[] NOT NULL,  tile_payload  text[] NOT NULL,
  id_ops        int4[] NOT NULL,  id_payload    uuid[] NOT NULL,
  place_prev    int4[] NOT NULL,          -- (tile_ord, start, end) triples, changed slots only
  place_idx     int4[] NOT NULL,
  UNIQUE (note_id, seq)
);

-- H2: per-block pre-image. One row ONLY for blocks whose content-visible columns changed.
CREATE TABLE pgmind.block_revision (
  note_id           uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  block_id          uuid NOT NULL,        -- deliberately no FK: the block may be gone
  seq               bigint NOT NULL,
  vault_id          uuid NOT NULL,
  existed           boolean NOT NULL,     -- false ⇒ minted by this revision (no pre-image)
  prev_kind         pgmind.block_kind,
  prev_content      text,
  prev_content_hash bytea CHECK (prev_content_hash IS NULL OR octet_length(prev_content_hash) = 32),
  prev_heading_path text[],
  prev_block_ref_id text,
  prev_attrs        jsonb,
  prev_parent_block uuid,
  confidence        real,                 -- RFC-004 Part B: NULL ⇒ deterministic binding
  bind              text CHECK (bind IN ('mint','ref','hash','carry','rebind','remove')),
  PRIMARY KEY (note_id, block_id, seq)
);

-- H3: periodic absolute snapshot, so deep as_of is bounded (D3).
CREATE TABLE pgmind.note_frame (
  note_id   uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  seq       bigint NOT NULL,
  vault_id  uuid NOT NULL,
  preamble  text NOT NULL,
  props     jsonb NOT NULL,
  tiles     text[] NOT NULL,
  block_ids uuid[] NOT NULL,
  placement int4[] NOT NULL,
  PRIMARY KEY (note_id, seq)
);

-- H4: the audit trail erasure owes (Law 8). Never mutated by any pgmind code path.
CREATE TABLE pgmind.excision_log (
  id            uuid PRIMARY KEY,
  vault_id      uuid NOT NULL,
  requested_at  timestamptz NOT NULL DEFAULT now(),
  requested_by  text NOT NULL DEFAULT current_user,
  reason        text NOT NULL,
  target        jsonb NOT NULL,           -- as given; carries no erased content (D7)
  scope         jsonb NOT NULL,           -- counts per lane, notes/revisions touched
  escalations   jsonb NOT NULL DEFAULT '[]'::jsonb,   -- tile-level fallbacks (D7)
  verified_at   timestamptz,
  survivors     int4                      -- NULL until verified; 0 = proven
);

CREATE INDEX block_revision_block ON pgmind.block_revision (block_id, seq DESC);
CREATE INDEX note_revision_note   ON pgmind.note_revision (note_id, seq DESC);
CREATE INDEX revision_parent      ON pgmind.revision (parent) WHERE parent IS NOT NULL;
```

The last index discharges an item RFC-003 D4 explicitly deferred to this RFC: `revision.parent` was left unindexed because Phase 2 had no deletion path, and this RFC introduces several.

**`REVOKE ALL … FROM PUBLIC`** on every function in D7 and D8; they are Law 11 admin surfaces. `excision_log` carries **no trigger** guarding mutation: no pgmind code path writes it after insert, the table is revoked from `PUBLIC`, and a superuser can alter any table in their own database — a trigger would be theatre that suggests a guarantee the extension cannot make.

### D3. Reading history

`seq` is per-note, dense and ascending, assigned under the note row lock (D5); it is what makes "before" a total order without trusting clocks. `note.head_revision` continues to name the current revision; `note.history_floor bigint NOT NULL DEFAULT 0` (§D11) names the oldest `seq` whose state can still be reconstructed.

```sql
knowledge.history(path text, limit_n int DEFAULT 50)
  → TABLE (revision uuid, seq bigint, verb text, author text, source text,
           message text, created_at timestamptz, blocks_changed int)
knowledge.read_as_of(path text, at anyelement)      → markdown
knowledge.blocks_as_of(path text, at anyelement)
  → TABLE (block_id uuid, ord int, kind text, content text, heading_path text[])
knowledge.diff(path text, from_at anyelement, to_at anyelement)
  → TABLE (block_id uuid, change text, before text, after text)   -- added|removed|changed|moved
knowledge.blame(path text)
  → TABLE (block_id uuid, ord int, first_revision uuid, last_changed_revision uuid,
           author text, source text, confidence real, changed_at timestamptz)
```

`at` is a `uuid` (revision), a `bigint` (seq) or a `timestamptz` (the latest revision at or before that instant) — one overload per type, no string parsing.

**Reconstruction.** Start at the nearest anchor at or above `T` — the current state, or a `note_frame` with `seq ≥ T` — and apply the pre-image scripts of every revision above `T` in descending `seq`. Cost is bounded by the frame cadence `pgmind.frame_every` (USERSET, default **50** revisions), not by the note's total depth. `blocks_as_of` zips the reconstructed id vector against the reconstructed placement vector and the block pre-images; it applies the same `tile_ord` range guard `knowledge.blocks()` applies today (RFC-003 D7, amended after review) — a historical row is not more trustworthy than a live one.

`blame` reads the newest `block_revision` row per `block_id` at or below head. Because position lives in the per-revision vectors and **not** in the effect row (D4), "newest effect row" means "last content change" directly, with no window function and no filtering of moves.

A read below `note.history_floor` raises **PM011**, always. It never returns a partially-reconstructed document: the failure mode this RFC most wants to prevent is an agent citing a revision and receiving a plausible, wrong one.

### D4. What each write records

The write path (RFC-003 D6 steps 5–8) and the five block ops gain one step: **before mutating a lane, record its pre-image.** Rules, normative:

- **One `note_revision` row per revision, always** — it carries the id and placement vectors, which is what makes `blocks_as_of` work without the parser (X2).
- **A `block_revision` row only when a content-visible column changed**: `kind`, `content`, `content_hash`, `heading_path`, `block_ref_id`, `attrs`, `parent_block`. **`ord`, `tile_ord`, `start_in_tile` and `end_in_tile` never produce an effect row** — position is carried by the per-revision vectors. This is the single most consequential rule in the RFC's economics: inserting a block at the top of a 100-block note, or renaming a heading that shifts nothing but `ord`, would otherwise write 100 full history rows for an edit whose semantic content is one block. Structural churn is exactly the workload Phase 4's importer produces, so a design that is O(note) per structural edit fails on the traffic it was built for.
- **Removal stops being `DELETE`** (D6).
- **`revision` gains `verb`** (`write|insert_blocks|update_block|move_block|split_block|merge_blocks|delete_note|undelete_note|move_note|excise|retain`) so `history()` is readable without reconstructing anything.
- **RFC-004 A4's `meta.minted` / `meta.removed` arrays are retired** into the history lanes, which now hold the same facts in typed columns (§D11). Today they make the average `revision` row **1,170 B of heap** — measured, [capacity-model-v1](../../eval/published/capacity-model-v1.json) — which at the design target is 8.85 GB of jsonb duplicating what H1 and H2 store properly.

The **Determinism Rule**: reconstruction reads stored bytes and stored vectors. It never calls comrak, never recomputes a hash to decide structure, and never depends on the block taxonomy of the *current* build. `revision.meta.parser_epoch` stamps the parse/hash generation that produced a revision — provenance for a future rehash migration (RFC-012), not a precondition, because nothing in the read path re-parses.

### D5. Concurrency: compare-and-swap, and what a writer observes

```sql
knowledge.write(path text, doc markdown, expected_head uuid DEFAULT NULL) → uuid
-- and expected_head on every mutating op, same default, same semantics
```

Normative:

1. **Serialization is the note row.** Every mutating operation takes `SELECT … FROM pgmind.note WHERE id = $1 FOR NO KEY UPDATE` on its target note. It is the right tool where Phase 2 used an advisory lock: it is released by the transaction, it blocks writers without blocking readers, and it makes `pg_blocking_pids()` show the wait — which is what lets the concurrency gate test interleavings without sleeping. The advisory lock survives for **note creation only**, where there is no row to lock yet.
2. **The path is re-checked under the lock.** Looking a note up by path and then locking it is a race against a concurrent rename: after acquiring the lock the write path MUST re-read `path` and raise **PM002** if it changed. (Phase 2 has no rename, so this is new surface, not a latent bug.)
3. **CAS precedes idempotence.** When `expected_head` is non-NULL and differs from the observed `head_revision`, raise **PM009** — *even when the incoming bytes are identical to the stored bytes*. RFC-003 D6 step 2's byte-identical short-circuit answers "did anything change"; CAS answers "did you see what you were changing." Reordering them would make a stale writer's no-op silently succeed.
4. **`expected_head` NULL means last-writer-wins**, explicitly and by the caller's choice. Phase 2 semantics are preserved for callers that have no head to assert.
5. **The extension never raises `40001`/`40P01` itself, and never retries internally.** Conflicts pgmind detects are `PM0xx` with the observed head in the error detail, so a client can re-read and retry deterministically. Serialization failures Postgres raises under REPEATABLE READ or SERIALIZABLE pass through untouched — an extension that swallowed them would break the caller's own retry loop.
6. **Multi-note operations lock in ascending `note.id` order**, so two writers touching the same pair cannot deadlock against each other.
7. **`knowledge.append_to_section(path, heading_path text[], fragment markdown, expected_head uuid DEFAULT NULL)`** appends after the last block of the named section. Two concurrent appends serialize on the note row and **both survive, in lock-acquisition order**; append is the one operation for which a conflict is not a conflict, and saying so is the point of having it as an operation rather than a read-modify-write.

### D6. Removal, tombstones, and note lifecycle

```sql
knowledge.delete_note(path text, expected_head uuid DEFAULT NULL)   → uuid
knowledge.undelete_note(path text)                                  → uuid
knowledge.move_note(path text, new_path text, expected_head uuid DEFAULT NULL) → uuid
```

- A block that leaves a note no longer has its row `DELETE`d without trace: the removal writes a `block_revision` pre-image with `bind='remove'` before the row goes. The `block` row itself is still removed — current state stays current state — but the content is recoverable at any `seq` above the floor.
- `delete_note` sets `note.tombstoned_at` (the column RFC-003 D3 shipped and nothing has ever written) and records a revision with `verb='delete_note'`. Tiles, blocks, edges and tags for a tombstoned note are removed from the live lanes; history keeps them. `undelete_note` reconstructs from history and clears the tombstone.
- `move_note` is a path change plus the D5 creation-repair pass (RFC-003 D5) re-run for **both** the old and new paths, since a rename can promote and demote basename resolution in one step. Edges are rewritten, never guessed.
- Blocks belonging to a tombstoned note are excluded from every read API, as they are today.

### D7. Excision

Erasure is the half of Law 8 that a version engine most easily fakes. The obligations are: erase from *every* surface, including derived ones; refuse rather than half-erase; log the erasure; and prove it.

```sql
pgmind.excise(target jsonb, reason text, and_head boolean DEFAULT false,
              dry_run boolean DEFAULT true) → uuid          -- excision_log.id
pgmind.verify_excision(excision uuid) → SETOF text          -- empty ⇒ proven
pgmind.enforce_excisions()            → int                 -- post-restore replay
```

`target` is `{"note_id":…}`, `{"path":…}`, `{"block_id":…}`, `{"revision":…}`, `{"path":…,"before":…}` or `{"literal":"…"}` (scanned within the caller's vault). `dry_run` defaults to **true** and returns the scope it *would* erase; erasure is never the accidental outcome of a typo.

**Mechanics.**

1. **Live content is refused, not silently spared.** If the target still exists at head, `excise` raises **PM012** unless `and_head => true`, in which case it first performs an ordinary audited write that removes the content (a real revision, with `verb='excise'`), and only then erases history. Erasure that quietly left the current copy in place would be the worst possible outcome of a right-to-erasure request.
2. **Byte-lane redaction is a splice, not a `NULL`.** A tile pre-image holds a whole top-level child — an entire list, table or code fence — so nulling it would erase every *other* block that shared the tile and would leave `read_as_of` reassembling a document with a hole. Instead the erased span is replaced, through the same splice machinery as RFC-003 D6, with a marker (`⟨redacted pgmind:‹excision-id›⟩`), and the post-splice PM008 assertion runs on the result. Redacted history therefore still reconstructs to byte-defined, parseable markdown.
3. **Escalation is deterministic, never refusal.** Where the marker cannot preserve the tile's block count — inside a table row, a tight nested list, a fenced block — the redaction escalates to **whole-tile redaction** (the tile becomes a single marker paragraph) and the escalation is recorded in `excision_log.escalations`. A marker that changes a tile's block count would silently re-align the id vector at revisions the excision never examined, reassigning block identity across history: a Law 4 violation with no error. Escalation makes the failure impossible by construction; recording it makes the collateral visible.
4. **Semantic-lane erasure nulls `prev_content`, `prev_content_hash`, `prev_attrs`, `prev_block_ref_id`, `prev_heading_path`** on matching effect rows, keeping the row skeleton so history still knows *that* a block existed and when. Hashes go too: BLAKE3 of a short block is a confirm-a-guess oracle, and RFC-002 D7's hash is BLAKE3 — any tooling that sweeps for SHA-256 is checking the wrong thing.
5. **Derived surfaces are erased in the same transaction**: `edge` rows whose `dst_path`/`alias` carry the text, `tag` rows, `revision.message`, `revision.meta`, and `note.path`/`preamble`/`properties` where the target is a note. The sweep enumerates columns from `information_schema.columns` **at call time**, so a table added by a later RFC is swept by construction rather than by remembering.
6. **Verification is a whole-note re-reconstruction, and the RFC does not pretend otherwise.** `verify_excision` walks *every* surviving revision of every affected note, reconstructs it, and asserts (a) the canary text appears in no column of any pgmind table, (b) every revision reconstructs to parseable bytes, (c) the id vector length equals the parsed block count at every revision. That is O(revisions per affected note), not O(1); excision is a rare, admin-initiated, audited operation and buying certainty with its runtime is the right trade. `survivors > 0` raises **PM013** and the transaction aborts — an incomplete excision is never committed.
7. **Restore is a hostile environment.** A `pg_dump` taken before an excision restores the erased content. `enforce_excisions()` replays every `excision_log` row against the restored database and returns the number of surviving hits it erased; the `dump-restore` gate runs it. The RFC states plainly what pgmind cannot do: it cannot reach backups it does not know about, and no in-database mechanism can.

### D8. Retention and compaction

```sql
pgmind.retain(keep_revisions int DEFAULT NULL, keep_since interval DEFAULT NULL,
              keep_sources text[] DEFAULT NULL, vault uuid DEFAULT NULL,
              dry_run boolean DEFAULT true) → TABLE (note_id uuid, seq_floor bigint, rows_removed bigint)
```

A **function with parameters, not a stored policy catalog**: there is no persisted mode that can be toggled into deleting more than the caller asked for, and no background job that runs when nobody is watching. `dry_run` defaults to true.

- Compaction writes a `note_frame` at the new floor, then deletes `block_revision` and `note_revision` rows below it, then advances `note.history_floor`.
- **`pgmind.revision` rows are never deleted by retention.** `history()` keeps listing every revision — id, author, source, verb, timestamp — long after its content has been compacted away. A vault that forgot must remember that it forgot; and a client holding a compacted revision id gets **PM011** ("no longer reconstructable"), never PM010 ("no such revision"), because those two mean opposite things to whoever has to debug it.
- `keep_sources` lets low-value history age faster than authored history — `source='rebind'` is the intended case (RFC-004 Part B).
- Post-condition, asserted by `pgmind.verify_history(note_id)`: for every `(note_id, block_id)` with any surviving effect row there is exactly one row at or below the floor, `seq` is dense and gapless above the floor, and `read_as_of(head) = read()` byte-for-byte.

### D9. Capacity: what this costs, and what is not yet known

From the published Phase 2 measurements ([capacity-model-v1](../../eval/published/capacity-model-v1.json)): block 429.9 B/row all-in, tile 180.0, revision 1,258.3, and 657.9 B/block all-in across the whole schema at 10k notes / 230k blocks.

The honest form of this section is a ratio with a named free variable. History size is **linear in effect rows per revision**, and that quantity is *unmeasured*: the modal edit's shape is a property of agent traffic, not of this design. At one changed block and one changed tile per revision it is ≈870 B/revision, giving ~1.9× current state at the plan's design target (100k notes / 10M blocks / 100 revisions). At the other extreme — whole-document rewrites, which is exactly what RFC-006's importer and a naive read-edit-write agent loop produce — it degenerates toward full copies. **The `storage-growth` gate measures the histogram first, and the multiplier is published against it** (§5). Publishing a single number without its denominators is how this section would lie.

**No partitioning in v1.** Extension-owned partitioned tables were tested against RFC-003 D3's dump/restore contract on PG 18.4 with three outcomes worth recording: partitions created *in the install script* vanish from `pg_dump` entirely (silent, total data loss); partitions created at runtime dump and restore correctly, but a partitioned index on an extension-owned parent breaks plain-`psql` restore (`relation … already exists`); and RLS on a partitioned parent is **bypassed by selecting from a leaf directly**, which is a tenancy hole (RFC-003 D1) that depends on grant ordering. Retention is therefore `DELETE`-based, with the honest consequence stated: deleted history returns space to the heap for reuse, not to the operating system. A future partitioning RFC must re-run these experiments across PG16/17/18 with a written acceptance criterion.

### D10. Error codes

Continuing RFC-004 A6's class (PM001–PM008):

| code | name | meaning |
|---|---|---|
| PM009 | `pgmind_stale_head` | `expected_head` is not the note's current head. Detail carries the observed head; retry by re-reading. |
| PM010 | `pgmind_unknown_revision` | The given revision is not a revision of this note. A client bug, not a race. |
| PM011 | `pgmind_history_unavailable` | The requested point is below `note.history_floor` — compacted or excised. Distinct from PM010 on purpose. |
| PM012 | `pgmind_excision_refused` | The target is live at head and `and_head` was not given. |
| PM013 | `pgmind_excision_incomplete` | Verification found survivors; the excision transaction aborts. |
| PM014 | `pgmind_note_tombstoned` | A mutating operation targeted a deleted note. |

### D11. Declared amendments to frozen and accepted RFCs

Precedence rules require these to be explicit, not incidental:

- **RFC-003 D3 (frozen) — `pgmind.note` gains `history_floor bigint NOT NULL DEFAULT 0`; `pgmind.revision` gains `seq bigint NOT NULL` and `verb text NOT NULL`.** `revision.source`'s CHECK is unchanged, but `'sync'` and `'rebind'` — permitted since Phase 2 and produced by nothing — become reachable, and `source` becomes caller-settable through RFC-006 and RFC-004 Part B rather than a hardcoded `'api'`.
- **RFC-003 D4 (frozen) — `revision.parent` gains the index D4 deferred to this RFC by name.**
- **RFC-003 D6 (frozen) — removal is no longer a bare `DELETE`**; the pre-image write precedes it, and the normative INSERT/UPDATE-before-DELETE ordering extends to it.
- **RFC-004 A4 (accepted) — `revision.meta.minted` / `.removed` are retired** in favour of `block_revision` rows carrying the same facts in typed columns; `meta` keeps the split/merge provenance objects. The pg_test `merge_without_keep_records_provenance` pins the old shape and changes with this RFC.
- **Signature changes** — adding `expected_head` to seven existing `knowledge.*` functions cannot be done with `CREATE OR REPLACE` across an arity change; the extension upgrade script must `DROP FUNCTION` then `CREATE`. That is RFC-012's problem and it is named here so it does not arrive unannounced.

---

## 3. Alternatives considered

- **Shadow rows — one immutable copy of every changed block and tile row, keyed by `(note_id, block_id, seq)`.** The simplest design and the easiest to explain to a DBA; rejected because history rows become O(note size) for a *structural* edit rather than O(edit size): an insert at ord 0, or a heading rename, rewrites every following block's `ord`/`heading_path` and therefore writes a full history row for each. Modelled against the published per-row costs, the same code lands anywhere between 2× and ~65× current state depending on workload mix — 8 GB or 420 GB at the design target — with the bad end being precisely the importer traffic Phase 4 generates. Its good ideas were taken: retention as a parameterized function rather than a policy catalog, no trigger on the audit table, `dry_run` by default, and a column sweep enumerated at call time.
- **Positional tile deltas with keyframes (the plan's own sketch).** Storage-optimal on paper and the source of this RFC's locality invariant (X1) and its splice-based redaction. Rejected as the spine because pgmind's tiling granularity was chosen for *parsing*, not for delta size: a tile is a whole list, table or code fence, so "changed one bullet" stores every bullet. On the published corpus a single eight-item list is 36% of its note, and real agent vaults skew harder that way — long tables, long checklists, an append-only "## Log". Its compression advantage evaporates exactly where the design promised it.
- **Operation log without pre-images (replay-forward).** Rejected: replay must reproduce the parser's behaviour forever, so history becomes hostage to comrak's version and to RFC-002 amendments the frozen RFC explicitly contemplates. Anchoring at current state with pre-images keeps the same blame-for-free property (the verb and author are on the revision) without the forward obligation.
- **CRDTs / operational transformation for concurrency.** Rejected on Law 4: identity in pgmind is *asserted by the write path*, not inferred from a merge lattice, and a CRDT would make every block's identity a function of replica history. CAS plus explicit conflict is the semantics an agent runtime can reason about; three-way merge is a client's business, not a storage engine's.
- **Block-granular excision only, without whole-note support.** Rejected: right-to-erasure requests arrive as "this person", not "this paragraph", and a note's path (`people/jane-doe`) is itself low-entropy identifying data.
- **Hashing the excision target so an auditor can confirm what was erased.** Rejected: an unsalted digest of a low-entropy string (a path, a name) *is* the identifying data, which is the pseudonymisation failure the erasure literature is built on. The log records who, when, why and how much — not what.

---

## 4. Consequences

**Easy after this.** Time-travel reads, `blame`, and diffs without a second representation of the present. Agent-safe concurrent writing with a conflict a client can act on. Undelete. A rebinding pipeline (RFC-004 Part B) that can write its confidence somewhere real. A retention story that does not require partitioning to be safe.

**Harder after this.** Every mutating code path now has a pre-image obligation, and `verify_history` becomes as load-bearing as `verify_note`. Deep `as_of` costs frame-cadence steps, so `frame_every` is a tuning knob with a real trade (storage vs deep-read latency) that the gate measures rather than assumes.

**Impossible without a new RFC.** Delta-encoding history payloads against each other (X1 forbids it, and erasure is the reason). Re-parsing during reconstruction (X2). Partitioned history tables (D9's dump/restore and RLS findings must be re-run first). Retention that deletes `revision` rows.

**Reversal cost.** Dropping the ledger for a different physical model means a migration that reconstructs every note's history through the public read API and re-writes it — mechanical, and possible precisely because reconstruction never re-parses.

---

## 5. Benchmark gate

*No gate, no acceptance.* This repo has twice shipped a gate that could not fail — an exit code captured and never read, and a hardcoded `"status": "ok"` — so §5.0 is normative for every suite below.

**5.0 Harness contract.** (a) `pending` is not a CI status: under `PGMIND_GATE_STRICT=1` (set in CI) a missing tool is a *failure*, never a skip. (b) **Every suite ships a negative control.** `make eval-selftest` runs each suite against an injected defect — `pgmind.break_history`, `pgmind.disable_serialization`, `pgmind.break_excision`, all admin-only and refused unless `pgmind.allow_fault_injection = on` — and asserts the suite reports `fail`. A suite whose selftest does not fail is not a gate, and this RFC is not accepted without them. (c) No absolute cross-machine timing threshold: thresholds are ratios of two measurements from the same invocation, and none tighter than 1.25× (the published run-to-run spread is ±2% and the growth curve swings ±13%). (d) Every published artifact carries `pg_version`, `build_profile` (`release`, or the suite fails), host, commit and the raw per-round samples.

| suite | metric | threshold |
|---|---|---|
| **`history-fidelity`** | 40 fixture notes + 2 000 fuzz documents, each driven through a seeded 250-operation stream over every mutating verb; after each op the harness records `read()` bytes and a digest of the `blocks()` rowset. Mismatches between the recording and `read_as_of` / `blocks_as_of` at every revision; `verify_note` + `verify_history` violations. | **0 / 0 / 0.** Determinism, not quality. |
| **`concurrency-isolation`** | `pg_isolation_regress` specs: CAS mismatch, CAS-vs-byte-identical-write, concurrent append, disjoint patch, write-vs-op, op-vs-op, move-vs-write, delete-vs-append, excise-vs-write, and the same permutation at READ COMMITTED (⇒ PM0xx) and REPEATABLE READ (⇒ 40001). Golden output may contain no uuid, timestamp or byte count — if an interleaving cannot be observed without printing an id, the API is wrong. | exit 0, zero diffs |
| **`concurrency-load`** | 8 writers × 60 s over 200 notes, mixed verbs, `psql` co-processes rendezvousing on `pg_blocking_pids()` (never `pg_sleep`, no new Python dependency). Failable: every returned revision present in its chain; `seq` unique and gapless; one root, no forks; every acknowledged append present exactly once; `verify_*` clean. | **0 violations.** Conflict rate, retry histogram and throughput published, not thresholded. |
| **`storage-growth`** | 1 000 notes driven to depth 1/10/50/100 under three shapes (one-block patch, whole-document write, structural insert) plus a 100-block variant. Publishes bytes per table for **every** table enumerated at run time, the **effect-rows-per-revision histogram by verb**, both ratios (vs current state *and* vs full-copy-per-revision) with the denominators in the key names, `as_of`/`diff`/`blame` p95 per depth, and bytes actually returned to the OS by retention. | Report-schema clause (both ratios, histogram, release build, one row per pgmind table) plus two in-run ratios: write cost at depth 100 ÷ depth 0 ≤ **1.25**; `as_of` p95 ÷ same-suite `read` p95 ≤ **25**. Everything else honest numbers. Publishes `capacity-model-v2.json`; **v1 is never edited.** |
| **`excision-completeness`** | 13 scenarios × unique canary and path segment: live block, block removed 50 revisions ago, block whose only carrier is a frame, block spanning frames, split, merged-away, whole note, moved note, tombstoned note, excise-then-compact, compact-then-excise, linked-from-elsewhere, repeat excision. Sweep counts occurrences of the canary, its **BLAKE3** (RFC-002 D7's hash — not SHA-256) and the affected uuids across every text/jsonb/bytea/array column of every pgmind table enumerated at run time, **and in raw `pg_dump` output**. Then dump → plain-`psql` restore → `enforce_excisions()` → sweep again. | **0 hits**, `verify_excision` empty, log row with correct counts, every refusal clause asserted individually. Anti-vacuity: the suite asserts `columns_scanned > 0` and that the scanned set covers the pgmind tables — an under-enumerating sweep is the exact way this gate would report green on a broken system. |

RFC-004 Part B's adversarial-edit-corpus match rate is Phase 3's other gate and belongs to that RFC.

---

## 6. Law compliance

- **Law 8 (append-only with audited excision)** — implemented here, both halves: history is insert-only, erasure is explicit, refused rather than partial, logged, verified, and replayed after restore.
- **Law 3 (markdown is a boundary)** — history stores tile bytes and typed per-block columns, never an AST datum.
- **Law 4 (parsing never yields identity)** — X2 forbids re-parsing during reconstruction, and D7's escalation rule exists so redaction can never re-align the id vector.
- **Law 7 (incremental maintenance only)** — the pre-image write is proportional to the edit, not the note; no operation rebuilds a note's history.
- **Law 9 (feel like PostgreSQL, no hidden behaviour)** — no background jobs, no triggers, no stored retention policy that can fire unattended; `dry_run` defaults to true on both destructive functions.
- **Law 11 (layers by contract)** — `pgmind.excise`, `pgmind.retain`, `pgmind.verify_history` and `pgmind.enforce_excisions` are admin surfaces, marked as such and revoked from `PUBLIC`.
- **Laws 1 and 2 (AI-free, no network I/O)** — untouched; nothing here calls anything.
