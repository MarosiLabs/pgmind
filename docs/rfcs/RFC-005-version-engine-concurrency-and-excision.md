# RFC-005: Version Engine, Concurrency Semantics & Excision

- **Status:** **Accepted 2026-08-06 — living while Phase 3 is active** (amendments land in place per §12 lifecycle; frozen at Phase 3 exit). **Amended 2026-08-06 after the pre-implementation adversarial review** — five critical defects (tenant RLS on the new tables, the audit row that kept what it erased, a privilege-filtered erasure sweep, arity-destroying redaction escalation, a dump-restore gate blind to every history table) and eleven majors, each marked in place below.
- **Phase:** 3
- **Owner:** project author
- **Created:** 2026-08-05 · **Accepted:** 2026-08-06 · **Frozen:** —

## 1. Context

Phase 2 shipped a vault that remembers only *now*. [RFC-003](RFC-003-vault-and-block-storage-layout.md) says so in its title — Law 8 deferred, current-state storage until this RFC — and its write path is honest about the consequence: a block that leaves a note is `DELETE`d, a merge retiree's rows are `DELETE`d, and `pgmind.revision` carries an author, a source and a timestamp but **no content at all**. Every `revision` row today is a receipt for a change nobody can reconstruct.

This RFC implements handbook **Law 8 — "Append-only with audited excision. Revisions are inserts; excision is explicit, logged, and policy-driven."** That law is already the qualified form of a claim the audit refused to let stand: [AUDIT C4](../archive/AUDIT.md) found that *every* shipped immutable store was forced to add erasure — Datomic excision, Dolt 2.0 GC, XTDB 2.x, TerminusDB squash — for storage economics and for the right to erasure. A version engine that cannot forget is not shippable, and one that forgets silently is worse. Both halves are normative here.

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
  prev_path     text,                     -- NULL ⇒ unchanged; move_note's pre-image
  prev_preamble text,                     -- NULL ⇒ unchanged by this revision
  prev_props    jsonb,
  -- Pre-image scripts. ops is a packed int4[] of (KEEP a b | INS k) instructions;
  -- payloads hold only what INS introduces, literally (X1). Length is read with
  -- cardinality(), never array_length(x,1), which returns NULL on the empty array.
  tile_ops      int4[] NOT NULL,  tile_payload  text[] NOT NULL,
  id_ops        int4[] NOT NULL,  id_payload    uuid[] NOT NULL,
  place_prev    int4[] NOT NULL,          -- (tile_ord, start, end) triples, changed slots only
  place_idx     int4[] NOT NULL,
  head_prev     int4[] NOT NULL,          -- changed heading_path slots: (idx, payload offset)
  head_payload  text[] NOT NULL,
  UNIQUE (note_id, seq)
);

-- H2: per-block pre-image. One row ONLY for blocks whose content-visible columns changed.
CREATE TABLE pgmind.block_revision (
  note_id           uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  block_id          uuid NOT NULL,        -- deliberately no FK: the block may be gone
  seq               bigint NOT NULL,
  vault_id          uuid NOT NULL,
  existed           boolean NOT NULL,     -- false ⇒ minted by this revision (no pre-image)
  redacted          boolean NOT NULL DEFAULT false,  -- true ⇒ NULLs below are erasure, not absence
  prev_kind         pgmind.block_kind,
  prev_content      text,
  prev_content_hash bytea CHECK (prev_content_hash IS NULL OR octet_length(prev_content_hash) = 32),
  prev_block_ref_id text,
  prev_attrs        jsonb,
  prev_parent_block uuid,
  confidence        real,                 -- RFC-004 Part B: NULL ⇒ deterministic binding
  bind              text CHECK (bind IN ('mint','ref','hash','carry','rebind','remove')),
  PRIMARY KEY (note_id, block_id, seq)
);

-- H3: periodic absolute snapshot of BOTH lanes, so deep as_of is bounded (D3).
CREATE TABLE pgmind.note_frame (
  note_id     uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  seq         bigint NOT NULL,
  vault_id    uuid NOT NULL,
  preamble    text NOT NULL,
  props       jsonb NOT NULL,
  path        text NOT NULL,
  tiles       text[] NOT NULL,
  -- Semantic lane, per block in ord order. Without these a frame anchors
  -- read_as_of but NOT blocks_as_of, and D3's cadence bound would be false for
  -- every structured read. *Added 2026-08-06 (post-acceptance review).*
  block_ids   uuid[] NOT NULL,
  kinds       text[] NOT NULL,
  contents    text[] NOT NULL,
  head_paths  jsonb NOT NULL,             -- array of text[]; jsonb because SQL has no text[][]
  parents     uuid[] NOT NULL,
  block_refs  text[] NOT NULL,
  attrs       jsonb NOT NULL,
  placement   int4[] NOT NULL,
  PRIMARY KEY (note_id, seq)
);

-- H4a: the audit trail erasure owes (Law 8). Dumped, swept, and content-free by
-- construction. Never mutated by any pgmind code path.
CREATE TABLE pgmind.excision_log (
  id            uuid PRIMARY KEY,
  vault_id      uuid NOT NULL,
  requested_at  timestamptz NOT NULL DEFAULT now(),
  requested_by  text NOT NULL DEFAULT current_user,
  reason        text NOT NULL,
  target_kind   text NOT NULL CHECK (target_kind IN ('note','block','revision','before','literal')),
  scope         jsonb NOT NULL,           -- counts per lane, notes/revisions touched
  escalations   jsonb NOT NULL DEFAULT '[]'::jsonb,   -- tile-level fallbacks (D7)
  verified_at   timestamptz,
  survivors     int4                      -- NULL until verified; 0 = proven
);

-- H4b: the replay input. Holds the executable target — which for the 'literal'
-- and 'note' forms IS identifying data — and is therefore deliberately NOT
-- registered with pg_extension_config_dump: it does not travel in backups.
-- *Split from H4 2026-08-06 (post-acceptance review): storing the target "as
-- given" made the audit table the one surviving copy of what was erased, in the
-- one table guaranteed to be reproduced in every future dump.*
CREATE TABLE pgmind.excision_replay (
  excision_id uuid PRIMARY KEY REFERENCES pgmind.excision_log(id) ON DELETE CASCADE,
  target      jsonb NOT NULL
);

CREATE INDEX block_revision_block ON pgmind.block_revision (block_id, seq DESC);
CREATE INDEX note_revision_note   ON pgmind.note_revision (note_id, seq DESC);
CREATE INDEX note_frame_note      ON pgmind.note_frame (note_id, seq DESC);
CREATE INDEX revision_parent      ON pgmind.revision (parent) WHERE parent IS NOT NULL;
```

**Tenancy.** *Amended 2026-08-06 (post-acceptance review).* All five new tables carry `vault_id`, and RFC-003 D1's shipped boundary — `pgmind.enable_vault_rls` — enumerated a **literal list of six table names** ([schema.rs:186](../../extension/src/schema.rs#L186)). Left alone, a deployment that adopted D1's boundary would have RLS on `block`/`tile`/`note` and none on the tables holding *literal copies of the same bytes, including bytes already deleted from the live lanes* — plus the erasure audit trail. Reproduced on PG 18.4: a tenant with `SET pgmind.vault_id` to its own vault read another vault's `block_revision.prev_content`, `note_revision.tile_payload`, `note_frame.tiles` and `excision_log` rows, while the shipped `tenant-isolation` gate stayed green because it too enumerates the same six names. **`enable_vault_rls` MUST enumerate every table in schema `pgmind` carrying a `vault_id` column from `pg_catalog` at call time**, must be re-runnable after an extension upgrade, and §5's tenant gate asserts policy count equals the catalog count (§D11 declares the RFC-003 D1 amendment).

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

**Reconstruction.** Start at the nearest anchor at or above `T` — the current state, or a `note_frame` with `seq ≥ T` — and apply the pre-image scripts of every revision above `T` in descending `seq`. Cost is bounded by the frame cadence `pgmind.frame_every` (USERSET, default **50** revisions), not by the note's total depth. `blocks_as_of` zips the reconstructed id vector against the reconstructed placement, heading-path and block pre-image state; it applies the same `tile_ord` range guard `knowledge.blocks()` applies today (RFC-003 D7, amended after review) — a historical row is not more trustworthy than a live one.

*Amended 2026-08-06 (post-acceptance review), three corrections without which the paragraph above is false:*

- **Frames must exist to be an anchor.** The write path MUST write a `note_frame` whenever `seq mod pgmind.frame_every = 0`. As accepted, the only frame-writing rule was compaction's (D8), which places a frame *at the floor* — usable as an anchor for `T = floor` alone, since reconstruction consumes anchors at or **above** `T`. Without a cadence writer the bound degrades to O(depth) and the RFC's headline read cost is unearned.
- **A frame anchors both lanes or neither.** H3 originally stored tiles, ids and placement but no `kind`, `content` or `heading_path`, so `blocks_as_of` had no anchor at all: it would have had to walk to head regardless of frames, or re-parse the frame's tiles — which X2 forbids. H3 now snapshots the semantic lane too.
- **A reconstruction observes one snapshot.** It reads an anchor and then a set of scripts, which is at least two statements; pgrx runs SPI read-only only when the transaction has no XID, so *any* prior write in the caller's transaction — which every mutating op guarantees — puts each subsequent statement on a fresh snapshot. Reproduced on PG 18.4: a concurrent commit between the two statements yields a document one revision off, silently. Reconstruction MUST therefore execute against a single pinned snapshot (one statement, or an explicitly held `ActiveSnapshot`) and raise rather than return if it cannot.

`blame` reads the newest `block_revision` row per `block_id` at or below head. Because position lives in the per-revision vectors and **not** in the effect row (D4), "newest effect row" means "last content change" directly, with no window function and no filtering of moves.

A read below `note.history_floor` raises **PM011**, always. It never returns a partially-reconstructed document: the failure mode this RFC most wants to prevent is an agent citing a revision and receiving a plausible, wrong one.

### D4. What each write records

The write path (RFC-003 D6 steps 5–8) and the five block ops gain one step: **before mutating a lane, record its pre-image.** Rules, normative:

- **One `note_revision` row per revision, always** — it carries the id and placement vectors, which is what makes `blocks_as_of` work without the parser (X2).
- **A `block_revision` row only when a content-visible column changed**: `kind`, `content`, `content_hash`, `block_ref_id`, `attrs`, `parent_block`. **`ord`, `tile_ord`, `start_in_tile`, `end_in_tile` and `heading_path` never produce an effect row** — all five are carried by the per-revision vectors in H1. This is the single most consequential rule in the RFC's economics: inserting a block at the top of a 100-block note would otherwise write 100 full history rows for an edit whose semantic content is one block. Structural churn is exactly the workload Phase 4's importer produces, so a design that is O(note) per structural edit fails on the traffic it was built for. *Amended 2026-08-06 (post-acceptance review): `heading_path` was in the content-visible set, and the same sentence offered "renaming a heading" as the case the rule saves — which it did not, since a rename changes no `ord` at all but rewrites `heading_path` for every block in the section. Renaming a `##` heading above 60 blocks wrote 60 full effect rows. `heading_path` is a positional fact (RFC-002 D6 derives it from preceding headings), so it belongs with the other positional vectors; `head_prev`/`head_payload` in H1 carry the changed slots.*
- **Removal stops being `DELETE`** (D6).
- **NULL in a `prev_*` column means "this column did not change."** Erasure is not absence: D7 sets `redacted = true` on every row it touches, so `blame`, `diff` and `blocks_as_of` can distinguish "unchanged" from "erased" and report the second as erased rather than silently inheriting the newer value. *Added 2026-08-06 (post-acceptance review) — the two readings were indistinguishable, which would have made redacted history reconstruct as though the erased text had never been edited.*
- **`revision` gains `verb`** (`write|insert_blocks|update_block|move_block|split_block|merge_blocks|delete_note|undelete_note|move_note|excise|retain`) so `history()` is readable without reconstructing anything.
- **RFC-004 A4's `meta.minted` / `meta.removed` arrays are retired** into the history lanes, which now hold the same facts in typed columns (§D11). Today they make the average `revision` row **1,170 B of heap** — measured, [capacity-model-v1](../../eval/published/capacity-model-v1.json) — which at the design target is 8.85 GB of jsonb duplicating what H1 and H2 store properly.

The **Determinism Rule**: reconstruction reads stored bytes and stored vectors. It never calls comrak, never recomputes a hash to decide structure, and never depends on the block taxonomy of the *current* build. `revision.meta.parser_epoch` stamps the parse/hash generation that produced a revision — provenance for a future rehash migration (RFC-012), not a precondition, because nothing in the read path re-parses.

### D5. Concurrency: compare-and-swap, and what a writer observes

```sql
knowledge.write(path text, doc markdown, expected_head uuid DEFAULT NULL) → uuid
-- and expected_head on every mutating op, same default, same semantics
```

Normative:

1. **Serialization is the note row.** Every mutating operation takes `SELECT … FROM pgmind.note WHERE id = $1 FOR NO KEY UPDATE` on its target note. It is the right tool where Phase 2 used an advisory lock: it is released by the transaction, it blocks writers without blocking readers, and it makes `pg_blocking_pids()` show the wait — which is what lets the concurrency gate test interleavings without sleeping. Verified on PG 18.4: it blocks a second `FOR NO KEY UPDATE`, `FOR SHARE`, and any `UPDATE` of the row (including `path`, a key column of a *partial* unique index), while leaving plain readers and `FOR KEY SHARE` alone.
2. **The note row lock protects the note row. Nothing else.** *Amended 2026-08-06 (post-acceptance review) — measured: holding it does not block another session inserting that note's tiles, updating its blocks, or writing its history rows.* Two consequences are normative. **(a) Every read whose value the operation depends on — the pre-image of both lanes, `head_revision`, `path`, `history_floor` — MUST be taken *after* the lock, in the same statement or later.** As accepted, D5 pinned only `path` to the post-lock snapshot, so a writer could compute a pre-image from a state its own lock never covered. **(b) `pgmind.excise` and `pgmind.retain` have no single target note** — `{"literal":…}` and `retain(vault)` are vault-wide — so they take `FOR NO KEY UPDATE` on every note they touch, in ascending `note.id` order, and are documented as long-lived exclusive maintenance operations rather than pretending to be online.
3. **Path occupancy is a separate lock domain from the note row.** *Amended 2026-08-06.* The advisory lock on `(vault, path)` is taken by **every operation that changes which note occupies a path** — create, `move_note`, `undelete_note` — for both the source and destination paths, in lexicographic order. As accepted, the advisory lock covered creation only, and the row lock covers a *row*, not a *name*: reproduced on PG 18.4, session A creating `new/target` while session B renamed a note onto `new/target` gave B a bare `23505` on `note_live_path` — an error D10 had no code for and D5.5 promises callers will never see. A path already occupied by a live note raises **PM015**.
4. **The path is re-checked under the lock.** After acquiring the note row lock the write path MUST re-read `path` and raise **PM002** if it changed.
5. **CAS precedes idempotence.** When `expected_head` is non-NULL and differs from the observed `head_revision`, raise **PM009** — *even when the incoming bytes are identical to the stored bytes*. RFC-003 D6 step 2's byte-identical short-circuit answers "did anything change"; CAS answers "did you see what you were changing." Reordering them would make a stale writer's no-op silently succeed.
6. **CAS on a path with no live note is still CAS.** *Amended 2026-08-06 (post-acceptance review).* A non-NULL `expected_head` against a path that does not exist — or exists only as a tombstone, since the by-path lookup filters tombstones out — raises **PM009**, never a silent create. The caller asserted it was editing something; the honest answer is that what it was editing is gone. Without this rule a writer racing a concurrent `delete_note` resurrects the note as a fresh creation and its CAS never fires.
7. **`expected_head` NULL means last-writer-wins**, explicitly and by the caller's choice. Phase 2 semantics are preserved for callers that have no head to assert.
8. **The extension never raises `40001`/`40P01` itself, and never retries internally.** Conflicts pgmind detects are `PM0xx` with the observed head in the error detail, so a client can re-read and retry deterministically. Serialization failures Postgres raises under REPEATABLE READ or SERIALIZABLE pass through untouched — an extension that swallowed them would break the caller's own retry loop.
9. **Multi-note operations lock in ascending `note.id` order**, so two writers touching the same pair cannot deadlock against each other. **This includes the D5 creation-repair pass**, which is a multi-note operation and was not treated as one: RFC-003 D5 has the write path `UPDATE pgmind.edge … WHERE vault_id = $1 AND dst_path = $2` across *other notes'* rows ([store.rs:431](../../extension/src/store.rs#L431)) while holding only its own note's lock. Repair now takes the row locks of the notes whose edges it rewrites, in ascending `note.id` order, after its own — so two concurrent creations that both repair the same linking note serialize instead of racing. *Amended 2026-08-06 (post-acceptance review).*
10. **`knowledge.append_to_section(path, heading_path text[], fragment markdown, expected_head uuid DEFAULT NULL)`** appends after the last block of the named section. Two concurrent appends serialize on the note row and **both survive, in lock-acquisition order**; append is the one operation for which a conflict is not a conflict, and saying so is the point of having it as an operation rather than a read-modify-write.
11. **Compare-and-swap has a second granularity: the block.** *Added 2026-08-06 — this RFC as accepted specified only note-level CAS, which the product plan's Phase 3 deliverables did not, and the omission has a user-visible cost.*

    ```sql
    knowledge.update_block(block_id uuid, fragment markdown,
                           expected_head uuid  DEFAULT NULL,
                           expected_hash bytea DEFAULT NULL) → pgmind.op_result
    ```

    `expected_hash` is the `content_hash` the caller last read for that block (exposed by `knowledge.blocks`). Non-NULL and different from the block's current hash ⇒ **PM016**. Normative details:

    - **It is read under the note row lock**, like every other value an operation depends on (rule 2a). A hash read before the lock is a hash that may already be stale.
    - **Both guards may be given and both are checked, `expected_head` first.** The caller chooses its granularity by which it passes; passing both means it wants both assertions, and the coarser one failing first is the more informative error.
    - **Block CAS does not remove serialization — it removes the *failure*.** Two agents patching different paragraphs of one note still take the note row lock one after the other. What changes is that neither is rejected. With only `expected_head`, the second writer is told its head moved even though nothing it touched changed, and its only escape is passing no guard at all: a choice between false conflicts and no safety. That is the concrete cost of the omission.
    - **`update_block` only, deliberately.** Split, merge, move and insert address blocks too, and could take the same guard, but nothing has asked for it; widening later is additive and narrowing is not.

    The product plan calls this operation `patch_block` and lists it beside `update_block` as though they were different functions. They are not: they would differ only in which guard they accept. This RFC folds the plan's `patch_block` into `update_block`'s signature rather than shipping two ways to do one thing — a declared deviation from plan §7's naming, recorded in D11, to be ratified when RFC-007 freezes the API and MCP surface.

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

**Where the target is stored is itself an erasure decision.** *Amended 2026-08-06 (post-acceptance review).* As accepted, `excision_log.target` held the caller's target "as given" and was annotated "carries no erased content" — false for two of the six forms, since `{"literal":"Jane Doe, 1978-04-02"}` *is* the content and §3 already says a path like `people/jane-doe` is identifying data. The audit table was `pg_extension_config_dump`-registered, so it would have become the one copy of the erased string guaranteed to be reproduced in every future backup. Reproduced on PG 18.4: after a maximally thorough excision, §5's own 0-hit sweep found the canary in `excision_log.target` and in the raw `pg_dump` output — the gate and the replay requirement were mutually unsatisfiable. The log now records only `target_kind` and counts (H4a, dumped and swept); the executable target lives in `pgmind.excision_replay` (H4b), which is **not** dump-registered and is the sweep's one named exception. An operator who restores an old dump therefore gets `enforce_excisions()` with the audit trail intact and the replay inputs absent for excisions predating that dump — which is stated in D7.7 rather than papered over.

**Mechanics.**

1. **Live content is refused, not silently spared.** If the target still exists at head, `excise` raises **PM012** unless `and_head => true`, in which case it first performs an ordinary audited write that removes the content (a real revision, with `verb='excise'`), and only then erases history. Erasure that quietly left the current copy in place would be the worst possible outcome of a right-to-erasure request.
2. **Byte-lane redaction is a splice, not a `NULL`.** A tile pre-image holds a whole top-level child — an entire list, table or code fence — so nulling it would erase every *other* block that shared the tile and would leave `read_as_of` reassembling a document with a hole. Instead the erased span is replaced, through the same splice machinery as RFC-003 D6, with a marker (`⟨redacted pgmind:‹excision-id›⟩`), and the post-splice PM008 assertion runs on the result. Redacted history therefore still reconstructs to byte-defined, parseable markdown.
3. **Escalation is arity-preserving, and redaction shifts the spans it moves.** *Amended 2026-08-06 (post-acceptance review) — as accepted, this clause destroyed the invariant it existed to protect.* Where a marker cannot be spliced in place — inside a table row, a tight nested list, a fenced block — the redaction escalates to **whole-tile redaction**, which as written collapsed the tile to "a single marker paragraph." Measured on a real vault: an ordinary 8-item bullet list is *one tile holding 16 addressable blocks* (8 items + 8 inner paragraphs), so escalation left the id vector 15 longer than the parsed block count, D7.6(c) failed by construction, PM013 aborted the transaction, and the erasure request could not be fulfilled through the documented API — for exactly the shape escalation was introduced to handle. Truncating the vector instead would silently retire 15 unrelated blocks' identity across history, the Law 4 violation this clause exists to prevent. Normative now: **whole-tile redaction emits one marker block per id the vector assigns to that tile**, preserving arity exactly; and **every redaction that changes a tile's byte length MUST shift the co-located spans in `note_revision.place_prev` and `note_frame.placement` in the same transaction** — measured, replacing a 17-byte string with a 58-byte marker shifted seven following blocks' spans by +41, and D7.6's three clauses could not see it (the canary was gone, the bytes still parsed, the count still matched). That is D7.6's new clause (d).
4. **Semantic-lane erasure nulls `prev_content`, `prev_content_hash`, `prev_attrs`, `prev_block_ref_id`, `prev_heading_path`** on matching effect rows, keeping the row skeleton so history still knows *that* a block existed and when. Hashes go too: BLAKE3 of a short block is a confirm-a-guess oracle, and RFC-002 D7's hash is BLAKE3 — any tooling that sweeps for SHA-256 is checking the wrong thing.
5. **Derived surfaces are erased in the same transaction**: `edge` rows whose `dst_path`/`alias` carry the text, `tag` rows, `revision.message`, `revision.meta`, and `note.path`/`preamble`/`properties` where the target is a note. The sweep enumerates columns **from `pg_catalog` (`pg_class`/`pg_attribute`) at call time**, so a table added by a later RFC is swept by construction rather than by remembering. *Amended 2026-08-06 (post-acceptance review): as accepted this said `information_schema.columns`, whose views are **privilege-filtered by the current user**. Measured on PG 18.4 — the same sweep saw 46 columns across 10 tables as the extension owner and 24 across 4 as a Law-11 erasure admin granted rights on exactly the history and audit tables, so the live `pgmind.block` lane was never opened and the canary still in it went unseen. The function returned a positive attestation of erasure, and §5's anti-vacuity clause, computed from the same filtered view, passed too.* `pgmind.excise` and `pgmind.verify_excision` are `SECURITY DEFINER` owned by the extension owner, with `search_path` pinned.
6. **Verification is a whole-note re-reconstruction, and the RFC does not pretend otherwise.** `verify_excision` walks *every* surviving revision of every affected note, reconstructs it, and asserts (a) the canary text appears in no column of any pgmind table — enumerated from `pg_catalog`, with the sole named exception of `pgmind.excision_replay`, which holds the target by design and does not travel in dumps (D2 H4b); (b) every revision reconstructs to parseable bytes; (c) the id vector length equals the parsed block count at every revision; (d) every span in the reconstructed placement vector lies inside its tile and the tile's byte length matches the sum of its blocks' spans, so a redaction that moved bytes without moving spans is caught. That is O(revisions per affected note), not O(1); excision is a rare, admin-initiated, audited operation and buying certainty with its runtime is the right trade. `survivors > 0` raises **PM013** and the transaction aborts — an incomplete excision is never committed, and no `excision_log` row survives an aborted excision.
7. **Restore is a hostile environment.** A `pg_dump` taken before an excision restores the erased content. `enforce_excisions()` replays every `excision_log` row against the restored database and returns the number of surviving hits it erased; the `dump-restore` gate runs it. The RFC states plainly what pgmind cannot do: it cannot reach backups it does not know about, and no in-database mechanism can.

### D8. Retention and compaction

```sql
pgmind.retain(keep_revisions int DEFAULT NULL, keep_since interval DEFAULT NULL,
              keep_sources text[] DEFAULT NULL, vault uuid DEFAULT NULL,
              dry_run boolean DEFAULT true) → TABLE (note_id uuid, seq_floor bigint, rows_removed bigint)
```

A **function with parameters, not a stored policy catalog**: there is no persisted mode that can be toggled into deleting more than the caller asked for, and no background job that runs when nobody is watching. `dry_run` defaults to true.

- Compaction writes a `note_frame` at the new floor, then deletes `block_revision`, `note_revision` **and every `note_frame` below** it, then advances `note.history_floor`. *Amended 2026-08-06 (post-acceptance review): frames below the floor were not in the deletion set, so a compacted — or excised-then-compacted — vault kept periodic full snapshots of exactly the content PM011 claims is no longer reconstructable.*
- **`pgmind.revision` rows are never deleted by retention.** `history()` keeps listing every revision — id, author, source, verb, timestamp — long after its content has been compacted away. A vault that forgot must remember that it forgot; and a client holding a compacted revision id gets **PM011** ("no longer reconstructable"), never PM010 ("no such revision"), because those two mean opposite things to whoever has to debug it.
- `keep_sources` lets low-value history age faster than authored history — `source='rebind'` is the intended case (RFC-004 Part B).
- Post-condition, asserted by `pgmind.verify_history(note_id)`. *Amended 2026-08-06 (post-acceptance review) — as accepted, all three clauses were unsatisfiable, vacuous, or both:*
  1. **The floor frame is the state at the floor, for both lanes.** The old "exactly one `block_revision` row at or below the floor per block" could only hold for a block that happened to change at exactly `seq = floor`; every other block's state lived in rows the same sentence deleted. H3's semantic-lane columns (D2) are what make the floor frame a complete anchor, so the clause becomes: a `note_frame` exists at `history_floor`, its vectors are internally consistent, and it covers every block live at that point.
  2. **`seq` is dense and gapless above the floor, and every `pgmind.revision` row above the floor has exactly one `note_revision` row.** The second half is what catches a history lane that silently stopped being written — including the dump-restore failure below.
  3. **Reconstruction is checked where it can fail.** `read_as_of(head) = read()` is a tautology — reconstruction at head applies no scripts and reads the current state by definition — so it proved nothing. `verify_history` instead reconstructs at the floor, at the newest frame, and at `min(20, depth)` revisions sampled deterministically between them, asserting each reconstructs to parseable bytes with an id vector matching its parsed block count.
- `blame`'s `first_revision` is the revision of the block's `existed = false` row. Below the floor that row is gone, so `first_revision` is **NULL with `history_floor` reported alongside it** — the oldest and most-cited blocks are exactly the ones compaction blinds, and the API says so rather than returning the floor as though it were an origin.

### D9. Capacity: what this costs, and what is not yet known

From the published Phase 2 measurements ([capacity-model-v1](../../eval/published/capacity-model-v1.json)): block 429.9 B/row all-in, tile 180.0, revision 1,258.3, and 657.9 B/block all-in across the whole schema at 10k notes / 230k blocks.

The honest form of this section is a ratio with a named free variable. History size is **linear in effect rows per revision**, and that quantity is *unmeasured*: the modal edit's shape is a property of agent traffic, not of this design. At one changed block and one changed tile per revision it is ≈870 B/revision, giving ~1.9× current state at the plan's design target (100k notes / 10M blocks / 100 revisions). At the other extreme — whole-document rewrites, which is exactly what RFC-006's importer and a naive read-edit-write agent loop produce — it degenerates toward full copies. **The `storage-growth` gate measures the histogram first, and the multiplier is published against it** (§5). Publishing a single number without its denominators is how this section would lie.

**Measured 2026-08-06** by the `storage-growth` gate (`eval/published/capacity-model-v2.json`), 120 notes of 23 blocks at 25 revisions each, three traffic shapes, `frame_every = 50`:

| verb | effect rows per revision |
|---|---|
| `insert_blocks` (structural insert at ord 0) | **1.00** |
| `update_block` (one block edited) | **1.00** |
| `write` (whole-document rewrite) | **3.36** |

So the free variable is now a measured one, and D4's central rule survives contact with it: a structural insert into a 23-block note costs **one** history row, not 23. History came to **0.207× current-state bytes** and **0.008× what storing every revision in full would cost** — both denominators named, because a single flattering ratio quoted alone is how this section would mislead. Deep `as_of` p95 was 0.56 ms against 0.33 ms for a head read (1.7×, against the gate's 25× ceiling).

Two caveats on those numbers, stated rather than buried: the corpus mixes the three shapes evenly, which real traffic will not, and `pgmind.revision` is counted as current state rather than history — it is metadata either way, but the key names say which side it sits on. The ~1.9× figure this section carried before was a model; it is superseded by the measurement, and the model was pessimistic.

Two figures need their own caveat, added after review. The 8.85 GB recovered by retiring `meta.minted` is derived from the *measured* Phase 2 revision row (1,170.6 B of heap, [capacity-model-v1](../../eval/published/capacity-model-v1.json)), but that row's size is a **create-shaped** artefact: every one of the 10k measured revisions minted all 23 of its note's blocks, so `meta.minted` held 23 quoted uuids. A steady-state vault whose revisions change one block carries a far smaller `meta`, and the saving shrinks with it. And the frame cadence adds storage the ratio above does not include: a frame is a full copy of both lanes every `frame_every` revisions, so history is ≈ (effect rows) + (current-state size ÷ `frame_every`) per note — the knob trades storage against deep-read latency, and the gate measures both ends of it rather than assuming a good default.

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
| PM015 | `pgmind_path_taken` | A create, `move_note` or `undelete_note` targeted a path a live note already occupies. Added after review: without it the caller gets a bare `23505` from `note_live_path`, which D5.8 promises will never happen. |
| PM016 | `pgmind_stale_block` | `expected_hash` is not the block's current `content_hash` (D5.11). Distinct from PM009 on purpose: PM009 means *someone* changed the note, PM016 means someone changed **this block**, and only the second one obliges the caller to re-read its own edit. Detail carries both hashes. |

### D11. Declared amendments to frozen and accepted RFCs

Precedence rules require these to be explicit, not incidental:

- **RFC-003 D1 (frozen) — `pgmind.enable_vault_rls` is amended to enumerate every table in schema `pgmind` carrying a `vault_id` column from `pg_catalog` at call time, instead of the literal six-name list it shipped with, and to be re-runnable after an extension upgrade.** RFC-012's upgrade script MUST re-invoke it where a policy already exists. Without this the four new history tables sit outside the tenant boundary while holding literal copies of note bytes (D2). *Added 2026-08-06: this was the review's first critical finding, and the omission was undeclared.*
- **RFC-003 §5.6 (frozen) — the `dump-restore` gate's table set becomes a run-time enumeration of schema `pgmind`, with non-zero counts asserted for the history tables.** Measured on PG 18.4: with `pg_extension_config_dump` missing for `block_revision` alone, *every* assertion the shipped suite makes stays green — equal counts across its six hardcoded tables, `verify_note` clean, post-restore write working — while 100% of per-block history vanishes from the backup. `verify_history`'s "one `note_revision` row per revision above the floor" (D8) is the in-database half of the same check.
- **RFC-003 D3 (frozen) — `pgmind.note` gains `history_floor bigint NOT NULL`; `pgmind.revision` gains `seq bigint NOT NULL` and `verb text NOT NULL`.** *Amended 2026-08-06:* the upgrade is `ADD COLUMN` nullable → backfill → `SET NOT NULL`, because `ADD COLUMN … NOT NULL` without a default aborts on any populated vault (reproduced: `ERROR: column "seq" of relation "revision" contains null values`, which would make every existing installation unupgradeable). `seq` is backfilled by walking the `revision.parent` chain, **not** by `created_at`, which ties for revisions written in one transaction. `history_floor` is backfilled to **each note's head `seq`, not 0**: Phase 2 revisions carry no recoverable content, so a floor of 0 would assert they are reconstructable and `read_as_of` would return current state as though it were a year-old revision — a plausible, wrong answer with no PM011, which is the single failure mode D3 exists to prevent. `revision.source`'s CHECK is unchanged. *Corrected 2026-08-06, after Part B shipped: this sentence claimed `'sync'` and `'rebind'` would "become reachable … through RFC-006 and RFC-004 Part B". Half of that is now falsified and the other half was a category error.* `source` answers **where a write came from** — an API call or the sync bridge. A rebind is not an origin, it is a mechanism, and one `write()` routinely rebinds some blocks and mints others, so there is no honest per-revision answer. The rebind signal therefore lives at block granularity, where it can be true: `block_revision.bind = 'rebind'` with the score in `confidence`. `'rebind'` in the revision CHECK is consequently **vestigial — nothing produces it and nothing should**; removing it is a schema change and waits for RFC-012's upgrade mechanism rather than churning the schema for tidiness. `'sync'` remains genuinely reserved for RFC-006, which is the only thing that can set it.
- **RFC-003 D4 (frozen) — `revision.parent` gains the index D4 deferred to this RFC by name.**
- **RFC-003 D6 (frozen) — removal is no longer a bare `DELETE`**; the pre-image write precedes it, and the normative INSERT/UPDATE-before-DELETE ordering extends to it.
- **RFC-004 A4 (accepted) — `revision.meta.minted` / `.removed` are retired** in favour of `block_revision` rows carrying the same facts in typed columns; `meta` keeps the split/merge provenance objects. The pg_test `merge_without_keep_records_provenance` pins the old shape and changes with this RFC.
- **Product plan §7 (lower precedence, recorded anyway) — `patch_block` is not a separate function.** The plan lists `knowledge.patch_block(block_id, markdown, expected_hash)` in the API surface and in the MCP tool list, beside `update_block` among the "lower-level block ops". Two functions differing only in which CAS guard they accept is two ways to do one thing; D5.11 gives `update_block` the `expected_hash` parameter instead. The plan's tool name survives at the MCP layer if RFC-007 wants it — that is a naming decision about a wire protocol, not about the SQL surface. *Recorded 2026-08-06: this deliverable was named in the plan, absent from this RFC as accepted, and therefore absent from the gates, which is how it went missing.*
- **Signature changes** — adding `expected_head` to seven existing `knowledge.*` functions cannot be done with `CREATE OR REPLACE` across an arity change; the extension upgrade script must `DROP FUNCTION` then `CREATE`. Verified, and the `DROP` is load-bearing rather than tidy: leaving both overloads makes every existing call ambiguous (`ERROR: function … is not unique`). That is RFC-012's problem and it is named here so it does not arrive unannounced.
- **Two upgrade mechanics worth recording, both verified on PG 18.4.** `pg_extension_config_dump()` may only be called from a `CREATE EXTENSION` or `ALTER EXTENSION … UPDATE` script — an interactive call raises — so registration of the new tables must live in the upgrade script itself. And `pg_dump` emits `CREATE EXTENSION` with **no version**, so restoring a Phase-3 dump onto a host still packaging Phase 2 fails partway through the restore (`column "history_floor" … does not exist`) rather than at the top; the dump-restore gate's runbook must say so. *(Refuted by the same experiment: a feared FK-ordering hazard in the new tables. `pg_dump` orders `COPY` FK-topologically regardless of `extconfig` order, and plain `psql -v ON_ERROR_STOP=1` restored all twelve tables cleanly — the comment at [schema.rs:204](../../extension/src/schema.rs#L204) claiming dump order follows registration order is simply wrong, and harmless.)*

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

**5.0 Harness contract.** (a) `pending` is not a CI status: under `PGMIND_GATE_STRICT=1` (set in CI) a missing tool is a *failure*, never a skip. (b) **Every suite ships a negative control.** `make eval-selftest` runs each suite against an injected defect — `pgmind.break_history`, `pgmind.disable_serialization`, `pgmind.break_excision`, `pgmind.break_dump_registration`, all admin-only and refused unless `pgmind.allow_fault_injection = on` — and asserts the suite reports `fail`. A suite whose selftest does not fail is not a gate, and this RFC is not accepted without them. (c) No absolute cross-machine timing threshold: thresholds are ratios of two measurements from the same invocation, and **none tighter than 1.5×**. *Amended 2026-08-06: the accepted text said 1.25×, derived from a ±13% band as though it were one-sided. The published growth curve — an identical 500-note workload, one invocation, one machine — runs 3.870 to 5.024 ms/note, `max/min = 1.298`, so the accepted threshold was already exceeded by the repo's own committed measurement and the gate would have flaked on arrival. The two-sample bound for a ±13% spread is (1+s)/(1−s) ≈ 1.30; 1.5× leaves margin for a real regression to still be caught.* (d) Every published artifact carries `pg_version`, `build_profile` (`release`, or the suite fails), host, commit and the raw per-round samples. (e) **Anti-vacuity checks never use the same source as the thing they check** — a sweep enumerated from a privilege-filtered view cannot also supply the expected count (D7.5's measured failure).

| suite | metric | threshold |
|---|---|---|
| **`history-fidelity`** | 40 fixture notes + 2 000 fuzz documents, each driven through a seeded 250-operation stream over every mutating verb; after each op the harness records `read()` bytes and a digest of the `blocks()` rowset. Mismatches between the recording and `read_as_of` / `blocks_as_of` at every revision; `verify_note` + `verify_history` violations. | **0 / 0 / 0.** Determinism, not quality. |
| **`concurrency-isolation`** | `pg_isolation_regress` specs: CAS mismatch, CAS-vs-byte-identical-write, CAS-against-a-deleted-note, concurrent append, disjoint patch, write-vs-op, op-vs-op, move-vs-write, move-into-a-path-being-created (⇒ PM015, never 23505), delete-vs-append, excise-vs-write, and the same permutation at READ COMMITTED (⇒ PM0xx) and REPEATABLE READ (⇒ 40001). Golden output may contain no uuid, timestamp or byte count — if an interleaving cannot be observed without printing an id, the API is wrong. **The binary's provenance is named, not assumed:** measured, `pg_isolation_regress` ships with a pgrx-*built* Postgres and with **none** of the PGDG packages CI installs (`postgresql-18`, `postgresql-server-dev-18`, `postgresql-server-dev-all` contain no `isolation`, `pg_regress` or `pgxs` files). CI must therefore run this leg against a pgrx-built cluster, and §5.0(a) turns its absence into a red build rather than a skip. | exit 0, zero diffs |
| **`concurrency` (block CAS)** | *Added 2026-08-06 with D5.11.* The "disjoint patch" case above, made explicit and failable: two writers patch **different** blocks of one note, each asserting only its own block's `content_hash` — both must succeed, and the note must end with both edits. Then the same pair with `expected_head` instead of `expected_hash`, which must produce PM009 for the second writer — the false conflict, pinned, so the difference between the two guards is a measured fact rather than a claim in this document. Plus: a stale `expected_hash` ⇒ PM016; a stale hash on a note whose *head* also moved ⇒ PM009 (the coarser guard is checked first); `expected_hash` on a block that no longer exists ⇒ PM003. | both disjoint patches land; every code exact |
| **`concurrency-load`** | 8 writers × 60 s over 200 notes, mixed verbs, `psql` co-processes rendezvousing on `pg_blocking_pids()` (never `pg_sleep`, no new Python dependency). Failable: every returned revision present in its chain; `seq` unique and gapless; one root, no forks; every acknowledged append present exactly once; `verify_*` clean. | **0 violations.** Conflict rate, retry histogram and throughput published, not thresholded. |
| **`storage-growth`** | 1 000 notes driven to depth 1/10/50/100 under three shapes (one-block patch, whole-document write, structural insert) plus a 100-block variant. Publishes bytes per table for **every** table enumerated at run time, the **effect-rows-per-revision histogram by verb**, both ratios (vs current state *and* vs full-copy-per-revision) with the denominators in the key names, `as_of`/`diff`/`blame` p95 per depth, and bytes actually returned to the OS by retention. | Report-schema clause (both ratios, histogram, release build, one row per pgmind table) plus two in-run ratios: write cost at depth 100 ÷ depth 0 ≤ **1.5** (§5.0(c)); `as_of` p95 ÷ same-suite `read` p95 ≤ **25**. Both frame-cadence ends are measured — storage at `frame_every` ∈ {10, 50, 200} against deep-`as_of` p95 — since D9 trades one for the other. Everything else honest numbers. Publishes `capacity-model-v2.json`; **v1 is never edited.** |
| **`excision-completeness`** | 13 scenarios × unique canary and path segment: live block, block removed 50 revisions ago, block whose only carrier is a frame, block spanning frames, split, merged-away, whole note, moved note, tombstoned note, excise-then-compact, compact-then-excise, linked-from-elsewhere, repeat excision. Sweep counts occurrences of the canary and its **BLAKE3** (RFC-002 D7's hash — not SHA-256) across every text/jsonb/bytea/array column of every pgmind table enumerated **from `pg_catalog`** at run time, **and in raw `pg_dump` output**. *Amended 2026-08-06: the affected uuids are struck from the swept set — X2 requires the id vectors to survive redaction, so a 0-hit clause over uuids was unsatisfiable by construction; and `pgmind.excision_replay` is a named expected-hit exception rather than a silent skip.* **The dump leg is reordered to test the direction that can fail:** dump *before* the excision, excise, restore that older dump, run `enforce_excisions()`, sweep. As accepted, the dump was taken after the erasure and therefore contained nothing to find — a stub `enforce_excisions()` returning 0 would have passed. | **0 hits**, `verify_excision` empty, log row with correct counts, every refusal clause asserted individually, and one scenario per escalation trigger asserting arity is preserved and co-located spans were shifted (D7.3/D7.6(d)). Anti-vacuity, per §5.0(e): the scanned set is compared against a constant expected table list, never against the catalog view the sweep itself used, and the suite runs once as the extension owner and once as a Law-11 admin role, asserting identical results. |

A sixth suite is added after review, because three of the five above were provably green on a broken system without it:

| suite | metric | threshold |
|---|---|---|
| **`history-durability`** | Extends RFC-003 §5.6's `dump-restore` (§D11 declares the amendment): the reference vault gains history at depth ≥ 3, and the count set is **enumerated from `pg_catalog` at run time** rather than the six hardcoded names. Asserts equal counts for every pgmind table before and after a plain-`psql` restore, non-zero history counts in the reference vault, `verify_history` clean, and — separately — that every `pgmind` table appears in `pg_extension_config_dump`'s registration set. Also asserts the tenant boundary covers every `vault_id`-carrying table, compared against a `pg_catalog` count. | equal counts, non-zero history, zero violations, policy count = table count. Selftest: `pgmind.break_dump_registration` (drop one table's registration) ⇒ must fail. |

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
