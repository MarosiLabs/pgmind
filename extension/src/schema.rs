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
CREATE FUNCTION pgmind.raise_error(code text, message text, detail text)
RETURNS void LANGUAGE plpgsql AS $fn$
BEGIN
  RAISE EXCEPTION USING ERRCODE = code, MESSAGE = message, DETAIL = detail;
END
$fn$;

CREATE TYPE pgmind.block_kind AS ENUM
  ('heading','paragraph','list_item','code_block','table','thematic_break','html_block');
CREATE TYPE pgmind.edge_kind AS ENUM ('wikilink','transclusion','blockref','mdlink');
CREATE TYPE pgmind.op_result AS (revision uuid, block_ids uuid[]);

CREATE TABLE pgmind.note (
  id            uuid PRIMARY KEY,
  vault_id      uuid NOT NULL DEFAULT '00000000-0000-0000-0000-000000000000',
  path          text NOT NULL CHECK (pgmind.path_is_valid(path)),
  basename      text GENERATED ALWAYS AS (regexp_replace(path, '^.*/', '')) STORED,
  properties    jsonb NOT NULL DEFAULT '{}'::jsonb,
  preamble      text NOT NULL DEFAULT '',
  head_revision uuid NOT NULL,
  -- deliberately no FK on head_revision: a note<->revision circular FK makes pg_dump
  -- warn and plain-psql restore impossible (RFC-003 D3); verify_note polices it
  created_at    timestamptz NOT NULL DEFAULT now(),
  tombstoned_at timestamptz
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
  created_at timestamptz NOT NULL DEFAULT now()
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

-- Indexes (RFC-003 D4)
CREATE UNIQUE INDEX note_live_path ON pgmind.note (vault_id, path) WHERE tombstoned_at IS NULL;
CREATE INDEX note_basename    ON pgmind.note (vault_id, basename) WHERE tombstoned_at IS NULL;
CREATE INDEX note_path_prefix ON pgmind.note (vault_id, path text_pattern_ops);
CREATE INDEX block_note_hash  ON pgmind.block (note_id, content_hash);
CREATE INDEX block_note_ref   ON pgmind.block (note_id, block_ref_id) WHERE block_ref_id IS NOT NULL;
CREATE INDEX block_parent     ON pgmind.block (parent_block) WHERE parent_block IS NOT NULL;
CREATE INDEX edge_src         ON pgmind.edge (src_note);
CREATE INDEX edge_src_block   ON pgmind.edge (src_block);
CREATE INDEX edge_dst         ON pgmind.edge (dst_note) WHERE dst_note IS NOT NULL;
CREATE INDEX edge_path        ON pgmind.edge (vault_id, dst_path);
CREATE INDEX tag_lookup       ON pgmind.tag (vault_id, lower(tag));
CREATE INDEX tag_note         ON pgmind.tag (note_id);
CREATE INDEX tag_block        ON pgmind.tag (block_id) WHERE block_id IS NOT NULL;
CREATE INDEX revision_note    ON pgmind.revision (note_id, created_at);

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
  EXCEPTION WHEN feature_not_supported OR invalid_parameter_value OR undefined_object THEN
    RAISE WARNING 'pgmind: lz4 toast compression unavailable on this server; using default';
  END;
END
$lz4$;

-- Backups (RFC-003 D3): extension-script tables are skipped by pg_dump unless
-- registered. Registration order is normative (FK-topological; pg_dump emits
-- COPY in this order, which restores under plain autocommit psql).
SELECT pg_catalog.pg_extension_config_dump('pgmind.note',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.revision', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tile',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.block',    '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge',     '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag',      '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.edge_id_seq', '');
SELECT pg_catalog.pg_extension_config_dump('pgmind.tag_id_seq',  '');
"#,
    name = "pgmind_storage",
    requires = [pgmind::path_is_valid, pgmind::path_normalize]
);
