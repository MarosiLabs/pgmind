# Plan: the pgmind user manual on the website

**Status:** working plan — followed by the implementation (and by any subworkflow agent that
picks up a piece of it).
**Created:** 2026-08-06
**Deliverable:** a complete user manual published as part of `website/`, covering quick start,
concepts, internals, a full SQL reference, and a large cookbook.

Precedence for everything below: **the installed extension's catalog is the truth.** Where an
RFC, the README, the landing page or this plan disagrees with `pg_proc`, the catalog wins and
the document is wrong. Nothing gets documented that `\df knowledge.*` cannot show.

---

## 1. What we are building, and why this shape

The landing page sells the idea and stops. Two `<!-- TODO: repoint to the standalone
getting-started page once it exists -->` comments in `website/index.html` are literally waiting
for this. Today a reader who is convinced has nowhere to go except the README and 1 700 lines
of RFC.

The manual has four distinct jobs and four distinct audiences, so it is four documents, not one:

| Page | Reader | Question it answers |
|---|---|---|
| Quick start | evaluating, 10 minutes in | "What is this for, and can I run it right now?" |
| Concepts | building against it | "What are the nouns, and what exactly does the markdown dialect mean?" |
| Internals | deciding whether to trust it | "How does it actually work, and where does it break?" |
| SQL reference | writing a query at 2am | "What is the exact signature and what can it raise?" |
| Cookbook | has a specific problem | "How do I do *this*?" |

Five pages plus a manual home. No build step: the site is a plain static directory uploaded by
`.github/workflows/pages.yml` (`path: './website'`). Every page is hand-written HTML sharing one
stylesheet and one script.

### Non-goals

- No framework, no bundler, no npm. The site has none and will not acquire one for docs.
- No client-side full-text search across pages in v1 (a filter box on the SQL reference, yes —
  see §4.5).
- No documentation of anything unimplemented **as though it worked**. Phase 4/5 surfaces appear
  only in clearly badged "not yet" blocks.

---

## 2. Ground truth (read this before writing a line)

### 2.1 The authoritative API inventory

Captured from the live extension. Regenerate with:

```bash
/Users/amin/.pgrx/18.4/pgrx-install/bin/psql -h localhost -p 28818 -d manual -At -c "
SELECT n.nspname || '.' || p.proname || '(' || pg_get_function_arguments(p.oid) || ') -> '
       || pg_get_function_result(p.oid)
FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace
WHERE n.nspname IN ('knowledge','pgmind') OR p.proname='pgmind_version'
ORDER BY n.nspname, p.proname, p.oid;"
```

The 43 functions, verbatim:

**`knowledge` — public API**

```
append_to_section(path text, heading_path text[], fragment markdown, expected_head uuid DEFAULT NULL) -> pgmind.op_result
backlinks(path text) -> TABLE(src_path text, block_id uuid, kind text, anchor text, excerpt text)
blame(path text) -> TABLE(block_id uuid, ord integer, first_revision uuid, last_changed_revision uuid,
                          author text, source text, confidence real, changed_at timestamptz, history_floor bigint)
blocks(path text) -> TABLE(block_id uuid, ord integer, kind text, parent_block uuid, heading_path text[],
                           content text, content_hash bytea, block_ref_id text, span_start bigint,
                           span_end bigint, attrs jsonb)
blocks(doc markdown) -> TABLE(ord integer, kind text, parent integer, heading_path text[], content text,
                              content_hash bytea, span_start bigint, span_end bigint, attrs jsonb)
blocks_as_of(path text, at bigint) -> TABLE(ord integer, block_id uuid, kind text, content text, heading_path text[])
blocks_as_of(path text, at uuid)   -> TABLE(ord integer, block_id uuid, kind text, content text, heading_path text[])
delete_note(path text, expected_head uuid DEFAULT NULL) -> uuid
diff(path text, from_at uuid, to_at uuid) -> TABLE(block_id uuid, change text, before text, after text)
history(path text, limit_n bigint DEFAULT 50) -> TABLE(revision uuid, seq bigint, verb text, author text,
                          source text, message text, created_at timestamptz, blocks_changed bigint,
                          reconstructable boolean)
insert_blocks(path text, fragment markdown, before uuid DEFAULT NULL, after uuid DEFAULT NULL,
              expected_head uuid DEFAULT NULL) -> pgmind.op_result
links(path text) -> TABLE(block_id uuid, kind text, target text, anchor text, alias text,
                          resolved_path text, dangling_reason text)
links(doc markdown) -> TABLE(kind text, target text, anchor text, alias text, block integer)
merge_blocks(block_ids uuid[], fragment markdown, keep uuid DEFAULT NULL, expected_head uuid DEFAULT NULL) -> pgmind.op_result
move_block(block_id uuid, before uuid DEFAULT NULL, after uuid DEFAULT NULL, expected_head uuid DEFAULT NULL) -> pgmind.op_result
move_note(path text, new_path text, expected_head uuid DEFAULT NULL) -> uuid
notes(glob text DEFAULT '**') -> TABLE(path text, title text, properties jsonb, head_revision uuid,
                          created_at timestamptz, updated_at timestamptz)
orphans() -> TABLE(path text)
preamble(doc markdown) -> text
properties(doc markdown) -> jsonb
read(path text) -> markdown
read_as_of(path text, at uuid) -> markdown
read_as_of(path text, at bigint) -> markdown
read_as_of(path text, at timestamptz) -> markdown
read_section(path text, heading_path text[]) -> markdown
split_block(block_id uuid, fragment markdown, expected_head uuid DEFAULT NULL) -> pgmind.op_result
stats() -> TABLE(vault_id uuid, notes bigint, blocks bigint, edges_resolved bigint, edges_dangling bigint,
                 tags bigint, revisions bigint, bytes bigint)
tagged(tag text) -> TABLE(path text, block_id uuid, tag text)
tags() -> TABLE(tag text, notes bigint, blocks bigint)
tags(doc markdown) -> TABLE(tag text, block integer)
undelete_note(path text) -> uuid
update_block(block_id uuid, fragment markdown, expected_head uuid DEFAULT NULL, expected_hash bytea DEFAULT NULL) -> pgmind.op_result
write(path text, doc markdown, expected_head uuid DEFAULT NULL) -> uuid
```

**`pgmind` — admin surface (Law 11)**

```
enable_vault_rls(force boolean DEFAULT false) -> void
excise(target jsonb, reason text, and_head boolean DEFAULT false, dry_run boolean DEFAULT true) -> uuid
path_is_valid(path text) -> boolean
path_normalize(path text) -> text
raise_error(code text, message text, detail text) -> void        -- REVOKEd from PUBLIC, internal
retain(keep_revisions bigint DEFAULT NULL, dry_run boolean DEFAULT true) -> bigint
verify_excision(excision uuid) -> SETOF text
verify_history(note_id uuid) -> SETOF text
verify_note(note_id uuid) -> SETOF text
```

**`public`**: `pgmind_version() -> text`

**Types:** `markdown` (boundary type), `pgmind.block_kind` enum
(`heading|paragraph|list_item|code_block|table|thematic_break|html_block`), `pgmind.edge_kind`
enum (`wikilink|transclusion|blockref|mdlink`), `pgmind.op_result` composite
(`revision uuid, block_ids uuid[]`).

**GUCs:** `pgmind.max_document_bytes` (int, default 8 MiB, min 1024, USERSET),
`pgmind.frame_every` (int, default 50, range 1–1 000 000, USERSET), `pgmind.vault_id` (string,
default `00000000-0000-0000-0000-000000000000`, USERSET), `pgmind.author` (string, default
unset, USERSET — see §2.5).

**Errors:** PM001–PM017, from `extension/src/errors.rs`.

### 2.5 AMENDED 2026-08-06 — RFC-011 provenance landed mid-authoring

Five commits (`36910ca`…`035b658`) landed on `main` **after** the inventory in §2.1 was
captured. Slice 9 (block-granular CAS) was already in the working tree and is already reflected
above. Slice 10 — **RFC-011 provenance** — is not. It adds **no new SQL functions**; it adds:

- **`pgmind.author` (GUC, USERSET, default unset).** The writer's own claim about who it is,
  recorded as `revision.author`. Verified in `extension/src/write.rs`:
  - unset **or empty or whitespace-only** ⇒ the column falls back to `current_user`;
  - the limit is **200 characters, counted in chars not bytes** (`AUTHOR_MAX_CHARS`);
  - over the limit raises **PM017** *before* the revision row is inserted, so an over-long
    author writes nothing at all;
  - it is **never authenticated**. Any session that can write can claim any author. The manual
    must say so plainly wherever it appears — that is the RFC's own framing, not a caveat we
    are adding.
- **PM017 `pgmind_invalid_author`.** Extends the error table to PM001–PM017.
- **`revision.meta` no longer carries `minted` / `removed`.** RFC-005 D11 declared them retired
  in favour of typed `block_revision` rows (`bind = 'mint'` / `'remove'`); the deletion has now
  shipped. `meta` keeps `op`, `carried` and the `rebind` / `split` / `merge` objects. **Any page
  documenting `meta.minted` or `meta.removed` is wrong.**

Consequence for authors and verifiers: the extension binary installed at the start of this
session predates slice 10, so `SHOW pgmind.author` in that build only appeared to work via
PostgreSQL's placeholder handling for unknown `foo.bar` settings. **Rebuild and reinstall before
verifying anything about `pgmind.author` or PM017:**

```bash
cd extension && SDKROOT=$(xcrun --show-sdk-path) \
  cargo pgrx install --pg-config ~/.pgrx/18.4/pgrx-install/bin/pg_config \
                     --no-default-features --features pg18
```

### 2.2 Gaps between the RFCs and the code — document the CODE

These are the traps. An agent reading RFC-005 and writing a reference page will invent
functions. Every item below is **specified but not shipped** and may appear only inside a
badged "not yet" note:

| Claimed in | Not in the catalog |
|---|---|
| RFC-005 D7 | `pgmind.enforce_excisions()` |
| RFC-005 D8 | `retain(keep_since, keep_sources, vault)` — shipped signature is `(keep_revisions, dry_run)` and returns `bigint`, not a TABLE |
| RFC-005 D7 | `excise` targets `{"note_id":…}`, `{"revision":…}`, `{"path":…,"before":…}` — shipped forms are `literal`, `path`, `block_id` **in that precedence order** |
| RFC-005 D3 | `blocks_as_of(path, timestamptz)` — only `uuid` and `bigint` overloads exist |
| RFC-005 D3 | `diff` by seq or timestamp — only `(uuid, uuid)` |
| Handbook §6.1, landing page | `knowledge.context()`, `search()`, `traverse()`, `expand()`, `context_explain()` — Phase 5 |
| Handbook §8, landing page | the `pgmind` CLI (`import`/`export`/`sync`) and `pgmind-mcp` — Phases 4 and 5 |
| RFC-005 D7 | `excise`/`verify_excision` as `SECURITY DEFINER` — they are not, today |
| RFC-004 A2 | `knowledge.move` for notes is named `move_note` |
| Product plan §7 | `patch_block` — folded into `update_block`'s `expected_hash` (RFC-005 D11) |

Two shipped-but-undocumented-in-RFC extras that **do** exist and must be documented:
`history().reconstructable` (bool) and `blame().history_floor` (bigint).

### 2.3 Behaviours that must be stated because they surprise people

Each of these is load-bearing and appears in the code or the tests. Every one gets a home in
the manual (mostly Concepts §"Sharp edges" and Cookbook §8):

1. `SELECT (knowledge.update_block(…)).*` evaluates the function **once per output column** and
   applies the edit twice. Call composite-returning ops from `FROM`. (README already warns.)
2. `knowledge.write` with byte-identical input returns the current head and writes **no**
   revision — CAS is still checked first (`write.rs:898`).
3. `expected_head` on a path with **no live note** raises PM009, never a silent create.
4. `append_to_section` walks back to the last *anchorable* row, so a section ending in a list
   works; a section ending in a bare blockquote raises PM005, not PM007.
5. Two concurrent `append_to_section` calls both survive, in lock order. That is the only
   operation for which a conflict is not a conflict.
6. `update_block`'s `expected_hash` removes the *false conflict*, not the serialization.
7. A wikilink is not recognised if a link-reference definition (`[foo]: url`) exists elsewhere
   in the document — CommonMark inline parsing wins (RFC-002 D3).
8. `[[a|b]]` inside a GFM table cell splits the cell; write `[[a\|b]]`.
9. `**bold**#tag` is not a tag — the `#` must be preceded by whitespace or block start **in the
   source**.
10. `code_block`, `table`, `html_block` and `thematic_break` cannot carry a `^id` marker.
11. `heading_path` is **not unique**; `read_section` and `append_to_section` take the first
    match in document order.
12. Paths are NFC-normalised and case-**sensitive**; `path_is_valid` does not normalise.
13. `notes()` globs are git-style: `*` inside a segment, `**` across segments, nothing else.
14. `tags()` groups case-insensitively and reports the lexicographically-first spelling.
15. `orphans()` ignores self-links and dangling edges.
16. Editing a paragraph and rewriting the whole note runs the **rebinder** — the ID may carry
    with a confidence score, or may mint. Deterministic carry is only exact-hash or `^id`.
17. `pgmind.vault_id` **scopes**; it does not **defend**. The boundary is
    `pgmind.enable_vault_rls()` plus grants.
18. `excise` refuses (PM012) while the target is live at head unless `and_head => true`, and
    `dry_run` defaults to **true**.
19. `retain` raises the history floor; reads below it raise PM011 (not PM010 — they mean
    opposite things).
20. `pgmind.excision_replay` is deliberately **not** in the dump. Restoring an old backup
    resurrects erased content and there is no shipped `enforce_excisions()` to replay it yet.
21. The extension is `superuser = true`, `trusted = false`, `relocatable = false`.
22. `lz4` TOAST compression emits a `WARNING` at `CREATE EXTENSION` on servers without it. Not
    an error.
23. `revision.author` defaults to `current_user`; `source` is `'api'` for every shipped
    operation (`'sync'`/`'rebind'` are reserved).
24. Block ops other than `update_block` take `expected_head` only — no `expected_hash`.
25. `insert_blocks` with neither `before` nor `after` appends after the last top-level block;
    giving **both** is PM005.

### 2.4 Verified against the live extension on 2026-08-06 (PG 18.4, pgmind 0.0.1)

Everything here was executed, not inferred. These are the facts the pages quote.

- **The GUCs do not exist until the library loads.** In a fresh session
  `SHOW pgmind.frame_every` fails with *unrecognized configuration parameter*; after any pgmind
  function call — or an explicit `LOAD 'pgmind';` — all three `SHOW` correctly. `SET
  pgmind.vault_id = …` works before the load (Postgres accepts the placeholder) and the value is
  honoured once `_PG_init` runs. This surprises everyone once; it belongs in Quick start and in
  the GUC section.
- **`knowledge.notes().title` is the path's last segment, not the frontmatter `title`.** For
  `projects/auth` with `title: Auth` in the frontmatter, `notes()` reports `auth`; the
  frontmatter value is in `properties->>'title'`.
- **`append_to_section` records `verb = 'insert_blocks'`** in `history()`, because it delegates
  to `insert_blocks` after resolving the anchor.
- **`excise` refuses live content even under `dry_run => true`.** The PM012 refusal is evaluated
  *before* the dry-run short-circuit, so a dry run against live content raises rather than
  reporting. `and_head => true` makes the dry run report
  `WARNING: pgmind: dry run — would erase 1 live and touch 4 history surface(s)` and return the
  nil UUID.
- **Excision's live-removal step records `verb = 'write'`,** not `'excise'` — the removal is an
  ordinary audited write through the normal path. (RFC-005 D7.1 says `'excise'`; the code does
  not. Document the code.)
- **`enable_vault_rls()` creates 10 policies** on this build — every `pgmind` table carrying
  `vault_id` — and emits a `NOTICE … does not exist, skipping` per table on its first run
  because it is idempotent by `DROP POLICY IF EXISTS`.
- **`CREATE EXTENSION` emits `WARNING: pgmind: lz4 toast compression unavailable on this
  server`** on a pgrx-built Postgres. It is not an error.
- **`SELECT (knowledge.append_to_section(…)).*` applied the append twice** — measured: one call,
  two identical blocks in the note. The trap is real, not theoretical.
- **Confirmed error text** (use verbatim, including `DETAIL:`):
  - PM001 `pgmind: invalid note path` / `pgmind_invalid_path — path "/bad/"`
  - PM002 `pgmind: note not found` / `pgmind_note_not_found — path "no/such/note"`
  - PM004 `pgmind: split_block fragment must contain at least two blocks` / `… — found 1`
  - PM007 `pgmind: section not found` / `… — heading path ["Nope"] in note "projects/auth"`
  - PM009 `pgmind: expected_head is not the note's current head` / `… — note projects/auth:
    expected …, head is …`
  - PM012 `pgmind: target is still live at head` / `… — 1 live row(s); pass and_head => true to
    remove it first`
  - PM016 `pgmind: expected_hash is not the block's current content hash` / `… — block …:
    expected …, current is …`
  - Every PM error arrives with `CONTEXT: PL/pgSQL function pgmind.raise_error(text,text,text)
    line 3 at RAISE`. Show it once, then elide it in later examples.
- **The seeded vault** reports `7 notes, 48 blocks, 16 resolved edges, 2 dangling, 9 tags,
  7 revisions, 1383 bytes` immediately after `seed.sql`. A bulleted list item produces **two**
  rows (the `list_item` and its inner `paragraph`) — that is why 7 short notes are 48 blocks, and
  it is worth stating early.

**Output convention.** UUIDs differ every run, so examples elide them the way the README does
(`019fd618-7bae-…`), and verification asserts *executes without error*, never *output matches*.

---

## 3. Files

Everything new lives under `website/docs/`. Nothing outside `website/` ships to the site, so the
plan itself (this file, in `docs/`) is safe from publication.

```
website/docs/index.html        Manual home — map, conventions, status
website/docs/quickstart.html   Purpose, install, first vault, the agent loop
website/docs/concepts.html     Vault model, markdown dialect, identity, errors
website/docs/internals.html    Storage, write path, carry/rebind, history, concurrency, erasure
website/docs/sql.html          Complete reference: 43 functions, types, GUCs, error codes
website/docs/cookbook.html     ~50 recipes in 8 categories
website/docs/docs.css          One stylesheet for all six
website/docs/docs.js           Nav state, TOC + scrollspy, copy buttons, reference filter
```

Modified:

```
website/index.html             Header/CTA/footer links into the manual (kills two TODOs)
website/sitemap.xml            Six new URLs
website/llms.txt               Manual section
```

Untouched: `website/styles.css`, `website/main.js`, `website/robots.txt`, `website/assets/*`.

### 3.1 The seed vault (shared running example)

Examples must be coherent across six pages, so every page draws on **one** demo vault, defined
once in `eval/manual/seed.sql` and rebuilt from scratch before any verification run. Notes:

| Path | Role in the manual |
|---|---|
| `projects/auth` | the main example: frontmatter, headings, a bulleted Decisions section, a `## Log`, wiki-links, tags, one `^id` marker |
| `rfc/auth-01` | link source, so `backlinks('projects/auth')` is non-empty |
| `runbooks/rotate-keys` | the "join knowledge to your own tables" recipe |
| `people/ada` | the excision example (a path that is itself identifying data) |
| `daily/2026-08-06` | append-to-section / agent-log examples |
| `index` | a hub note linking to everything, so `orphans()` is meaningful |
| `notes/scratch` | ambiguous-basename and dangling-link demos |

`projects/auth` is also the note used by the landing page and the README, so the whole project
tells one story.

---

## 4. Page contracts

### 4.0 The shared shell (build this FIRST, then fan out)

Every page is exactly this skeleton; only `<main>` differs. Authors of page content **must not**
alter the shell.

```html
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>…page title… · pgmind manual</title>
  <meta name="description" content="…">
  <link rel="canonical" href="https://marosilabs.github.io/pgmind/docs/…">
  <meta name="theme-color" content="#0a0c10">
  <meta name="color-scheme" content="dark">
  <link rel="icon" href="../assets/favicon.png" type="image/png" sizes="180x180">
  <link rel="stylesheet" href="docs.css">
  <!-- og:* + twitter:* mirroring the landing page, per-page title/description -->
</head>
<body>
  <a class="skip" href="#doc">Skip to content</a>
  <svg class="sprite" aria-hidden="true"><symbol id="gh" …/><symbol id="copy" …/>
       <symbol id="check" …/><symbol id="menu" …/></svg>
  <header class="site-head"> brand · nav (Manual = current) · GitHub · Repository button </header>
  <div class="docs-shell">
    <aside class="docs-side" id="side"> …identical in all six pages… </aside>
    <main class="docs-main" id="doc"> …page body… </main>
    <aside class="docs-toc" aria-label="On this page"><nav id="toc"></nav></aside>
  </div>
  <footer class="site-foot">…</footer>
  <script src="docs.js" defer></script>
</body>
</html>
```

Rules the shell imposes:

- **Sidebar markup is byte-identical across pages.** `docs.js` sets `aria-current="page"` from
  `location.pathname`, so nobody hand-maintains six copies of the active state.
- **The right-hand TOC is generated** by `docs.js` from `main h2[id], main h3[id]`. Authors
  therefore MUST give every `h2` and `h3` a stable, kebab-case `id`.
- Every `<pre><code class="sql">` / `class="shell"` / `class="md"` gets a copy button injected by
  `docs.js`. Authors write no button markup.
- Page ends with `<nav class="page-nav">` prev/next links.

### 4.1 `index.html` — manual home

1. Lede: what the manual covers, and the one-sentence pitch.
2. Status callout: research pre-alpha, phases 0–2 exited, phase 3 in progress; what that means
   for the reader (no upgrade path, unstable SQL surface).
3. Five cards linking to the other pages with a one-line "read this if…".
4. **Conventions**: the three badges (`In main` / `Phase 4` / `Phase 5`), dollar-quoting `$md$`,
   the `FROM`-not-select-list rule, how examples show output, `psql` prompts.
5. "The whole thing in one screen" — a single annotated SQL block: create, write, read, append,
   history.
6. Links out: repo, handbook, RFCs, eval results.

### 4.2 `quickstart.html`

1. **What pgmind is** — three paragraphs. Markdown vault as rows; the API is SQL; no model is
   ever called and no socket is ever opened.
2. **What it is for, at this stage** — honest scope: experiments and prototypes on phases 0–3
   (vault model, block ops, versioning, concurrency, erasure). Not for serving traffic.
3. **When *not* to use it** — one writer, one machine, offline: stay on files.
4. **Install** — prerequisites (`brew install pkgconf icu4c`, rustup pinned by
   `rust-toolchain.toml`, Python 3.10+ for eval), `make setup` / `make build` / `make test`,
   `cargo pgrx run pg18`, `CREATE EXTENSION pgmind;`. Note the lz4 warning, `superuser = true`,
   and that installing into an existing cluster needs `pg_config` on `PATH` +
   `cargo pgrx install`.
5. **Your first note** — `write` → `read` → `blocks` → `links` → `tags`, with real output.
6. **The five moves an agent makes** — read a section; append to a section; patch one block;
   cite a block; look at history. Each ≤ 8 lines of SQL.
7. **Two agents, one note** — `expected_head` demo with the real PM009 text; then the same thing
   done right with `append_to_section` and with `expected_hash`.
8. **One vault per tenant** — `SET pgmind.vault_id`, then `pgmind.enable_vault_rls()`; state
   plainly that the GUC alone is scoping, not security.
9. **What is not here yet** — `context()`, CLI, MCP, with phase badges and links to the roadmap.
10. **When something goes wrong** — the PM error table condensed to cause → fix, plus the
    `(f(x)).*` trap and "function is not unique" after an upgrade.
11. Next steps.

### 4.3 `concepts.html`

1. The vault model in one diagram (ASCII or inline SVG): vault → note → tile/block → edge/tag,
   plus revision/history.
2. **Notes and paths** — grammar table (allowed/forbidden with examples), NFC, case sensitivity,
   1024-byte cap, `path_normalize` vs `path_is_valid`, basename, globs (`*`, `**`, zero-segment
   absorption, literal prefix pushdown, 4096-byte cap).
3. **A note's anatomy** — preamble ‖ tiles; what a tile is (a top-level document child); why the
   concatenation is byte-exact.
4. **Blocks** — the taxonomy table from RFC-002 D2 (kind, addressable, notes, attrs keys),
   containers vs blocks, `ord`, `parent_block`, spans.
5. **Sections and `heading_path`** — heading-text normalisation rule, duplicates, first-match.
6. **The markdown dialect** — precedence rule first, then: wiki-links (closed grammar, target /
   anchor / alias, escapes, trimming, NFC, empty-target non-link), transclusions, markdown links
   (scheme test, `#fragment`, `.md` strip), tags (charset, whitespace rule, no ATX collision),
   `^id` markers (which kinds, innermost-carries, stripped from hashes). Each with a
   short input → `knowledge.links()`/`tags()` output pair.
7. **Frontmatter** — YAML mapping only, invalid-is-content, reserved `tags` and `pgmind-pin`.
8. **Content hash** — the seven normalisation steps, worked example showing quote-context
   invariance, what the hash is for (dedup, carry, embedding reuse) and what it is not
   (identity).
9. **Identity** — UUIDv7 block ids, opaque; ID vs hash vs `^id` label; the three-way invariant
   from RFC-004 A1.
10. **Link resolution** — exact → unique basename → dangling(`missing|ambiguous|invalid`);
    repair on create/rename; anchors resolve at query time.
11. **Revisions** — `seq`, `verb`, `author`, `source`, `head_revision`, `history_floor`,
    reconstructable.
12. **Vaults and tenancy** — one column, one GUC, one policy.
13. **Errors** — the PM001–PM017 table with "what the caller should do".
14. **Sharp edges** — the list from §2.3 that is conceptual rather than operational.

### 4.4 `internals.html`

1. **The eleven laws**, compressed, each with "what this forbids".
2. **Two lanes** — byte lane (`note.preamble` + `tile[]`) and semantic lane (`block[]`);
   `source_of()` invariant; why current state never pays for history.
3. **The storage schema** — every table with its columns and the reasoning that shaped it:
   `note`, `revision`, `tile`, `block`, `edge`, `tag`, `note_revision`, `block_revision`,
   `note_frame`, `excision_log`, `excision_replay`. Include the index list and the deliberate
   omissions (no FK on `head_revision`; `NO ACTION` on `parent_block`).
4. **The write path, step by step** — normalise → advisory path lock → `FOR NO KEY UPDATE` →
   re-read under the lock → CAS → byte-identical short-circuit → parse → carry → capture
   pre-image → reconcile (INSERT/UPDATE before DELETE) → revision row → history rows → maybe
   frame → edge repair. A numbered walkthrough with the exact ordering rules and *why each
   order* is load-bearing.
5. **The deterministic carry** — pass 1 `^id` claims (collision rules), pass 2a same-section
   hash, pass 2b any-section hash, pass 3 mint/remove. Worked example with three blocks.
6. **Heuristic rebinding (RFC-004 Part B)** — where it runs (between passes 2 and 3, `write()`
   only, never the block ops); unigram ∪ bigram features; Dice; bidirectional containment for
   split runs; the order-monotonic DP; τ=0.5, τ_split=0.6, `MAX_RUN=4`, `CELL_BUDGET=40 000`
   and the declared fallback; the published measurements
   (`eval/published/rebinding-v2.json`: recall 0.951, precision 0.980, inferred recall 0.826,
   38/42 cases perfect) and the four cases it will never solve.
7. **Block-operation mechanics** — fragment arity (parentless count), splice + separator
   synthesis at the seam only, the PM008 post-splice re-parse assertion, container-children
   constraints, item re-marking to the destination list's style, anchor legality.
8. **The history engine** — pre-images not post-images; X1 (locality) and X2 (no parser) and
   what each buys; the KEEP/INS script format; positional facts (`ord`, `tile_ord`, spans,
   `heading_path`) in `note_revision` vs content-visible columns in `block_revision`, and the
   economics that forced the split; frames and `pgmind.frame_every`; the reconstruction
   algorithm (anchor at-or-above, apply backwards, single snapshot); redacted pre-images.
9. **Concurrency** — two lock domains (the row, the name); why `FOR NO KEY UPDATE`; every read
   taken from under the lock; CAS at note and block granularity and the false-conflict cost of
   having only the coarse one; append's both-survive rule; multi-note ascending-id ordering
   including edge repair; pgmind never raises 40001 and never retries internally.
10. **Erasure and retention** — refuse-don't-half-erase; the `pg_catalog` sweep and why not
    `information_schema`; splice-not-NULL redaction; in-transaction verification that aborts;
    the audit/replay split and what a restored dump does; retention floor + floor frame +
    revisions never deleted.
11. **Verification** — what `verify_note`, `verify_history` and `verify_excision` each check,
    and what "empty result" means.
12. **Capacity and performance** — the published numbers with their denominators:
    `capacity-model-v1` (429.9 B/block row, 657.9 B/block all-in at 10k notes/230k blocks,
    read p95 0.454 ms) and `capacity-model-v2` (effect rows/revision by verb 1.00/1.00/3.36,
    history 0.207× current state, deep `as_of` p95 0.56 ms vs 0.33 ms head read). Include the
    caveats the RFC states, not just the flattering ratios.
13. **Backup and restore** — `pg_extension_config_dump` registration set; what is deliberately
    absent; the "restore onto an older extension build fails partway" hazard.
14. **The AI-free boundary** — no model, no socket, no background worker, no trigger; what that
    means for allowlisting and for determinism.

Every claim on this page cites its source file or RFC decision inline (e.g. `write.rs:871`,
RFC-005 D5.2) so a reader can check it.

### 4.5 `sql.html` — the reference

Layout: a filter input at the top (`docs.js` filters function cards by name/summary as you
type), then grouped sections. **One card per function**, and the card template is fixed:

```
### knowledge.append_to_section  [badge]
<signature block — exactly as pg_get_function_arguments renders it>
One-sentence purpose.
Parameters   : table (name, type, default, meaning)
Returns      : table (column, type, meaning)  — or a scalar description
Raises       : PM0xx list with the trigger condition
Notes        : the gotchas that matter for this function
Example      : SQL + real captured output
See also     : sibling functions
```

Groups, in order:

- **A. Types and values** — `markdown` (input rules, size GUC, byte fidelity), `block_kind`,
  `edge_kind`, `op_result`, and how to consume a composite return correctly.
- **B. Parsing without storing** — the five `markdown`-value functions.
- **C. Reading a vault** — `read`, `read_section`, `notes`, `blocks(path)`, `links(path)`,
  `backlinks`, `tags()`, `tagged`, `orphans`, `stats`.
- **D. Writing** — `write`, `insert_blocks`, `append_to_section`, `update_block`, `move_block`,
  `split_block`, `merge_blocks`. Each documents its identity outcome (RFC-004 A2) explicitly.
- **E. Note lifecycle** — `delete_note`, `undelete_note`, `move_note`.
- **F. History and time travel** — `history`, `read_as_of` ×3, `blocks_as_of` ×2, `diff`,
  `blame`.
- **G. Administration** — `verify_note`, `verify_history`, `excise`, `verify_excision`,
  `retain`, `enable_vault_rls`, `path_is_valid`, `path_normalize`; a note on `raise_error`
  being internal and revoked.
- **H. Server settings** — the three GUCs, with ranges, defaults, context, and what changing
  each actually trades.
- **I. Error codes** — full PM001–PM017 table: SQLSTATE, condition name, meaning, what the
  caller should do, which functions raise it. Plus how to catch them in plpgsql
  (`WHEN sqlstate 'PM009'`) and in client libraries.
- **J. Not yet implemented** — one clearly-badged table listing everything from §2.2 with the
  phase it lands in. This section exists so nobody has to discover the gap by getting an error.

### 4.6 `cookbook.html`

Every recipe: **Problem** (one line) → **SQL** → **What you get** (real output) → **Why it
works** (two lines, linking into concepts/internals). Target ≈ 50 recipes across eight
categories.

1. **Agent memory** — atomic append from N workers; read-modify-write with CAS and a plpgsql
   retry loop; disjoint block patches with `expected_hash`; idempotent re-write; per-session
   scratch note; a fact ledger deduplicated by `content_hash`; toggling a task checkbox; writing
   a note with a `^id` marker so identity survives an external round trip.
2. **Navigation and graph** — table of contents; indented outline; backlink panel with excerpts;
   broken-link report; ambiguous-basename report; orphans and hubs; recursive-CTE link
   neighbourhood at distance N; shortest path between two notes; tag co-occurrence; tag
   intersection and exclusion; glob + frontmatter-property filters; expanding `![[…]]`
   transclusions into a composed document.
3. **Retrieval, until `context()` exists** — FTS over blocks with `tsvector` and `ts_headline`;
   a ranked search function; hand-rolled context assembly (pins + backlinks + link distance,
   packed to a character budget, with a citation per block); an
   `embedding_hook(block_id, content_hash, …)` table with pgvector that never re-embeds an
   unchanged block; a fusion query blending FTS rank, link distance and recency. Each labelled
   as *your* code, not pgmind's, and each pointing at the Phase 5 function that replaces it.
4. **History, audit and citations** — emit a citation (`path#^id @ revision`) and resolve it
   later; reconstruct exactly what an agent read; diff two revisions; blame with confidence;
   list blocks whose identity was *inferred* for human review; a vault-wide activity feed;
   "what changed in the last day"; restore a note to an earlier revision; undelete; rename with
   edge repair.
5. **Multi-tenancy and access** — tenant-per-vault with `SET LOCAL` inside a transaction (the
   connection-pool-safe form); enabling RLS and what `force` does; the grant set an application
   role actually needs; a query that proves isolation.
6. **Operations** — health sweep with `verify_note` over every note; `verify_history` sweep;
   bounded history with `retain` (dry run first); the full excision workflow with verification;
   bytes-per-table capacity query; a backup checklist; importing a folder of markdown before the
   CLI exists (shell loop + `psql`); exporting the vault back to files.
7. **Application integration** — psycopg example with correct parameter binding and PM-error
   handling; a node-postgres example; SQL wrapper functions shaped like the future MCP tools;
   `LISTEN`/`NOTIFY` on new revisions (flagged as reaching into `pgmind.*`, Law 11); a
   materialised outline view for fast UI loads.
8. **Sharp edges, demonstrated** — each of the traps in §2.3 as a two-block before/after: the
   `(f(x)).*` double edit; appending to a list-terminated section; wiki-links in table cells;
   the reference-definition collision; whole-note rewrite vs block ops for identity; PM011 after
   `retain`; `vault_id` without RLS.

### 4.7 Mandated anchor ids

The sidebar is byte-identical in all six pages and links into these anchors, so they are a
contract. An author who renames one breaks five other files. Every `h2`/`h3` needs *some* id
(the TOC is generated from them); these specific ones are fixed:

| Page | Required `h2` ids, in order |
|---|---|
| `quickstart.html` | `what-it-is`, `when-to-use`, `install`, `first-note`, `agent-loop`, `concurrency`, `tenancy`, `not-yet`, `troubleshooting`, `next` |
| `concepts.html` | `model`, `paths`, `anatomy`, `blocks`, `sections`, `dialect`, `frontmatter`, `hashing`, `identity`, `resolution`, `revisions`, `vaults`, `errors`, `edges` |
| `internals.html` | `laws`, `lanes`, `schema`, `write-path`, `carry`, `rebinding`, `block-ops`, `history`, `concurrency`, `erasure`, `verification`, `capacity`, `backup`, `boundary` |
| `sql.html` | `types`, `parsing`, `reading`, `writing`, `lifecycle`, `history`, `admin`, `settings`, `errors`, `not-yet` |
| `cookbook.html` | `agent-memory`, `navigation`, `retrieval`, `history-audit`, `tenancy`, `operations`, `integration`, `sharp-edges` |

Per-function anchors in `sql.html` are `fn-<schema>-<name>`, e.g.
`fn-knowledge-append-to-section`, `fn-pgmind-verify-note`. Overloads share one entry.

Recipe anchors in `cookbook.html` are `r-<kebab-slug>`, e.g. `r-atomic-append`.

### 4.8 The shell exemplar

`website/docs/index.html` is finished and is the template. Copy its `<head>` (adjusting title,
description, canonical, og/twitter), its sprite, its `<header>`, its entire `<aside
class="docs-side">`, and its `<footer>` verbatim. Change only `<main>` and the per-page metadata.
`docs.css` and `docs.js` are finished; do not edit them — report a need instead.

Markup vocabulary available (all styled, nothing else needed):

- `<div class="code"><div class="cap">psql</div><pre><code class="sql">…</code></pre>
  <pre class="out"><code class="out">…</code></pre></div>` — input then captured output. The
  `cap` div is optional. `class="shell"` on the code element for shell snippets.
- `<div class="note-box">` (informational), `.note-box.warn`, `.note-box.trap`; optional
  `<span class="lbl">Label</span>` as the first child.
- `<span class="badge ok|next|admin|err">` — see §5.
- `<div class="table-scroll"><table>…</table></div>` for anything wide.
- `<div class="cards"><a href><b>Title</b><span>blurb</span></a>…</div>`.
- Reference entries: `<section class="fn-group">` wrapping `<div class="fn" id="fn-…">` with
  `<h3>`, `<div class="sig"><code>…</code></div>`, `<p class="purpose">`, then `<h4>` labels
  (`Parameters`, `Returns`, `Raises`, `Notes`, `Example`, `See also`).
- `<nav class="page-nav">` with `<a>`/`<a class="next">` containing `<span>Previous|Next</span>`.

---

## 5. Style rules (binding on every author)

1. **Voice matches the existing site**: plain, specific, no marketing verbs, no exclamation
   marks, no "simply"/"just"/"easily". Say what happens and what it costs. The landing page and
   the README are the tone reference.
2. **Every claim is checkable.** Numbers come from `eval/published/*.json`. Behaviours cite a
   source file and line, or an RFC decision id.
3. **Honesty about maturity is a feature.** Where something is unimplemented, weak, or
   surprising, say so in the same breath as the capability.
4. **No invented API.** If it is not in §2.1, it does not appear except inside a "not yet" block.
5. **Examples are real.** Every SQL block must have been executed against the seed vault and its
   output pasted, trimmed only for width. Illustrative-only blocks are labelled as such.
6. **Errors are shown as errors** — the actual `ERROR:`/`DETAIL:` lines, captured.
7. **SQL formatting**: lowercase keywords are *not* used — the site's existing examples use
   uppercase `SELECT`/`FROM`; match that. Two-space continuation indent, `=>` for named
   arguments, `$md$…$md$` for markdown literals.
8. **Accessibility**: one `h1` per page; heading levels never skip; every `h2`/`h3` has an `id`;
   tables have `<th scope>`; the skip link works; focus styles are inherited from the shell.
9. **Self-contained**: no external fonts, scripts, styles, or images. Everything relative.
10. **Width discipline**: code lines ≤ 96 characters so mobile scroll is bounded; tables wrapped
    in `.table-scroll`.

---

## 6. Execution order

**Stage 0 — foundation (serial, before any page content).**
- `eval/manual/seed.sql`: the demo vault, rebuildable from empty.
- Rebuild the database, run the seed, capture the baseline outputs the pages will quote.
- `website/docs/docs.css` and `website/docs/docs.js`.
- One finished reference page (`index.html`) as the shell exemplar every other author copies.

**Stage 1 — page authoring (parallel; one agent per file, no shared files).**
- `quickstart.html`, `concepts.html`, `internals.html`, `sql.html`, `cookbook.html`.
- `sql.html` and `cookbook.html` are the two large ones and may be split into two agents each if
  size warrants (reference groups A–E / F–J; cookbook categories 1–4 / 5–8), merged by the
  owner of the file.
- Each author: reads this plan §2 and §4.x for its page, reads the sources it cites, writes the
  page, **runs every SQL block against the live database**, pastes real output.

**Stage 2 — verification (parallel, one verifier per page, adversarial).**
Each verifier independently:
- extracts every SQL block from the page and re-runs it against a **freshly seeded** database;
- greps every `knowledge.`/`pgmind.` identifier in the page against the §2.1 inventory and fails
  on anything not present (outside a "not yet" block);
- checks every internal link and anchor resolves;
- checks heading ids exist and are unique;
- reports findings; it does not silently fix.

**Stage 3 — integration (serial, owner).**
- `website/index.html`: header nav gains **Manual**; both `#get-started` CTAs repoint to
  `docs/quickstart.html` (removing the two TODO comments); the "get started" prose links to the
  manual; the footer gains a Manual link.
- `website/sitemap.xml`: six new `<url>` entries.
- `website/llms.txt`: a `## Manual` section listing the six pages with one-line descriptions.
- Fix everything Stage 2 found; re-run the SQL sweep once more over all six pages.

**Stage 4 — final gate (owner).**
- Full SQL sweep from an empty database: zero unexpected errors.
- Link/anchor sweep: zero broken.
- Inventory sweep: zero invented identifiers.
- Read every page top to bottom once for voice and duplication.

---

## 7. Definition of done

Checked 2026-08-09. An item marked **(gate)** is re-checked by `make eval` on every CI run and
cannot silently regress; an item marked **(human)** was verified once by reading, and this list
is the only record that it was.

- [x] Six pages exist, share one shell, and are reachable from the landing page in one click.
      **(gate:** `manual-inventory` resolves every internal link and anchor**)**
- [x] Every SQL example in the manual executes green against a freshly seeded database, or is
      an intentional error whose real message is shown. **(gate:** `manual-examples`, 283 blocks
      across six pages; 19 non-SQL or two-session blocks reported by name, never dropped**)**
- [x] No identifier appears that is not in §2.1, outside an explicitly badged "not yet" block.
      **(gate:** `manual-inventory`, 918 identifiers against a live catalog**)**
- [x] Every function in §2.1 is documented in `sql.html` — all 43, plus 4 GUCs, 4 types, and
      17 error codes. (3 GUCs / 16 codes until §2.5; `pgmind.author` and PM017 landed with
      RFC-011 mid-authoring.) **(gate:** `manual-inventory` checks the reverse direction too —
      a function that ships without a reference entry fails**)**
- [x] The cookbook has ≥ 40 recipes, each with real output. 73 recipes. **(human** for "real
      output"; the SQL behind it is **gate)**
- [x] Internals cites a source file or RFC decision for every non-obvious claim. **(human)**
- [x] All internal links and anchors resolve; no external resource is fetched. **(gate)**
- [x] `sitemap.xml` and `llms.txt` list the manual; the two TODOs in `index.html` are gone.
      **(human)**
- [x] Pages read correctly at 375 px, 768 px and 1440 px, dark theme only, matching the landing
      page's design language. **(human)**

### 7.1 What the gates deliberately do not check

Stated so that a green `make eval` is not mistaken for more than it is:

- **That an example's pasted output still matches.** The gate asserts a block runs, not that
  its output is byte-identical to what the page shows. UUIDs and timestamps differ every run,
  so the pages elide them; asserting on the rest would mean a second serialisation format to
  maintain. A query that starts returning the wrong *rows* passes this gate.
- **That a name still badged "not yet" has not since shipped.** At row granularity the checker
  cannot tell a missing function from a missing *overload* of one that exists, and a gate that
  cries wolf gets ignored. §2.2 is the human check for that direction.
- **Prose.** No gate reads English. Every claim in §2.3 and §2.4 is a human assertion.

---

## 8. Environment for verification

The seed vault is `eval/manual/seed.sql`, committed. It asserts its own `knowledge.stats()`
at the bottom — 7 notes, 48 blocks, 16 resolved edges, 2 dangling, 9 tags, 7 revisions,
1383 bytes — so a seed that has drifted from what the pages describe refuses to seed at all
rather than quietly rebasing every example onto a different vault.

The gates run with everything else:

```bash
make eval          # includes manual-examples and manual-inventory
```

To drive a page by hand against a `cargo pgrx run` cluster:

```bash
cd extension && cargo pgrx start pg18
PGBIN=~/.pgrx/18.4/pgrx-install/bin
$PGBIN/dropdb  -h localhost -p 28818 --if-exists manual
$PGBIN/createdb -h localhost -p 28818 manual
$PGBIN/psql -h localhost -p 28818 -d manual -f eval/manual/seed.sql
```

The first version of this plan put the seed in a session scratchpad and said "nothing from the
verification harness is committed". That is why the manual's central claim — every example was
executed — became unreproducible within three days: the vault the pages were verified against
no longer existed. The seed is repo state now, and the claim is a gate.
