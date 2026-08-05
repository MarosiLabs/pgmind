# RFC-003: Vault & Block Storage Layout — Law 8 deferred in Phase 2 (current-state storage until RFC-005)

- **Status:** Living (accepted 2026-08-05; Phase 2 active — amendments during phase land in place per §12 lifecycle; amended same day after adversarial review: dump/restore contract, RLS trust model, FK topology, splice semantics, resolution repair; amended again after Phase-2 code review: GUC parse agreement + shipped RLS helper (D1), normalization symmetry (D5), seam-scoped separator synthesis + PM008 with no outside set (D6), published capacity path (D8))
- **Phase:** 2
- **Owner:** project author
- **Created:** 2026-08-05 · **Accepted:** 2026-08-05 · **Frozen:** —

## 1. Context

Phase 2 makes the vault real: notes with paths, per-block rows, a link graph, and a tag index living in PostgreSQL tables — the point where pgmind stops being a parser and becomes a knowledge base ([product plan §16](../PRODUCT-PLAN.md), handbook [§6.3](../../PGMIND.md)). This RFC decides the storage layout: table shapes, indexes, the multi-tenancy column, path-grammar enforcement, dangling-link representation, frontmatter-tag modeling, the write-path mechanics that maintain it all incrementally, and the capacity model. Identity — which UUID each block gets and keeps — is RFC-004's; this RFC provides the columns identity lives in.

Two prior decisions shape everything here. First, the audit's C3 finding: a monolithic document datum is the trap that makes every edit a whole-document TOAST rewrite (Law 3 prohibits it). Second, RFC-002's frozen parse model: addressable blocks only (containers are *tiling spans*, not nodes — `Document.top_level` tiles the source byte-exactly with the preamble). The storage layout mirrors that model one-to-one, because a storage shape that fights its parser invents translation bugs at the exact boundary that must be byte-faithful.

Binding laws: 3 (per-block rows are the record), 7 (incremental maintenance only), 9 (feel like Postgres and like a vault), 2 (no network anywhere), 11 (admin surfaces marked as such).

## 2. Decision

### D1. Schemas, tenancy, and the current vault

- Storage lives in schema **`pgmind`**; the public API stays in **`knowledge`** (naming ratified project-wide by RFC-007; this RFC establishes the storage half).
- Every storage table carries **`vault_id uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'`** — the *default vault*. There is no vault registry table in v1: a vault is a namespace value, nothing more. `NOT NULL` with a well-known default (rather than the plan sketch's nullable column) keeps RLS policies a single indexed equality with no `IS NOT DISTINCT FROM` footguns, and costs single-tenant users nothing.
- `vault_id` is **denormalized onto every table** (not just `note`) so an RLS policy is a local predicate, never a subquery join. The write path maintains the copies; the `tenant-isolation` gate proves them.
- API functions operate in the **current vault**: GUC **`pgmind.vault_id`** (userset, uuid literal, default = the default vault). Function signatures stay path-only; multi-tenant callers `SET pgmind.vault_id` per session/transaction. A malformed GUC value errors (`PM001` family) at first use.
- **The extension's GUC parse MUST accept exactly what PostgreSQL's `uuid` input accepts** — the canonical `8-4-4-4-12` dashed form and the bare 32-hex form, each optionally brace-wrapped, and nothing else. *Amended 2026-08-05 (post-Phase-2 review).* A hand-rolled parser drifted from the cast in two directions and both were exploitable by any role, since the GUC is userset: `u8::from_str_radix` accepts a leading `+`, so `'+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0+0'` resolved to the all-zeros **default vault** — silently filing a tenant's writes into a shared vault — while the RLS predicate below raised `22P02` on that identical string, leaving the extension and the policy disagreeing about which vault the session was in. A second bug rejected valid input by counting *bytes* and then slicing at byte offsets, panicking mid-character on a 32-byte multibyte value instead of raising `PM001`. Pinned by `malformed_vault_guc_raises_pm001`.
- **RLS is not enabled by default**, but the policy is **shipped as SQL, not only as prose**: `pgmind.enable_vault_rls(force boolean DEFAULT false)` applies the pattern below to all six tables, idempotently. *Amended 2026-08-05 (post-Phase-2 review).* Shipping this only as a documented recipe meant a deployer who granted table access and skipped the recipe got a role that could read and write every vault by `SET`ting one GUC — the gap between "scoping" and "boundary" with nothing in the artifact to close it. It stays opt-in (calling it changes behaviour for existing single-vault installs), and it does not replace the grant-anchored variant below for SQL-capable tenants. The pattern:

```sql
ALTER TABLE pgmind.note     ENABLE ROW LEVEL SECURITY;  -- likewise: tile, block, edge, tag, revision
CREATE POLICY vault_isolation ON pgmind.note
  USING (vault_id = current_setting('pgmind.vault_id')::uuid);
```

All `knowledge.*` functions are **`SECURITY INVOKER`** (plan §13), so the policy actually applies to them. This policy **fails closed**: a session that has neither `SET pgmind.vault_id` nor loaded the extension errors on `current_setting` rather than seeing rows (deliberate — do *not* rewrite it with a two-arg `current_setting(…, true)` COALESCE-ing to the default vault; that fails *open* into the default vault, empirically verified). The pattern assumes non-owner application roles: table owners bypass RLS unless `FORCE ROW LEVEL SECURITY` is applied.
- **The trust model, stated normatively.** A userset GUC is settable by any role, so the pattern above provides vault *scoping* — accident-proofing and query hygiene — and is a tenant *boundary* only when a trusted layer (the application, a pooler, the MCP server) owns the connection and tenants cannot issue arbitrary SQL. For SQL-capable tenants, the documented boundary variant anchors on the role: an admin-owned grant table `pgmind_app.vault_grant(grantee name, vault_id uuid, PRIMARY KEY (grantee, vault_id))` with tenants granted `SELECT` only (self-granting fails on INSERT permission), and the policy `USING (vault_id = current_setting('pgmind.vault_id')::uuid AND vault_id IN (SELECT vault_id FROM pgmind_app.vault_grant WHERE grantee = current_user))` — current-vault selection survives, the grant bounds it (verified against a hostile `SET`). This is a recipe over user-owned objects, not extension DDL. (A `PGC_SUSET` GUC plus PG15+ `GRANT SET ON PARAMETER` is a documented alternative for deployments that prefer parameter ACLs.) RFC-007 owns per-session tenant/role selection for the MCP surface and builds on this model.

### D2. The two-lane storage model

A note is stored in two lanes, both per-block scale, mirroring RFC-002's parse model exactly:

- **Byte lane — `pgmind.tile`:** one row per *top-level document child* (RFC-002 D6's tiling unit: a top-level paragraph, or a whole list/quote container), holding its raw source slice including trailing blank-line trivia. Invariant: `source = note.preamble ‖ concat(tile.raw ORDER BY ord)`, byte-exact. This is what makes `read()` byte-faithful through tables.
- **Semantic lane — `pgmind.block`:** one row per *addressable block* (RFC-002 D2 taxonomy; containers have no rows, exactly as they have no `Block` entries in the parser). Rows carry the identity UUID, normalized content, content hash, and structural addressing.

Blocks locate their bytes by **tile-relative spans** (`tile_ord`, `start_in_tile`, `end_in_tile`): blocks in tiles whose `raw` is unchanged *and whose position is unchanged* are never touched — content edits leave every other row physically untouched (the extraction gate verifies this via system-column stability); structural edits (insert/remove/reorder of blocks or tiles) additionally renumber `ord`/`tile_ord` of subsequent rows, bounded by note size (see the fractional-ordering deferral in §3). Container structure (quote depth, list style) is not stored redundantly: when an edit needs container context, the write path re-parses the affected tile (parsing is cheap and pure; storage stays minimal). Neither lane is a monolithic datum; the C3 pathology — every edit rewrites the whole document's TOAST — cannot occur because an edit rewrites only the affected tiles.

### D3. Table shapes (normative DDL)

Types and defaults are normative; storage parameters at the end of this section. All UUIDs are minted by the extension per RFC-004 A1 (UUIDv7 — `gen_random_uuid()` appears nowhere). Emission ordering is normative: `pgmind.path_is_valid`/`path_normalize` (D5) MUST be created **before** the table DDL (the `note` CHECK depends on them; pgrx `extension_sql!(…, requires = […])` pins the order), and the schema statement is `CREATE SCHEMA IF NOT EXISTS` so it composes with pgrx's `#[pg_schema]` emission.

```sql
CREATE SCHEMA IF NOT EXISTS pgmind;

CREATE TYPE pgmind.block_kind AS ENUM
  ('heading','paragraph','list_item','code_block','table','thematic_break','html_block');
CREATE TYPE pgmind.edge_kind AS ENUM ('wikilink','transclusion','blockref','mdlink');

CREATE TABLE pgmind.note (
  id            uuid PRIMARY KEY,
  vault_id      uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
  path          text NOT NULL CHECK (pgmind.path_is_valid(path)),
  basename      text GENERATED ALWAYS AS (regexp_replace(path, '^.*/', '')) STORED,
  properties    jsonb NOT NULL DEFAULT '{}'::jsonb,
  preamble      text NOT NULL DEFAULT '',
  head_revision uuid NOT NULL,                    -- DELIBERATELY no FK: a note↔revision circular FK makes
                                                  -- pg_dump warn and plain-psql restore impossible in any
                                                  -- COPY order (verified); the write path guarantees
                                                  -- validity, verify_note (D7) polices it
  created_at    timestamptz NOT NULL DEFAULT now(),
  tombstoned_at timestamptz                       -- written only by RFC-005 machinery (Phase 3)
);

CREATE TABLE pgmind.revision (
  id         uuid PRIMARY KEY,
  vault_id   uuid NOT NULL,
  note_id    uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  parent     uuid REFERENCES pgmind.revision(id),  -- NULL ⇒ first revision of the note
  author     text NOT NULL DEFAULT current_user,
  source     text NOT NULL DEFAULT 'api' CHECK (source IN ('api','sync','rebind')),
  message    text,
  meta       jsonb NOT NULL DEFAULT '{}'::jsonb,   -- op provenance (RFC-004 A4) until RFC-011
  created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE pgmind.tile (
  note_id  uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id uuid NOT NULL,
  ord      int4 NOT NULL,                          -- dense 0..t-1 per note
  raw      text NOT NULL,                          -- source slice incl. trailing trivia (D2)
  PRIMARY KEY (note_id, ord) DEFERRABLE INITIALLY DEFERRED
  -- deferrable because structural edits renumber ords in-place (a non-deferred unique
  -- constraint fails transiently mid-UPDATE); legal only because nothing references this PK
);

CREATE TABLE pgmind.block (
  id            uuid PRIMARY KEY,                  -- the identity (RFC-004)
  note_id       uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id      uuid NOT NULL,
  ord           int4 NOT NULL,                     -- dense document order among addressable blocks
  parent_block  uuid REFERENCES pgmind.block(id),  -- enclosing addressable block; NO ACTION on delete:
                                                   -- a wrong reconcile order fails loudly instead of
                                                   -- cascading away carried children (D6 ordering rule)
  kind          pgmind.block_kind NOT NULL,
  heading_path  text[] NOT NULL DEFAULT '{}',
  content       text NOT NULL,                     -- RFC-002 D7 normalized logical content
  content_hash  bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  block_ref_id  text,                              -- ^id marker; a label, never identity (Law 4)
  tile_ord      int4 NOT NULL,
  start_in_tile int4 NOT NULL CHECK (start_in_tile >= 0),
  end_in_tile   int4 NOT NULL CHECK (end_in_tile >= start_in_tile),
  attrs         jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT block_note_ord UNIQUE (note_id, ord) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE pgmind.edge (
  id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,  -- edges carry no cross-edit identity
  vault_id      uuid NOT NULL,
  src_note      uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  src_block     uuid NOT NULL REFERENCES pgmind.block(id) ON DELETE CASCADE,
  kind          pgmind.edge_kind NOT NULL,
  dst_path      text NOT NULL,                     -- target as written (NFC, trimmed)
  dst_heading   text,                              -- #Heading anchor text (NFC)
  dst_block_ref text,                              -- #^id anchor
  alias         text,
  dst_note      uuid REFERENCES pgmind.note(id),   -- NO ACTION: note removal semantics are RFC-005's;
                                                   -- SET NULL would violate the dangling CHECK below
  resolved_via  text CHECK (resolved_via IN ('exact','basename')),
  dangling_reason text CHECK (dangling_reason IN ('missing','ambiguous','invalid')),
  CHECK ((dst_note IS NULL) = (dangling_reason IS NOT NULL)),
  CHECK ((dst_note IS NULL) = (resolved_via IS NULL))   -- demotion clears resolved_via (D5)
);

CREATE TABLE pgmind.tag (
  id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  vault_id uuid NOT NULL,
  note_id  uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  block_id uuid REFERENCES pgmind.block(id) ON DELETE CASCADE,   -- NULL ⇒ frontmatter (note-level)
  tag      text NOT NULL                                          -- stored as written
);

-- Backups: extension-script tables are SKIPPED by pg_dump unless registered (verified:
-- unregistered, every vault silently vanishes from every backup). Registration order is
-- normative — pg_dump emits config-table COPY in registration order, and this order is
-- the FK-topological one that restores under plain autocommit psql:
SELECT pg_catalog.pg_extension_config_dump('pgmind.note',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.revision', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tile',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.block',    '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag',      '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge_id_seq', '');  -- identity sequences too —
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag_id_seq',  '');  -- else restored PKs collide
```

(The identity-sequence names above are the pgrx-emitted defaults; the implementation registers whatever names the emitted DDL produces.) With `head_revision` deliberately FK-free and `parent_block`/self-referencing FKs checked at end of statement, this registration order restores under a plain `psql < dump.sql`; the `dump-restore` gate (§5) proves it on every CI run.

Notes on deliberate shapes:

- **A note's title is its last path segment** (the Obsidian rule: the title *is* the filename) — served by the generated `basename` column, which link resolution needs anyway; no separate stored `title` (it would always be equal), and no frontmatter override in v1 — RFC-002 D4's reserved-key list is frozen; a future RFC may add `title` to it, and would add the column then. This deviates from the plan-§6 sketch's stored `title`, deliberately.
- **`block_ref_id` is deliberately not unique** per note. Real vaults contain duplicate `^id`s; import must never error on legal markdown (RFC-002 D4 philosophy). Resolution and identity claims both take the lowest `ord` (RFC-002 D8, RFC-004 A3).
- **`revision` rows in Phase 2 are metadata-only** (head tracking + provenance). Reconstructable history begins when RFC-005's engine lands; recording author/source/meta from the first write makes Phase 3 additive instead of a retrofit. Consequence stated in §4.
- **No composite FK from `block(note_id, tile_ord)` to `tile`**: Postgres requires FK targets to be non-deferrable, and tile renumbering needs in-transaction PK updates. The invariant is enforced by the write path and checked by `pgmind.verify_note()` (D7) in every gate run.
- **Frontmatter tags are note-level** (`block_id IS NULL`) — ratifying the plan-sketch shape: they annotate the note, not any block, and this keeps `tagged()` one predicate.
- **Deliberately absent from the Phase 2 schema** (so the plan-§6 sketch's remaining tables are not silently dropped): `block_revision` and `excision_log` arrive with RFC-005 (Phase 3), `embedding_hook` with RFC-009 (Phase 6). Nothing in Phase 2 depends on their shapes.

### D4. Indexes (normative) and storage parameters

```sql
-- note: path lookup, live-path uniqueness, basename resolution, glob prefix scans
CREATE UNIQUE INDEX note_live_path ON pgmind.note (vault_id, path) WHERE tombstoned_at IS NULL;
CREATE INDEX note_basename ON pgmind.note (vault_id, basename) WHERE tombstoned_at IS NULL;
CREATE INDEX note_path_prefix ON pgmind.note (vault_id, path text_pattern_ops);

-- block: per-note traversal (covered by block_note_ord), Stage-0 carry, ^id resolution,
-- and the parent FK (RI checks on block deletion — without it, every delete is a
-- full-table scan per referencing table; ~100x measured)
CREATE INDEX block_note_hash ON pgmind.block (note_id, content_hash);
CREATE INDEX block_note_ref  ON pgmind.block (note_id, block_ref_id) WHERE block_ref_id IS NOT NULL;
CREATE INDEX block_parent    ON pgmind.block (parent_block) WHERE parent_block IS NOT NULL;

-- edge: links(), backlinks(), lazy re-resolution + ambiguity demotion, src-block RI
CREATE INDEX edge_src       ON pgmind.edge (src_note);
CREATE INDEX edge_src_block ON pgmind.edge (src_block);
CREATE INDEX edge_dst       ON pgmind.edge (dst_note) WHERE dst_note IS NOT NULL;
CREATE INDEX edge_path      ON pgmind.edge (vault_id, dst_path);

-- tag: case-insensitive tag lookup, per-note maintenance, block RI
CREATE INDEX tag_lookup ON pgmind.tag (vault_id, lower(tag));
CREATE INDEX tag_note   ON pgmind.tag (note_id);
CREATE INDEX tag_block  ON pgmind.tag (block_id) WHERE block_id IS NOT NULL;

-- revision: history-by-note (Phase 3 reads; cheap now)
CREATE INDEX revision_note ON pgmind.revision (note_id, created_at);
```

Tombstoned paths are reusable (partial unique index); note IDs are never reused. No bare `vault_id` index and no FTS column in v1 — the first is unproven need (RLS filters don't require it for correctness; the capacity suite measures whether stats/scans want it), the second belongs to the search RFC (RFC-007/010) with its language-configuration decision; adding a generated tsvector column later is an upgrade-script table rewrite, accepted pre-1.0 (RFC-012).

Storage parameters: `ALTER TABLE … ALTER COLUMN … SET COMPRESSION lz4` on `tile.raw`, `block.content`, `note.preamble`, and the jsonb columns (handbook §6.3). Servers built without `--with-lz4` reject this: the install script MUST attempt it and fall back to the server default with a `WARNING` rather than fail installation (lz4 is recommended-not-required; RFC-012 records it as a packaging recommendation). `revision.parent` and `note.head_revision` remain unindexed referencing columns — harmless in Phase 2's no-delete regime, flagged for RFC-005 which introduces deletion paths. Autovacuum tuning is deliberately *not* set in v1: the capacity suite publishes observed bloat/vacuum behavior first; tuning by measurement, not folklore.

### D5. Path grammar enforcement & link resolution lifecycle

- **`pgmind.path_is_valid(text) → bool`** and **`pgmind.path_normalize(text) → text`** (both `IMMUTABLE`, thin wrappers over pgmind-core's RFC-002 D8 grammar) are public. The write path normalizes (NFC, trim) then validates — an invalid note path is error `PM001`; the table CHECK is the backstop against direct-SQL corruption.
- **Normalization is symmetric: every path-taking entry point normalizes, not just `write()`.** *Amended 2026-08-05 (post-Phase-2 review).* Normalizing only on the write side made `write()` accept a strictly larger set of strings than the readers could find: `knowledge.write('notes/café' /* NFD */, …)` stored the NFC spelling, and `knowledge.read` with the identical argument raised `PM002` on a note it had just created. macOS emits NFD, so RFC-006's sync bridge is exactly the consumer that would hit it. The rule belongs at the shared lookup, not at each entry point — implementations MUST normalize inside the by-path note lookup so `read`, `read_section`, `blocks`, `links`, `backlinks` and `insert_blocks` cannot drift from `write`. Pinned by `paths_round_trip_through_normalization`.
- Edges store the target **as written** (NFC-trimmed); grammar-violating targets are dangling with `reason='invalid'` (RFC-002 D8) — never an error.
- **Resolution is note-level and incremental** (Law 7). On edge creation: exact path match in the current vault → `resolved_via='exact'`; else, for slash-free targets, unique live-basename match → `resolved_via='basename'`; two-plus candidates → `dangling='ambiguous'`; zero → `dangling='missing'`.
- **On note creation** (the only resolution-relevant event in Phase 2 — rename/delete arrive in Phase 3, whose RFC must extend this lifecycle), the write path **re-runs full D8 resolution for every edge whose outcome the new note could change** — those found via `edge_path` with `dst_path` = the new path, or slash-free `dst_path` = the new basename — so incremental maintenance is *definitionally equal* to full recomputation. Concretely: an edge whose `dst_path` exactly equals the new path re-resolves to it with `resolved_via='exact'` — **including an edge previously basename-resolved to a different note** (exact categorically precedes basename, frozen RFC-002 D8; a root-level note creation can therefore *promote* a basename match to an exact one, never demote it); a dangling `missing` edge whose slash-free target now has exactly one live basename candidate resolves via `basename`; a basename-resolved edge whose slash-free target now has two-plus candidates (and no exact match) **demotes** to `dangling='ambiguous'` with `resolved_via` cleared. Deterministic, index-backed, no scans of unrelated rows.
- **Sub-note anchors (`#Heading`, `#^id`) resolve at query time**, not in storage. Eager `dst_block` resolution would make every heading edit in a note invalidate *incoming* edges from every other note — a cross-note write amplification Law 7 exists to forbid. Readers (`backlinks()`, Phase 5 `context()`) resolve anchors on demand via `block_note_ref` and heading-path lookups; the plan-sketch `dst_block` column is dropped (Alternatives).

### D6. Write path (normative pipeline)

`knowledge.write(path text, doc markdown) → uuid` (revision ID; upsert; `expected_head` CAS arrives with RFC-005):

1. Normalize + validate the path (`PM001` on failure).
2. **Idempotence short-circuit:** if the note exists and `preamble ‖ tiles` equals the new source byte-for-byte, return the current head revision — no new revision row, zero row churn. (This is what makes Phase 4 re-import a no-op, plan §12.)
3. Parse (pgmind-core, RFC-002 — the *only* parser; SQL never re-implements it).
4. Run RFC-004's identity carry (A3) against the note's existing blocks → each parsed block gets a carried or minted UUID.
5. Upsert `note` (path, properties, preamble); insert the `revision` row (parent = old head) and swap `head_revision`.
6. Reconcile lanes by set-diff: tiles whose `raw` is unchanged at unchanged `ord` are untouched; block rows are `INSERT`/`UPDATE`/`DELETE`d minimally — a row is written only if one of its stored columns actually changed. **Ordering is normative:** carried rows' positional columns (including `parent_block`) are `UPDATE`d and minted parents `INSERT`ed *before* any removed-row `DELETE` executes, and removals are subtree-safe (one statement, or children before parents) — the `parent_block` NO ACTION FK turns a violation of this order into a loud error instead of a silent cascade of carried children. Removed blocks' rows are deleted (Phase 2 is current-state storage; Phase 3 turns removal into tombstoned history — §6).
7. Reconcile extraction by set-diff on natural keys (`edge`: src_block+kind+dst_path+anchors+alias; `tag`: note+block+tag). Identical occurrences of one natural key within one block **dedupe to a single row** (`[[x]] and [[x]]` in a paragraph is one edge; golden-cased). **The diff is against the full new parse, never per-changed-block**: CommonMark makes extraction document-global (adding a `[foo]: url` reference definition in one paragraph changes whether `[[foo]]` elsewhere is a wiki-link — RFC-002 D3). Set-diff gives Law 7's *no-rebuild, no-churn* property without pretending extraction is block-local.
8. Resolve new/changed edges; run the D5 creation-repair pass if this write created the note.

Block-level operations (`insert_blocks`, `update_block`, `move_block`, `split_block`, `merge_blocks` — identity semantics in RFC-004 A2; each returns the named composite **`pgmind.op_result (revision uuid, block_ids uuid[])`**, created in the extension DDL) share steps 5-8 and edit bytes surgically. `insert_blocks` is deliberately plural against the plan's `insert_block` — a fragment may contain several blocks, all minting; with neither `before` nor `after` given it appends after the last top-level block.

Splice mechanics, normative:

- **Locus.** Locate the target's tile-relative span and splice the replacement bytes there. Newly written interior lines take **canonical decoration** (quote prefix `"> "`, item continuation = marker-width spaces, fragment list markers re-styled to the destination list's marker/numbering); bytes outside the spliced span are never reformatted. "Affected tile" names only this splice locus — never the parse or reconcile scope.
- **Separator synthesis.** After any structural splice at top level (move, insert, split/merge of top-level blocks), every tile **at the splice seam** except the note's final tile MUST end with a blank line, synthesized if absent; the note's final tile keeps exactly the trailing trivia it had before the operation. The seam is the inserted/relocated tile run plus the tile immediately preceding it — the only boundaries the splice created.

  *Amended 2026-08-05 (post-Phase-2 review).* "Every affected tile" was implemented as *every tile in the note*, which contradicts this same bullet's "bytes outside the spliced span are never reformatted": adjacent tiles may legitimately have no blank line between them (`para\n# Heading\n` is two tiles), and any later insert silently rewrote them to `para\n\n# Heading\n\n`. PM008 cannot catch it — RFC-002 D7 strips trailing newline runs before hashing, so every outside block's kind and `content_hash` are unchanged — and gate 2's churn-discipline test missed it because every fixture was already blank-separated. Scoping synthesis to the seam is what the no-reformat rule always required. Pinned by `insert_preserves_unseparated_neighbouring_tiles`. Without this, reordering `A\n\n` before `B\n` yields `B\nA\n\n` — one merged paragraph (verified). Synthesized trivia is part of the spliced span (the no-reformat rule holds) and is hash-neutral: RFC-002 D7 step 7 strips trailing newline runs, so `move_block`'s "content and hash unchanged" contract survives. (Distinct from RFC-004's rejected *merge joiners*: that rejection concerns block content; this rule concerns inter-tile trivia.)
- **Parse scope is the whole note.** After splicing, re-parse the FULL reassembled source (preamble ‖ tiles) — the same parse step 7 already requires, so this costs nothing — and reconcile all lanes and extraction from it. Tile-local re-parsing is unsound: an unclosed fence or type-1 HTML block in a fragment swallows every following tile, and a spliced paragraph can be absorbed backward by a preceding construct (both verified against the parser).
- **Post-splice assertion (PM008).** From the full re-parse, assert: (a) the parse's top-level tiling equals the reconciled tile rows byte-exactly (the same predicate `verify_note` checks), and (b) the op's RFC-004 A2 postconditions hold — the block multiset outside the spliced region is unchanged (positional facts may move), and the fragment surfaces as exactly the op's expected block set. On failure raise **`PM008 pgmind_splice_restructures`** (DETAIL names the absorbing/absorbed tile ords) and abort — rejecting is the v1 semantics; silently retiring neighbor IDs would violate A2's "created or targeted" contract. Whole-document `write()` is immune by construction: its tiles always derive from the step-3 full parse.

  *Amended 2026-08-05 (post-Phase-2 review).* Clause (b) MUST hold **for every op, including those with an empty outside set**. `move_block` rewrites the whole note and pins every block by permutation, so "the block multiset outside the spliced region is unchanged" compared an empty set to an empty set and asserted nothing: a re-parse that produced *more* blocks than the original left the surplus unpinned, and it minted a fresh ID with no error — precisely the silent outcome this clause forbids. When there is no outside set, the assertion is instead made directly: the new parse's block count MUST equal the old parse's, and every new block MUST be covered by a pin. Implementations MUST NOT encode "this check does not apply here" as a sentinel range whose emptiness depends on downstream arithmetic.

v1 structural limits (relaxed by RFC-005, which owns `append_to_section` and richer targeting): `insert_blocks` anchors at top level (or item-level when the fragment parses as a single list); `move_block` reorders siblings within one container or at top level; cross-container moves are `PM006`.

Everything is maintained by these explicit code paths — **no triggers, no rules, no hidden behavior** (Law 9). Direct DML into `pgmind.*` bypasses maintenance and is unsupported (plan §5's API contract); `verify_note` exists to detect it.

### D7. Read/navigate API (Phase 2 surface)

Path-taking functions accept `text` and scope to the current vault; unknown-typed literals resolve to the `text` overloads over Phase 1's `markdown` ones (string-category preference — golden-cased). Signatures (shapes normative; RFC-007 freezes them for 0.x):

```sql
knowledge.read(path text) → markdown                       -- preamble ‖ tiles, byte-faithful
knowledge.read_section(path text, heading_path text[]) → markdown
                                                           -- heading + subtree slice; first match in
                                                           -- document order (RFC-002 D2); PM007 if absent
knowledge.notes(glob text DEFAULT '**')
  → TABLE (path text, title text, properties jsonb, head_revision uuid,
           created_at timestamptz, updated_at timestamptz)  -- updated_at = head revision's created_at
knowledge.blocks(path text)
  → TABLE (block_id uuid, ord int4, kind text, parent_block uuid, heading_path text[],
           content text, content_hash bytea, block_ref_id text,
           span_start int8, span_end int8, attrs jsonb)     -- spans absolute in source, computed from tiles
knowledge.links(path text)
  → TABLE (block_id uuid, kind text, target text, anchor text, alias text,
           resolved_path text, dangling_reason text)
knowledge.backlinks(path text)
  → TABLE (src_path text, block_id uuid, kind text, anchor text, excerpt text)
                                                           -- excerpt = source block's normalized content
knowledge.tags()        → TABLE (tag text, notes bigint, blocks bigint)   -- grouped case-insensitively,
                                                                          -- spelled min(tag) (deterministic)
knowledge.tagged(tag text) → TABLE (path text, block_id uuid, tag text)   -- case-insensitive match
knowledge.orphans()     → TABLE (path text)                -- live notes with zero resolved incoming edges
                                                           -- from OTHER notes (self-links and dangling
                                                           -- edges never count)
knowledge.stats()
  → TABLE (vault_id uuid, notes bigint, blocks bigint, edges_resolved bigint,
           edges_dangling bigint, tags bigint, revisions bigint, bytes bigint)
```

Glob semantics: RFC-002 D8 exactly (`*` within segment, `**` across; nothing else). `notes(glob)` matches live notes only. Admin/debug (Law 11, marked as such): **`pgmind.verify_note(note_id uuid) → SETOF text`** returns invariant violations (tiling identity, span bounds, ord density, lane agreement with a fresh parse) — empty means healthy; the gates run it after every mutation scenario.

### D8. Capacity model (published deliverable)

The model is arithmetic plus measurement, published as `eval/published/capacity-model-v1.json` (+ human-readable notes):

- **Variables:** per-row heap widths for the six tables (measured, not estimated: `pgtoast`-aware, via `pg_column_size`/`pg_total_relation_size` on the synthetic vault), index sizes, blocks-per-note and edges-per-block distributions from the generator (calibrated to Walkthrough A's shape: ~23 blocks, ~3 links, ~0.2 tags per note).
- **Reference measurement (CI scale):** a seeded deterministic generator produces **10k notes / ≥ 230k blocks**; the suite measures total bytes by table+index, bytes/block all-in, `knowledge.write()` throughput (notes/s, single connection), and p95 for `read`, `backlinks`, `tagged` at that scale.
- **Published extrapolation** to the plan §14 design target (100k notes / 10M blocks / 100 revisions avg) with the stated assumptions — including the honest caveat that revision-load behavior is *modeled only* until Phase 3's storage-growth benchmark measures it.
- Numbers are published even when unflattering (plan §18); the Phase 2 gate requires publication, not flattery. The only thresholded number is correctness-adjacent: import throughput is *reported* against the ≥ 2k notes/s design target, not gated on it (the CLI import path that target describes lands in Phase 4).
- **The deliverable is `eval/published/capacity-model-v1.json`, a committed file.** *Amended 2026-08-05 (post-Phase-2 review).* The suite wrote only to `eval/results/`, which is gitignored, so the published artifact never landed in the repo. Three further requirements, all of which the first implementation missed:
  - **"Publish, don't flatter" is not "never fail."** The suite's status MUST be derived from the measurement actually happening — the expected note count stored, a non-zero block count, a clean `verify_note` sweep — not hard-coded to `ok`. A degenerate corpus is a failure, not an honest number.
  - **The timed region is `knowledge.write()` alone.** Staging the synthetic corpus is test scaffolding and MUST be loaded before the timer starts; charging it to the write path understates the published throughput.
  - **The measured artifact MUST be the release build.** `cargo pgrx test` installs its own debug, `pg_test`-enabled build over whatever the harness installed, so a harness that runs the pg_test suites before the measurement suites publishes debug numbers. Whichever step runs last must leave the release artifact in place.

## 3. Alternatives considered

- **Monolithic source column on `note` + derived block index** — rejected: audit C3's exact pathology (whole-doc TOAST rewrite per edit, WAL churn); also makes block rows second-class, inviting drift.
- **Container rows in `pgmind.block`** — rejected: containers carry no identity (RFC-002 made them non-addressable), so their rows would be identity-free tenants of the identity table, and the storage model would diverge from the frozen parse model it must mirror. Container context is re-derivable by re-parsing one tile.
- **Absolute byte spans on block rows** — rejected: editing tile *k* would shift-update every block row after it, destroying the "unchanged rows untouched" property Law 7 and the churn gate depend on. Tile-relative spans localize writes.
- **Eager `dst_block` anchor resolution (plan sketch)** — rejected: heading edits would invalidate incoming edges across the vault (cross-note write amplification); query-time anchor resolution is index-backed and keeps maintenance note-scoped.
- **Nullable `vault_id` (plan sketch)** — rejected: `UNIQUE NULLS NOT DISTINCT` works on PG15+, but every RLS policy and every join would carry null-handling; a well-known default vault UUID is simpler and equally zero-tax.
- **Vault registry table** — deferred: adds a join and admin surface with no v1 behavior behind it; a future RFC can add one compatibly (the column is the contract).
- **Per-revision edge rows (plan sketch's `edge.revision_id`)** — rejected for v1: multiplies edge rows by revision count (row economics, handbook §11 risk table); the link graph indexes *current* state. Historical link queries, if ever wanted, are an RFC-005+ derivation.
- **Fractional/gap ordering (`ord`)** — deferred: dense int matches Phase 1's `ord` exactly and is trivially correct; reorder churn is bounded by note size (≤ 8 MiB documents). A future RFC can switch if capacity data demands.
- **DB triggers for index maintenance** — rejected: hidden behavior (Law 9), fires on unsupported direct DML producing half-maintained state; explicit write-path code + `verify_note` is inspectable.
- **FTS column now** — deferred to the search RFCs with their language-config decision; premature schema is the hardest kind to walk back (upgrade rewrite noted in §4).

## 4. Consequences

*Easier:* Phase 3 versioning bolts onto revision rows that already exist with provenance; the sync bridge gets byte-faithful storage and an idempotent import primitive for free; RLS multi-tenancy is one documented statement per table; every downstream feature (backlinks, orphans, context) is an indexed query, not a scan.
*Harder:* two lanes must agree — `verify_note` and the gates exist because lane drift is this layout's characteristic failure mode; giant single-container documents (one 8 MiB list = one tile) degrade to whole-tile rewrites per edit (measured by the capacity suite; the document-size GUC bounds it).
*Accepted debts:* Phase 2 is **current-state storage** — removed blocks' content is gone until RFC-005's history engine (revisions are metadata-only; stated in walkthrough docs so nobody mistakes pre-Phase-3 pgmind for versioned); adding FTS later rewrites `block`; `updated_at` requires the head-revision join.
*Reversal cost:* changing table shapes after 0.1.0 means `ALTER EXTENSION` migration scripts (RFC-012); changing the tiling model itself (e.g. to container rows) means a full rewrite of the write path — this RFC is the one to get right.

## 5. Benchmark gate

Phase 2 exits when these suites pass in CI and results are published (suite IDs normative; all thresholds 100% unless stated):

1. **`identity-semantics`** — defined in RFC-004 §5 (shared gate; this RFC provides the tables it runs against).
2. **`extraction-correctness`** — seeded from Phase 1's `vault-syntax-extraction` goldens, now through storage: after `write()`, edge/tag/property rows match goldens; resolution lifecycle cases (missing→resolved on note creation; **basename→exact promotion** when a root-level note matching `dst_path` exactly is created; basename→ambiguous demotion *only* when the new note shares the basename at a different path; invalid targets; blockref/mdlink/transclusion kinds); duplicate-reference dedup (one edge row per natural key per block); the reference-definition document-global case; **churn discipline** — a no-op rewrite touches zero rows; a one-paragraph edit leaves all other block/tile rows' `xmin` unchanged; a heading-text edit updates only that heading and its section's descendants (the denormalized `heading_path` — the one edit with deliberately wider, still section-bounded, churn); structural edits renumber only subsequent rows of the same note.
3. **`storage-round-trip`** — `write()` then `read()` is byte-identical across the Phase 1 round-trip corpora plus a 10k-document seeded fuzz sample, through tables; `verify_note` returns empty after every write.
4. **`tenant-isolation`** — two claims, tested separately: (a) *scoping* under the D1 GUC pattern — every D7 function and direct table reads return zero rows outside the selected vault for a non-superuser, non-owner role, and a fresh session that never SET the GUC fails closed (errors, sees nothing); (b) *boundary* under the D1 grant-anchored pattern — a hostile `SET pgmind.vault_id` to an ungranted vault returns zero rows. `stats()` counts only the current vault.
5. **`capacity-model`** — the D8 measurement runs and `eval/published/capacity-model-v1.json` is written with all listed numbers (publish honest numbers; no flattery threshold).
6. **`dump-restore`** — `pg_dump` of the reference vault, plain autocommit `psql` restore into a fresh database (no `--single-transaction` crutch), then: row counts equal across all six tables, identity-sequence positions advance past restored PKs, `verify_note` empty for every note, and a post-restore `write()` succeeds.

Phase 1 suites keep passing (regression is failure).

## 6. Law compliance

- **Law 2:** nothing here performs I/O beyond the database's own storage; no network exists.
- **Law 3:** the record is per-block rows plus per-top-level-child tiles — no monolithic datum; the markdown type remains a boundary (`read` reconstructs, never stores).
- **Law 5:** `block.id` and `block.content_hash` are separate columns with separate indexes and separate meanings, exactly as the law demands.
- **Law 7:** maintenance is set-diff at note scope: unchanged rows untouched (gated via `xmin`), no full rebuilds, no cross-note invalidation (query-time anchors are this law applied to the link graph).
- **Law 8:** revision rows are append-only from the first write. Phase 2 block storage is current-state *by the accepted phase plan* — history is Phase 3's deliverable (RFC-005); this RFC deliberately leaves removed content unrecoverable until then, **declares the deferral in its title** per the handbook's violation rule, and says so out loud rather than half-building a version engine ahead of its RFC.
- **Law 9:** enum kinds, plain tables, ordinary indexes, no triggers, one GUC; API nouns are notes/blocks/links/tags.
- **Law 11:** `pgmind.verify_note` is an admin/debug surface, documented as such; public functions touch only documented storage.
