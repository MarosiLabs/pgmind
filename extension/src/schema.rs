//! Storage schema (RFC-003 D3/D4): the two-lane vault model, emitted at
//! CREATE EXTENSION time. Ordering is normative: the path functions exist
//! before the `note` CHECK that calls them (pgrx `requires` pins it).

use pgrx::prelude::*;

pub const DEFAULT_VAULT: &str = "00000000-0000-0000-0000-000000000000";

#[pg_schema]
pub mod pgmind {
    use pgrx::prelude::*;

    /// RFC-002 D8 grammar check (pure syntax over an already-normalized path);
    /// the storage CHECK constraint's backstop (RFC-003 D5).
    #[pg_extern(immutable, parallel_safe)]
    pub fn path_is_valid(path: &str) -> bool {
        pgmind_core::path::path_is_valid(path)
    }

    /// NFC-normalize + trim a candidate note path (RFC-003 D5). Does not validate.
    #[pg_extern(immutable, parallel_safe)]
    pub fn path_normalize(path: &str) -> String {
        pgmind_core::path::path_normalize(path)
    }
}

extension_sql!(
    r#"
-- Typed-error trampoline (RFC-004 A6): plpgsql RAISE accepts arbitrary
-- SQLSTATEs, which pgrx's closed errcode enum cannot emit directly.
-- search_path is pinned on every function this script defines. Nothing here
-- is SECURITY DEFINER today, so an unqualified `lower()` or operator would
-- only let a caller confuse their own session; pinning it now means the
-- grant-anchored boundary (D1) can adopt SECURITY DEFINER later without this
-- becoming a privilege-escalation retrofit.
CREATE FUNCTION pgmind.raise_error(code text, message text, detail text)
RETURNS void LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $fn$
BEGIN
  RAISE EXCEPTION USING ERRCODE = code, MESSAGE = message, DETAIL = detail;
END
$fn$;
REVOKE ALL ON FUNCTION pgmind.raise_error(text, text, text) FROM PUBLIC;

CREATE TYPE pgmind.block_kind AS ENUM
  ('heading','paragraph','list_item','code_block','table','thematic_break','html_block');
CREATE TYPE pgmind.edge_kind AS ENUM ('wikilink','transclusion','blockref','mdlink');
CREATE TYPE pgmind.op_result AS (revision uuid, block_ids uuid[]);

-- The container. Until now a vault was a bare uuid that sprang into existence
-- the first time somebody wrote a row stamped with it, so it had no name, no
-- description, no existence test, and a typo produced a new empty vault
-- indistinguishable from an empty one.
--
-- Four columns, deliberately. There is no `tenant`: applications own tenancy,
-- and they carry their own hierarchy in the NAME, which reuses the RFC-002 D8
-- path grammar so `acme/alice/agents/billing` is legal and
-- `knowledge.vaults('acme/**')` works with the glob matcher the vault already
-- has. One grammar in the product, not two.
CREATE TABLE pgmind.vault (
  id          uuid PRIMARY KEY,
  name        text NOT NULL UNIQUE CHECK (pgmind.path_is_valid(name)),
  description text,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- The vault every session starts in, so that `CREATE EXTENSION; knowledge.write(…)`
-- still works without ceremony.
INSERT INTO pgmind.vault (id, name, description)
VALUES ('00000000-0000-0000-0000-000000000000', 'default',
        'The vault a session uses when pgmind.vault_id is unset.');

CREATE TABLE pgmind.note (
  id            uuid PRIMARY KEY,
  vault_id      uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000'
                  REFERENCES pgmind.vault(id),
  path          text NOT NULL CHECK (pgmind.path_is_valid(path)),
  basename      text GENERATED ALWAYS AS (regexp_replace(path, '^.*/', '')) STORED,
  properties    jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- What the note calls ITSELF: frontmatter `title`, else its first level-1
  -- heading. Resolved by the write path because both terms need the parse, and
  -- an agent listing 100k notes must not pay an index probe per row to learn a
  -- document's name.
  --
  -- NULL when the note declares neither, and `notes()` falls back to
  -- `basename` on the same row. Storing the fallback instead would go stale on
  -- `move_note`, which changes the basename and knows nothing about titles.
  title         text,
  -- Generated, not stored by the write path: it is a pure function of
  -- frontmatter, so Postgres can maintain it and there is one fewer thing an
  -- op can forget. NULL when the note declares none — pgmind does not invent a
  -- summary from the lead paragraph, because a guessed description that an
  -- agent then trusts is worse than an absent one.
  description   text GENERATED ALWAYS AS (properties->>'description') STORED,
  preamble      text NOT NULL DEFAULT '',
  head_revision uuid NOT NULL,
  -- deliberately no FK on head_revision: a note<->revision circular FK makes pg_dump
  -- warn and plain-psql restore impossible (RFC-003 D3); verify_note polices it
  created_at    timestamptz NOT NULL DEFAULT now(),
  tombstoned_at timestamptz,
  -- RFC-005 D3: the oldest seq whose state can still be reconstructed. A read
  -- below it raises PM011 rather than returning a partially-reconstructed
  -- document. Fresh notes start at 0 (everything they have is reconstructable);
  -- an upgraded Phase-2 vault backfills this to each note's HEAD seq, since
  -- Phase 2 revisions carry no recoverable content (RFC-005 D11).
  history_floor bigint NOT NULL DEFAULT 0
);

CREATE TABLE pgmind.revision (
  id         uuid PRIMARY KEY,
  vault_id   uuid NOT NULL,
  note_id    uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  parent     uuid REFERENCES pgmind.revision(id),
  author     text NOT NULL DEFAULT current_user,
  source     text NOT NULL DEFAULT 'api' CHECK (source IN ('api','sync','rebind')),
  message    text,
  meta       jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at timestamptz NOT NULL DEFAULT now(),
  -- RFC-005 D3: per-note, dense, ascending, assigned under the note row lock.
  -- It is what makes "before" a total order without trusting clocks -- two
  -- revisions written in one transaction share created_at.
  seq        bigint NOT NULL,
  verb       text NOT NULL CHECK (verb IN
               ('write','insert_blocks','update_block','move_block','split_block',
                'merge_blocks','append_to_section','delete_note','undelete_note',
                'move_note','excise','retain')),
  UNIQUE (note_id, seq)
);

CREATE TABLE pgmind.tile (
  note_id  uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id uuid NOT NULL,
  ord      int4 NOT NULL,
  raw      text NOT NULL,
  PRIMARY KEY (note_id, ord) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE pgmind.block (
  id            uuid PRIMARY KEY,
  note_id       uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id      uuid NOT NULL,
  ord           int4 NOT NULL,
  parent_block  uuid REFERENCES pgmind.block(id),
  -- NO ACTION on parent delete: a wrong reconcile order fails loudly instead of
  -- cascading away carried children (RFC-003 D6 ordering rule)
  kind          pgmind.block_kind NOT NULL,
  heading_path  text[] NOT NULL DEFAULT '{}',
  content       text NOT NULL,
  content_hash  bytea NOT NULL CHECK (octet_length(content_hash) = 32),
  block_ref_id  text,
  tile_ord      int4 NOT NULL,
  start_in_tile int4 NOT NULL CHECK (start_in_tile >= 0),
  end_in_tile   int4 NOT NULL CHECK (end_in_tile >= start_in_tile),
  attrs         jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  CONSTRAINT block_note_ord UNIQUE (note_id, ord) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE pgmind.edge (
  id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  vault_id      uuid NOT NULL,
  src_note      uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  src_block     uuid NOT NULL REFERENCES pgmind.block(id) ON DELETE CASCADE,
  kind          pgmind.edge_kind NOT NULL,
  dst_path      text NOT NULL,
  dst_heading   text,
  dst_block_ref text,
  alias         text,
  dst_note      uuid REFERENCES pgmind.note(id),
  resolved_via  text CHECK (resolved_via IN ('exact','basename')),
  dangling_reason text CHECK (dangling_reason IN ('missing','ambiguous','invalid')),
  CHECK ((dst_note IS NULL) = (dangling_reason IS NOT NULL)),
  CHECK ((dst_note IS NULL) = (resolved_via IS NULL))
);

CREATE TABLE pgmind.tag (
  id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  vault_id uuid NOT NULL,
  note_id  uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  block_id uuid REFERENCES pgmind.block(id) ON DELETE CASCADE,
  tag      text NOT NULL
);

-- ===================================================================
-- Phase 3 history lanes (RFC-005 D2). History stores PRE-IMAGES: what the
-- two lanes held BEFORE a revision. Reconstruction starts at an anchor at or
-- above the target (current state, or a frame) and applies pre-images
-- backwards. Two invariants govern every row here:
--   X1  no history row defines its bytes by reference to another history
--       row's bytes -- every payload is literal, which is what makes erasure
--       a bounded local rewrite instead of a chain re-base;
--   X2  reconstruction never invokes the parser -- structure comes from the
--       stored vectors, so history survives an RFC-002 amendment.
-- ===================================================================

-- H1: per-revision note-level pre-image. One row per revision, always.
-- Vector lengths are read with cardinality(); array_length(x,1) returns NULL
-- on the empty array, which is the footgun this schema hands an implementer.
CREATE TABLE pgmind.note_revision (
  revision_id   uuid PRIMARY KEY REFERENCES pgmind.revision(id),
  note_id       uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  vault_id      uuid NOT NULL,
  seq           bigint NOT NULL,
  prev_path     text,                      -- NULL => unchanged (move_note's pre-image)
  prev_preamble text,
  prev_props    jsonb,
  -- KEEP/INS scripts: ops is packed int4, payload holds only what INS adds.
  tile_ops      int4[] NOT NULL DEFAULT '{}',  tile_payload text[] NOT NULL DEFAULT '{}',
  id_ops        int4[] NOT NULL DEFAULT '{}',  id_payload   uuid[] NOT NULL DEFAULT '{}',
  -- Positional facts, changed slots only: block index -> (tile_ord, start, end).
  place_idx     int4[] NOT NULL DEFAULT '{}', place_prev   int4[] NOT NULL DEFAULT '{}',
  -- heading_path is positional too (RFC-002 D6 derives it from preceding
  -- headings), so it rides here rather than producing one effect row per block
  -- in a renamed section. jsonb because SQL has no ragged text[][].
  head_idx      int4[] NOT NULL DEFAULT '{}', head_prev    jsonb NOT NULL DEFAULT '[]'::jsonb,
  UNIQUE (note_id, seq)
);

-- H2: per-block pre-image. Written ONLY when a content-visible column changed;
-- ord/tile_ord/spans/heading_path never produce a row here (they are in H1).
CREATE TABLE pgmind.block_revision (
  note_id           uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  block_id          uuid NOT NULL,          -- no FK: the block may already be gone
  seq               bigint NOT NULL,
  vault_id          uuid NOT NULL,
  existed           boolean NOT NULL,       -- false => minted here, no pre-image
  redacted          boolean NOT NULL DEFAULT false,  -- true => NULLs are erasure, not absence
  prev_kind         pgmind.block_kind,
  prev_content      text,
  prev_content_hash bytea CHECK (prev_content_hash IS NULL OR octet_length(prev_content_hash) = 32),
  prev_block_ref_id text,
  prev_attrs        jsonb,
  prev_parent_block uuid,
  confidence        real,                   -- RFC-004 Part B; NULL => deterministic
  bind              text CHECK (bind IN ('mint','ref','hash','carry','rebind','remove')),
  PRIMARY KEY (note_id, block_id, seq)
);

-- H3: periodic absolute snapshot of BOTH lanes, so deep as_of is bounded. A
-- frame that carried only bytes would anchor read_as_of and not blocks_as_of,
-- and X2 forbids recovering the difference by parsing.
CREATE TABLE pgmind.note_frame (
  note_id    uuid NOT NULL REFERENCES pgmind.note(id) ON DELETE CASCADE,
  seq        bigint NOT NULL,
  vault_id   uuid NOT NULL,
  path       text NOT NULL,
  preamble   text NOT NULL,
  props      jsonb NOT NULL,
  tiles      text[] NOT NULL,
  block_ids  uuid[] NOT NULL,
  kinds      text[] NOT NULL,
  contents   text[] NOT NULL,
  head_paths jsonb NOT NULL,
  parents    uuid[] NOT NULL,
  block_refs text[] NOT NULL,
  attrs      jsonb NOT NULL,
  placement  int4[] NOT NULL,
  PRIMARY KEY (note_id, seq)
);

-- H4a: the audit trail Law 8 owes. Content-free by construction, dumped, and
-- swept by the excision gate like any other table.
CREATE TABLE pgmind.excision_log (
  id           uuid PRIMARY KEY,
  vault_id     uuid NOT NULL,
  requested_at timestamptz NOT NULL DEFAULT now(),
  requested_by text NOT NULL DEFAULT current_user,
  reason       text NOT NULL,
  target_kind  text NOT NULL CHECK (target_kind IN ('note','block','revision','before','literal')),
  scope        jsonb NOT NULL DEFAULT '{}'::jsonb,
  escalations  jsonb NOT NULL DEFAULT '[]'::jsonb,
  verified_at  timestamptz,
  survivors    int4
);

-- H4b: the replay input. For the 'literal' and 'note' forms the target IS
-- identifying data, so this table is deliberately NOT registered for dump: it
-- must not travel in backups. enforce_excisions() reads it; the excision gate
-- names it as its one expected hit.
CREATE TABLE pgmind.excision_replay (
  excision_id uuid PRIMARY KEY REFERENCES pgmind.excision_log(id) ON DELETE CASCADE,
  target      jsonb NOT NULL
);

-- The full-text lane. One definition, used by both the index below and
-- knowledge.search(), because an expression index is only used when the query
-- repeats it EXACTLY -- and two copies of a text-search expression drift into
-- a silent sequential scan that still returns the right answer.
--
-- Bounded on purpose. to_tsvector raises 54000 once the tsvector it builds
-- passes 1048575 bytes, and that is reachable well inside what pgmind accepts:
-- a 849 KB block of distinct words produces a 1.24 MB tsvector, against a
-- pgmind.max_document_bytes that defaults to 8 MiB. Unbounded, this index would
-- make the write path REFUSE a note it stores today -- the same defect RFC-003
-- D6 froze a rule against after the jsonb ceiling silently narrowed the
-- accepted document size. Truncating instead means a very large block is
-- searchable by its first 100k characters rather than not storable at all.
--
-- 'english' is the configuration, fixed rather than settable. A generated
-- column would bake it into the heap and cost a table rewrite to change; here
-- changing it is REINDEX after redefining this function. It is also the wrong
-- stemmer for a vault that is not in English -- which is a real limitation,
-- written down rather than hidden.
CREATE FUNCTION pgmind.search_vector(content text) RETURNS tsvector
  LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT
  AS $fn$ SELECT to_tsvector('english', left($1, 100000)) $fn$;

-- Indexes (RFC-003 D4)
CREATE UNIQUE INDEX note_live_path ON pgmind.note (vault_id, path) WHERE tombstoned_at IS NULL;
CREATE INDEX note_basename    ON pgmind.note (vault_id, basename) WHERE tombstoned_at IS NULL;
CREATE INDEX note_path_prefix ON pgmind.note (vault_id, path text_pattern_ops);
CREATE INDEX block_note_hash  ON pgmind.block (note_id, content_hash);
CREATE INDEX block_note_ref   ON pgmind.block (note_id, block_ref_id) WHERE block_ref_id IS NOT NULL;
CREATE INDEX block_parent     ON pgmind.block (parent_block) WHERE parent_block IS NOT NULL;
-- Maintained by the write path like every other index (Law 7), rather than by
-- a materialized view somebody has to remember to REFRESH.
CREATE INDEX block_fts        ON pgmind.block USING gin (pgmind.search_vector(content));
CREATE INDEX edge_src         ON pgmind.edge (src_note);
CREATE INDEX edge_src_block   ON pgmind.edge (src_block);
CREATE INDEX edge_dst         ON pgmind.edge (dst_note) WHERE dst_note IS NOT NULL;
CREATE INDEX edge_path        ON pgmind.edge (vault_id, dst_path);
CREATE INDEX tag_lookup       ON pgmind.tag (vault_id, lower(tag));
CREATE INDEX tag_note         ON pgmind.tag (note_id);
-- "Which blocks in THIS note carry tag X" — an explicit product requirement,
-- and the one tag question the three indexes above could not answer without a
-- scan: tag_lookup narrows by vault and then filters every note that uses the
-- tag, tag_note narrows by note and then filters every tag it carries.
CREATE INDEX tag_note_lookup  ON pgmind.tag (note_id, lower(tag));
CREATE INDEX tag_block        ON pgmind.tag (block_id) WHERE block_id IS NOT NULL;
CREATE INDEX revision_note    ON pgmind.revision (note_id, created_at);
-- RFC-003 D4 left revision.parent unindexed and flagged it for RFC-005 by
-- name, because Phase 2 had no deletion path. Phase 3 has several.
CREATE INDEX revision_parent  ON pgmind.revision (parent) WHERE parent IS NOT NULL;
CREATE INDEX note_revision_note  ON pgmind.note_revision (note_id, seq DESC);
CREATE INDEX block_revision_block ON pgmind.block_revision (block_id, seq DESC);
CREATE INDEX note_frame_note     ON pgmind.note_frame (note_id, seq DESC);
CREATE INDEX note_revision_vault ON pgmind.note_revision (vault_id);
CREATE INDEX block_revision_vault ON pgmind.block_revision (vault_id);
CREATE INDEX note_frame_vault    ON pgmind.note_frame (vault_id);
-- vault_id is the predicate on every knowledge.stats() count and on the RLS
-- policy D1 documents, so it must be indexed on the tables that carry it —
-- without these, stats() sequentially scans the block, revision and tile heaps.
CREATE INDEX block_vault      ON pgmind.block (vault_id);
CREATE INDEX revision_vault   ON pgmind.revision (vault_id);
CREATE INDEX tile_vault       ON pgmind.tile (vault_id);

-- LZ4 TOAST where the server supports it (RFC-003 D4: recommended, not required)
DO $lz4$
BEGIN
  BEGIN
    ALTER TABLE pgmind.tile  ALTER COLUMN raw        SET COMPRESSION lz4;
    ALTER TABLE pgmind.block ALTER COLUMN content    SET COMPRESSION lz4;
    ALTER TABLE pgmind.note  ALTER COLUMN preamble   SET COMPRESSION lz4;
    ALTER TABLE pgmind.note  ALTER COLUMN properties SET COMPRESSION lz4;
    ALTER TABLE pgmind.block ALTER COLUMN attrs      SET COMPRESSION lz4;
    ALTER TABLE pgmind.revision ALTER COLUMN meta    SET COMPRESSION lz4;
    ALTER TABLE pgmind.note_revision  ALTER COLUMN tile_payload SET COMPRESSION lz4;
    ALTER TABLE pgmind.block_revision ALTER COLUMN prev_content SET COMPRESSION lz4;
    ALTER TABLE pgmind.note_frame     ALTER COLUMN tiles        SET COMPRESSION lz4;
    ALTER TABLE pgmind.note_frame     ALTER COLUMN contents     SET COMPRESSION lz4;
  EXCEPTION WHEN feature_not_supported OR invalid_parameter_value OR undefined_object THEN
    RAISE WARNING 'pgmind: lz4 toast compression unavailable on this server; using default';
  END;
END
$lz4$;

-- RFC-003 D1's tenant boundary, as shipped SQL rather than prose.
--
-- `pgmind.vault_id` is Userset by design: it SCOPES a session to a vault, it
-- does not defend one. The boundary is row-level security plus grants, and
-- until now that existed only in the RFC and in a test — a deployer who
-- granted table access and skipped the policy got a role that could read and
-- write every vault by SETting one GUC.
--
-- Deliberately not enabled by default: turning RLS on for every install would
-- change the behaviour of existing single-vault deployments. Call this once
-- per database to adopt the boundary. Idempotent.
CREATE FUNCTION pgmind.enable_vault_rls(force boolean DEFAULT false)
RETURNS TABLE (table_name text, covered boolean, detail text) LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $fn$
-- The table list is ENUMERATED FROM pg_catalog, never written out. RFC-005's
-- review found the literal list already stale the moment Phase 3 added its
-- history tables: a tenant could read another vault's block_revision.prev_content
-- and note_frame.tiles -- literal copies of note bytes, including bytes already
-- deleted from the live lanes -- while the tenant-isolation gate stayed green
-- because it enumerated the same six names. Any future table that carries a
-- vault_id is now inside the boundary by construction.
DECLARE t text;
BEGIN
  FOR t IN
    SELECT c.relname
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid
     WHERE n.nspname = 'pgmind' AND c.relkind = 'r'
       AND a.attname = 'vault_id' AND a.attnum > 0 AND NOT a.attisdropped
     -- Creation order, not alphabetical. Each ALTER takes ACCESS EXCLUSIVE and
     -- holds it to commit, so the order here is a lock order: creation order is
     -- FK-topological (note, revision, tile, block, ...), which is the order
     -- every writer touches these tables in. Alphabetical takes `block` before
     -- `note` and deadlocks against any concurrent write (observed).
     ORDER BY c.oid
  LOOP
    EXECUTE format('ALTER TABLE pgmind.%I ENABLE ROW LEVEL SECURITY', t);
    -- FORCE also subjects the table owner, which is what a deployment where
    -- the extension owner is also an application role actually needs.
    IF force THEN
      EXECUTE format('ALTER TABLE pgmind.%I FORCE ROW LEVEL SECURITY', t);
    END IF;
    EXECUTE format('DROP POLICY IF EXISTS vault_isolation ON pgmind.%I', t);
    -- NULLIF, because a custom GUC does not come back as NULL.
    --
    -- After `SET LOCAL pgmind.vault_id = …; COMMIT` the setting holds the
    -- EMPTY STRING, not NULL, and never returns to NULL for the life of the
    -- backend (measured). Without NULLIF the predicate casts '' to uuid and
    -- every later statement on that connection fails with 22P02 — so the
    -- pooled-connection pattern, which is the shipped way to scope a request,
    -- poisoned the connection it was meant to protect on its second
    -- transaction. NULL makes the predicate NULL, which denies rather than
    -- errors: an unset vault sees nothing instead of breaking the session.
    EXECUTE format(
      'CREATE POLICY vault_isolation ON pgmind.%I USING '
      '(vault_id = NULLIF(current_setting(''pgmind.vault_id'', true), '''')::uuid)', t);
    table_name := t; covered := true; detail := NULL; RETURN NEXT;
  END LOOP;

  -- Report what the enumeration could NOT cover, rather than skipping it in
  -- silence. A pgmind table without vault_id is outside the boundary by
  -- construction -- pgmind.excision_replay is one today, and it holds the
  -- erased identifying data -- and a deployer who reads only the success of
  -- this call would never learn that.
  FOR t IN
    SELECT c.relname
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'pgmind' AND c.relkind = 'r'
       AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
                        WHERE a.attrelid = c.oid AND a.attname = 'vault_id'
                          AND a.attnum > 0 AND NOT a.attisdropped)
     ORDER BY c.oid
  LOOP
    table_name := t; covered := false;
    detail := 'no vault_id column: this table is outside the vault boundary';
    RETURN NEXT;
  END LOOP;
END
$fn$;
REVOKE ALL ON FUNCTION pgmind.enable_vault_rls(boolean) FROM PUBLIC;

-- Backups (RFC-003 D3): extension-script tables are skipped by pg_dump unless
-- registered. Registration order is normative (FK-topological; pg_dump emits
-- COPY in this order, which restores under plain autocommit psql).
-- The registry first: FK-topological, and `note.vault_id` references it.
-- The WHERE clause excludes the seeded default vault, which CREATE EXTENSION
-- inserts on the restore side before this table's data is replayed — dumping
-- it would restore into a duplicate key.
SELECT pg_catalog.pg_extension_config_dump('pgmind.vault',
       'WHERE id <> ''00000000-0000-0000-0000-000000000000''');
SELECT pg_catalog.pg_extension_config_dump('pgmind.note',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.revision', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tile',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.block',    '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag',      '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag_id_seq',  '');
-- Phase 3 history lanes. Missing ONE of these loses 100% of that lane from
-- every backup while the Phase 2 dump-restore gate stays green, which is why
-- the history-durability gate asserts this registration set against pg_catalog
-- rather than against a list someone has to remember to update.
SELECT pg_catalog.pg_extension_config_dump('pgmind.note_revision',  '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.block_revision', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.note_frame',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.excision_log',   '');
-- pgmind.excision_replay is deliberately absent: it holds the executable
-- excision target, which for the 'literal' and 'note' forms IS the identifying
-- data the excision erased. It must not travel in a dump (RFC-005 D2 H4b).
"#,
    name = "pgmind_storage",
    requires = [pgmind::path_is_valid, pgmind::path_normalize]
);

// Law 11 / RFC-005 D7: the admin surface is not `PUBLIC`.
//
// This was decided, written into RFC-005 D7 and restated in PRODUCT-PLAN §7.1
// ("revoked from PUBLIC") — and never implemented. Until now the only two
// REVOKEs in the extension were `raise_error` and `enable_vault_rls`, so any
// role that could reach the database could erase content, bound history, and
// probe for the existence of a string across the vault. `excise` in
// particular defaults `dry_run => true` and reports its hit count in a
// WARNING, which made it a free existence oracle.
//
// Grant these deliberately, to the role that owns erasure requests.
extension_sql!(
    r#"
REVOKE ALL ON FUNCTION pgmind.excise(jsonb, text, boolean, boolean) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgmind.verify_excision(uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgmind.retain(bigint, boolean) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgmind.verify_note(uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgmind.verify_history(uuid) FROM PUBLIC;
"#,
    name = "pgmind_admin_grants",
    // `finalize` rather than `requires`: these functions are private items
    // inside `#[pg_schema] mod pgmind` in excision.rs and verify.rs, so there
    // is no path from here to name them as dependencies. Emitting last is
    // exactly the ordering a REVOKE needs anyway.
    finalize
);
