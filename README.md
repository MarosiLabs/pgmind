# pgmind

**A brain for AI agents, inside PostgreSQL.**

pgmind is a PostgreSQL extension that stores an Obsidian-shaped markdown vault — notes,
wiki-links, tags, backlinks, block-level history — as relational rows instead of files.
Many agents can write to it concurrently without clobbering each other, every paragraph
carries a stable ID an agent can cite, and the whole vault is queryable in SQL.

**No AI is anywhere in the middle.** pgmind never calls a model and opens no network
sockets. Embeddings, if you want them, are a lane you fill yourself with pgvector.

> ### 🔬 Research pre-alpha
> Phases 0–3 have exited with their gates published; Phase 4 was cut; Phase 5 is in
> progress and ends at the first public release, 0.1.0. Every example below runs today
> except the one marked Phase 5. The SQL surface is being frozen for 0.x right now
> ([RFC-007](docs/rfcs/RFC-007-query-api-and-mcp-surface.md)) and there is no upgrade path
> between versions yet. Build on it to experiment, not to serve traffic.

---

## The pain

The most widely deployed agent memory in the world is a folder of markdown: memory
files, an Obsidian vault, a `docs/` tree piped into a prompt. It won for good reasons —
models read and write it natively, humans can still edit it, and there is no schema to
argue about.

On a laptop that is the whole story: one user, one writer, one disk. Then the agent moves
into a backend, and the same folder starts to break:

- **Two requests write the same note.** One wins, silently. Nobody gets an error.
- **"What links here?"** is a grep across the whole tree, on every call.
- **History** means shelling out to `git` inside a request handler.
- **Every tenant needs its own directory** — and every read has to prove it stayed there.
- **Notes on disk, vectors in a service.** No transaction spans the two, so they drift.
- **Backup and migration** become a second system beside the database you already run.

None of that is markdown's fault. It is the filesystem's.

## The approach

Keep the markdown. Replace the filesystem.

- **Lost writes are refusable.** Pass the revision you read as `expected_head` and a stale
  write fails loudly instead of overwriting someone. Omit it and you get last-writer-wins —
  explicitly, and by your choice. Most agent writes need no guard at all: appending to a
  section and editing one block are separate operations that do not conflict.
- **Everything is a row.** Every heading, paragraph, list item, link and tag is queryable.
  An outline or a backlink lookup is one query, not a directory walk.
- **Nothing is overwritten.** Each edit appends a revision. Read a note as it was at any
  point, diff two revisions, blame a paragraph.
- **Blocks keep their identity.** A paragraph's UUID survives edits to the note around it
  — including whole-document rewrites, where a confidence-scored rebinder carries IDs
  across and marks what it had to infer.
- **Many vaults, one database.** A vault is a registered object with a name and an id;
  every row carries its `vault_id`. Scope a session with one setting, or name the vault
  per call. Row security enforces the scope against a bug in your application — not
  against anything that can run its own SQL.
- **Erasure is a real operation.** History is append-only *by default*, not forever.
  Excision is explicit, audited, scoped to one vault, and verified inside the transaction
  that performs it.
- **It is just your database.** Notes, indexes and any embeddings you add live in
  Postgres, inside the backup you already take.

---

## Usage

The public API is one schema — `knowledge` — over the nouns a vault already has.
Administrative operations live in `pgmind`. Markdown goes in; the same bytes come out.

### Install and write

```sql
CREATE EXTENSION pgmind;

-- A vault has a name and an id. Pass the id if your application already has one.
SELECT * FROM knowledge.create_vault('acme/alice/memory', 'Alice''s agent memory');
SET pgmind.vault_id = '019feaf0-…';

-- Returns the new revision id. A byte-identical rewrite is a no-op.
SELECT knowledge.write('projects/auth', $md$
# Auth

## Decisions

- [[oauth2]] over server sessions, see [[rfc/auth-01]] #architecture
- Refresh tokens rotate every 24h #decision

## Log

Migrated the session store.
$md$);

-- A whole folder in one call: two parallel arrays, one row back per note,
-- in input order. All-or-nothing, capped at 1000.
SELECT * FROM knowledge.write_many(ARRAY['guides/tone', 'guides/scope'],
                                   ARRAY[$md$# Tone$md$, $md$# Scope$md$]::markdown[]);
```

### Concurrency: two agents, one note

Pass the revision you read as `expected_head` and a stale write is rejected rather than
silently applied:

```sql
SELECT knowledge.write('projects/auth', $md$...$md$, expected_head => $1);

-- ERROR:  pgmind: expected_head is not the note's current head
-- DETAIL: pgmind_stale_head — note projects/auth: expected 0199…, head is 019f…
```

Better still, don't take the whole note. Section-scoped and block-scoped operations let
two agents edit the same note at the same time without conflicting at all:

```sql
-- Two concurrent appends to the same section both survive, in lock order.
SELECT * FROM knowledge.append_to_section(
    'projects/auth', ARRAY['Auth','Log'], $md$Rotated the signing key.$md$);
--             revision               |               block_ids
-- 019f…-2e63ea313444                 | {019f…-8f1e7a620df1}

-- Or address a single paragraph by id.
SELECT * FROM knowledge.update_block($1, $md$- Refresh tokens rotate every 12h #decision$md$);
```

`insert_blocks`, `move_block`, `split_block` and `merge_blocks` round out the set, each
with defined identity semantics and each accepting `expected_head`.

> **They return `SETOF pgmind.op_result`** — one row, `(revision, block_ids)`. `SELECT *
> FROM knowledge.update_block(…)` reads best, and `SELECT (knowledge.update_block(…)).*`
> is safe: a set-returning expression is evaluated once. While these returned a *scalar*
> composite, that second form applied the edit twice.

### Navigate

```sql
-- Table of contents: headings are already rows, so nothing is re-parsed.
SELECT ord, attrs->>'level' AS level, content
FROM   knowledge.blocks('projects/auth')
WHERE  kind = 'heading';

-- Read back one section instead of the whole note.
SELECT knowledge.read_section('projects/auth', ARRAY['Auth','Decisions']);

-- Who points here, and from which paragraph?
SELECT src_path, kind, excerpt FROM knowledge.backlinks('projects/auth');
--   src_path   |   kind   |                 excerpt
-- rfc/auth-01  | wikilink | Rotation policy for [[projects/auth]]. #arch…

-- Outgoing links, including the ones that resolve to nothing.
SELECT kind, target, resolved_path, dangling_reason FROM knowledge.links('projects/auth');
--   kind   |   target    | resolved_path | dangling_reason
-- wikilink | oauth2      |               | missing
-- wikilink | rfc/auth-01 | rfc/auth-01   |

SELECT * FROM knowledge.tagged('architecture');   -- everything carrying a tag
SELECT * FROM knowledge.tags();                   -- tag → note/block counts
SELECT * FROM knowledge.orphans();                -- nothing links here
SELECT * FROM knowledge.notes('projects/**');     -- id, path, title, description, head
SELECT * FROM knowledge.stats();                  -- notes, blocks, edges, revisions, bytes
```

### Search

Full-text search over blocks, on an index the write path maintains. No refresh, no
embedding, no second service:

```sql
SELECT path, rank, excerpt FROM knowledge.search('rotate keys');
--         path         |  rank  |                     excerpt
-- projects/auth        | 0.0909 | **Key** **rotation** is [[runbooks/rotate-keys|…
-- runbooks/rotate-keys | 0.0323 | **Rotate** the signing **keys**

-- Narrow to one note, or to blocks carrying every tag listed. With no text query at
-- all it is a pure tag intersection, and `rank` comes back NULL rather than a made-up 0.
SELECT * FROM knowledge.search('rotation', path => 'projects/auth');
SELECT * FROM knowledge.search('', tags => ARRAY['architecture','decision']);
```

Typo-tolerant search is deliberately absent: it needs a `pg_trgm` dependency that has not
been taken. Ranking is `ts_rank_cd`, and the text-search configuration is `english`,
fixed — the wrong stemmer for a vault that is not in English.

### History and time travel

```sql
-- Every revision, newest first.
SELECT seq, verb, author, source, blocks_changed FROM knowledge.history('projects/auth');
-- seq |     verb      | author | source | blocks_changed
--   1 | insert_blocks | amin   | api    |              1
--   0 | write         | amin   | api    |              8

-- What changed between two revisions, paragraph by paragraph.
SELECT change, before, after FROM knowledge.diff('projects/auth', $1, $2);
-- change | before |          after
-- added  |        | Rotated the signing key.

-- The note as it was — by revision id, by sequence number, or by wall-clock time.
-- (The time form raises if the note has no revision that old.)
SELECT knowledge.read_as_of('projects/auth', 0::bigint);
SELECT knowledge.read_as_of('projects/auth', now() - interval '7 days');
SELECT * FROM knowledge.blocks_as_of('projects/auth', $1);
```

### Identity survives a rewrite

When a whole note is replaced — an agent regenerating it, a folder sync — pgmind rebinds
the old block IDs onto the new text and records how sure it was. Deterministic carries
record no confidence at all; inferred ones are marked, so an audit can tell them apart:

```sql
SELECT b.ord, k.content, b.confidence
FROM   knowledge.blame('projects/auth') b
JOIN   knowledge.blocks('projects/auth') k USING (block_id)
ORDER  BY b.ord;
-- ord |               content                | confidence
--   4 | Refresh tokens rotate every 6h, per… |  0.5714286
--   7 | Migrated the session store.…         |  0.6363636
```

The measured match rate on the adversarial edit corpus is published in
[`eval/published/`](eval/published/) — including the first run, which carried **0 of 22**.

### Many vaults

pgmind has no tenant concept. Applications have tenants, users and agents; a vault is the
container, and you put your hierarchy in its name:

```sql
SELECT * FROM knowledge.vaults('acme/**');   -- id, name, description, created_at

-- Scope the session, or name the vault per call — the last argument of every
-- vault-scoped function, which is what lets one query span several vaults.
SET pgmind.vault_id = '019feaf0-…';
SELECT count(*) FROM knowledge.notes(vault => 'acme/alice/memory');

-- Enforce the scope with row security. Returns one row per table with what it covered.
SELECT * FROM pgmind.enable_vault_rls(force => true);
```

An id nobody registered is an error (`PM018`), not a new empty vault — the failure that
used to turn a typo into a vault you could never find again. Note the honest limit: the
registry itself has no `vault_id` to police, so `knowledge.vaults()` lists every vault's
*name* regardless of the policy. Content is scoped; names are not.

### Erasure

Append-only is the default, not a promise you can't keep. Excision is a deliberate,
audited operation that refuses to leave the live copy behind and verifies itself before
committing:

```sql
-- dry_run defaults to true: erasure is never the outcome of a typo.
SELECT pgmind.excise('{"literal":"Refresh tokens rotate every 24h"}'::jsonb,
                     'erasure request #42');
-- WARNING: pgmind: dry run — would erase 0 live and touch 4 history surface(s)

SELECT pgmind.excise('{"literal":"…"}'::jsonb, 'erasure request #42', dry_run => false);
SELECT * FROM pgmind.verify_excision($1);   -- empty result ⇒ proven erased from this vault

SELECT pgmind.retain(keep_revisions => 50, dry_run => false);   -- bound history

-- Invariant checkers, by note id. Empty result ⇒ healthy.
SELECT v.* FROM pgmind.note n, LATERAL pgmind.verify_note(n.id) v;
```

### Not yet: `context()`

The headline call — deterministic, token-budgeted context assembly with a citation on
every block — lands in Phase 5 with the first public release:

```sql
-- Phase 5 · 0.1.0 · not implemented yet
SELECT knowledge.context(root => 'projects/auth', token_budget => 12000);
```

---

## Status

Research pre-alpha. A phase exits only when its benchmark gate passes and the numbers are
published — including the unflattering ones.

| Phase | Scope | State |
|---|---|---|
| 0 | Groundwork | ✅ exited |
| 1 | Markdown type & parser — CommonMark + GFM, wiki-links, block refs | ✅ exited |
| 2 | The vault model — notes, blocks, edges, tags, backlinks | ✅ exited |
| 3 | Versioning & concurrency — revisions, diff, blame, CAS, rebinding, excision | ✅ exited |
| 4 | ~~Sync bridge~~ — **cut 2026-08-09** ([why](docs/PGMIND.md#11-risks--open-questions)); export/import ship as [scripts/](scripts/) | ✂️ |
| 5 | Query API, MCP server, `knowledge.context()` — **the first public release, 0.1.0** | ▶ in progress |
| 6 | Optional vector lane (pgvector hooks) | — |
| 7 | Retrieval planner & context maturation | — |

Search and the vault registry have landed;
[RFC-007](docs/rfcs/RFC-007-query-api-and-mcp-surface.md) is the draft that freezes the SQL
surface for 0.x and designs the rest. One artifact beyond the extension is planned but not
built: `pgmind-mcp`, the vault as eight MCP tools. Moving a vault in or out needs no CLI — [`scripts/export-vault.sh`](scripts/export-vault.sh)
and [`scripts/import-vault.sh`](scripts/import-vault.sh) do it, and `make eval`'s
`folder-round-trip` suite proves the bytes survive paths chosen to break them.

## Build, run, test

Requirements: Rust (pinned by `rust-toolchain.toml`; rustup installs it automatically) and
Python 3.10+ for the eval harness. On macOS also `brew install pkgconf icu4c`, needed to
compile the pgrx-managed Postgres.

```bash
make setup   # install cargo-pgrx (pinned) and init a pgrx-managed Postgres
             # (compiles PG into ~/.pgrx — self-contained and writable; system installs
             #  often aren't: libpq's pg_config is client-only, and macOS blocks writing
             #  extensions into Postgres.app's protected bundle)
make build   # build the extension          (PG=18 by default; make build PG=16)
make test    # core unit tests + cargo pgrx test — runs inside a real Postgres
make lint    # fmt + clippy
make eval    # run the benchmark gates → eval/results/latest.json
```

PostgreSQL 16, 17 and 18 are supported. To try it interactively, `cargo pgrx run` builds
the extension, installs it into the managed cluster and drops you at a `psql` prompt:

```bash
cd extension
cargo pgrx run pg18 --no-default-features --features pg18
```

```sql
CREATE EXTENSION pgmind;
SELECT knowledge.write('hello', $md$# Hello

A [[world]] #greeting
$md$);
SELECT * FROM knowledge.blocks('hello');
```

## Documentation

| Document | Role |
|---|---|
| [docs/PGMIND.md](docs/PGMIND.md) | The handbook — vision, philosophy, architecture laws (the constitution) |
| [docs/PRODUCT-PLAN.md](docs/PRODUCT-PLAN.md) | The operating blueprint — system design + phased delivery plan |
| [docs/rfcs/](docs/rfcs/README.md) | Per-phase RFCs, written and accepted before implementation |
| [website/docs/](https://marosilabs.github.io/pgmind/docs/) | The user manual — quickstart, concepts, SQL reference, cookbook, internals |
| [eval/](eval/README.md) | The benchmark gates, corpora, and published results |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Roles, governance, RFC lifecycle |
| [docs/archive/](docs/archive/) | The original v0.1 handbook and the evidence audit it was rewritten from |

Precedence when they disagree: handbook laws > accepted RFCs > product plan > code.

## License

[PostgreSQL License](LICENSE).
