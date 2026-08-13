# RFC-007: Query API & MCP Surface — the freeze, and the agents' front door

- **Status:** Draft — proposed for acceptance. The query half (D4, D5) is implemented and gated; the MCP half (D6–D9) is designed and **not built**.
- **Phase:** 5
- **Owner:** project author
- **Created:** 2026-08-13 · **Accepted:** — · **Frozen:** —

## 1. Context

Four frozen RFCs defer decisions to this one, and one of them has been outstanding since Phase 0. RFC-001 D10 proposed the schema names `knowledge` and `pgmind` "ratified in RFC-007"; RFC-003 restates the same deferral and adds two more — the FTS column "belongs to the search RFC (RFC-007/010) with its language-configuration decision" (§D4) and "RFC-007 freezes them for 0.x" of the Phase 2 read signatures (§D7); RFC-005 D5.11 folds `patch_block` into `update_block` as "a declared deviation … to be ratified when RFC-007 freezes the API and MCP surface"; RFC-011 D2 withholds a `message` parameter because "function signatures are precisely what RFC-007 freezes at Phase 5; changing them twice is worse than changing them once."

Phase 4 was cut, so this is the next RFC written, and 0.1.0 ships at the end of the phase it governs (RFC-000 D7: "First public release … is the Phase 5 vertical slice: import → query → history → MCP → deterministic `context()`").

**What is not here.** Deterministic context assembly and token budgeting are RFC-008's — `knowledge.context()`, `context_explain()`, and every question about counting tokens. Ranking quality, BM25 and the retrieval planner are RFC-010's. Packaging the MCP binary is RFC-012's. This RFC decides the *query surface* an agent reaches through, the *freeze* on it, and the *server* that presents it.

**The problem this exists to solve** is recorded in [`docs/ease-of-use-audit.md`](../ease-of-use-audit.md): eleven requirements from a real multi-tenant agent-memory product, each of which must be easier than a folder of markdown files. At the audit, four had no answer and one was unsafe. Rows 0–4 of its §9 have since shipped. This RFC is rows 5 and 6 — search, and the surface an agent actually touches.

---

## 2. Decision

### D1. The schema names are `knowledge` and `pgmind`, ratified

`knowledge` is the public API and the only schema an application, an agent or a document may name. `pgmind` is storage plus the admin surface, is not covered by any stability promise, and its functions are `REVOKE`d from `PUBLIC` where they are destructive (RFC-005 D7).

This ratifies RFC-001 D10 and the identical deferral in RFC-003 D1. The names have been shipped since Phase 1; what was missing was the decision that they are *the* names. They are.

A third schema is out of scope for 0.x. An application's own wrappers belong in the application's schema — the cookbook's recipes use `app.*` for exactly this reason, and nothing in `knowledge` will ever be created there on the user's behalf.

### D2. The `knowledge` surface is frozen for 0.x, and "additive" is defined

From acceptance of this RFC until 1.0, changes to `knowledge.*` MUST be additive. Precisely — and the precision is the point, because "additive" is where compatibility promises usually rot:

**Permitted.**
1. A new function.
2. A new parameter on an existing function, **with a default**, appended **after every existing parameter except `vault`** (D3 keeps `vault` last).
3. A new overload on a distinct argument type list, subject to the ambiguity rule below.
4. Widening what an argument accepts, where every previously-accepted value keeps its meaning.

**Forbidden without a new RFC.**
5. Removing or renaming a function, a parameter, or a returned column.
6. Reordering parameters or returned columns.
7. Changing a returned column's type.
8. Adding a returned column to a `RETURNS TABLE` function. This is *not* additive: `SELECT *` consumers gain a column they did not ask for, `INTO` bindings break, and the composite type changes shape. New columns need a new function.
9. Changing a default's value, which is a behaviour change wearing a signature's clothes.
10. Narrowing what any function accepts. A feature that cannot handle an input pgmind already stores is not permitted to reject it — see D4's bound, and RFC-003 D6's identical rule for batching.

**The overload ambiguity rule.** A new overload MUST NOT change which function an existing call resolves to. This is not hypothetical: adding `vault text DEFAULT NULL` to the zero-argument `knowledge.tags()` created a one-argument `text` form, and PostgreSQL resolves an uncast literal to `text` by preference — so `tags($md$# doc$md$)`, which the manual documented as reaching the markdown parser, silently began resolving to the vault overload. It raises `PM018` in the common case and, if the text happens to name a real vault, quietly answers a different question. Every documented example already carried `::markdown` so nothing broke; the manual's advice did. Under this rule that change would have required the cast to be documented *first*.

*Why freeze now, before the MCP server exists.* Because the MCP tool surface is a wire protocol over these signatures, and a protocol whose backing signatures still move is not a protocol. RFC-003 D7 already declared the read shapes normative and deferred only the 0.x commitment; this supplies it.

### D3. `vault` is the last parameter, resolved argument → GUC, and is never an agent's to choose

Every vault-scoped `knowledge.*` function takes `vault text DEFAULT NULL` as its **final** parameter. It accepts a vault *name* or a uuid in its text spelling; a value that parses as a uuid is looked up by id, otherwise by name. `NULL` falls back to the `pgmind.vault_id` GUC. A vault that is not in the registry is `PM018` — never an empty result, and never a silently created vault.

Last position is normative, not cosmetic: it is what makes rule D2.2 safe. Any future parameter is appended before `vault`, so `vault` stays the parameter callers can always name positionally last or by keyword.

> **X1 (the vault is server-side).** In any deployment where the caller is a language model, the vault MUST be selected by the trusted layer that owns the connection, and MUST NOT appear in the tool schema the model sees. A `vault` argument a model can fill in voids every isolation property in D8.

X1 is the rule the whole tenancy story rests on, and it is a rule about the *server*, not about SQL. The SQL parameter is correct and necessary — an application joining across vaults needs it per row, which a session GUC cannot express — and it is exactly what must not be exposed onward. D6 and D8 carry this through.

### D4. Search is PostgreSQL's, indexed by a bounded expression, and returns the innermost match

```sql
knowledge.search(q text, path text DEFAULT NULL, tags text[] DEFAULT NULL,
                 limit_n integer DEFAULT 20, vault text DEFAULT NULL)
  RETURNS TABLE (path text, block_id uuid, heading_path text[], excerpt text, rank real)

knowledge.tagged(tag text, path text DEFAULT NULL, vault text DEFAULT NULL)
  RETURNS TABLE (path text, block_id uuid, tag text)
```

`q` is `websearch_to_tsquery` syntax. That parser is chosen over `to_tsquery` because **it cannot be made to raise**: the input to a search box is whatever a user or an agent types, and a syntax error is not an answer.

`tags` keeps only blocks carrying **every** tag listed, where a tag counts if it is on the block **or** on its note — so a frontmatter tag scopes the whole document, which is what a reader means by it. `path` narrows to one note in both functions and is `PM002` when the path has no live note: a tag nothing carries and a path you mistyped are different answers and must not look alike.

**A query with no lexemes in it is a filter, not a failure.** Empty, whitespace or all stop words means *no text predicate*: `tags` and `path` still apply, and `rank` is NULL — there is no ranking function involved, and a fabricated `0.0` is a number a caller could sort by. With no other predicate either, the result is empty rather than the whole vault, which is the only safe reading of an empty search box. This is what makes `tags` a usable argument at all: as first implemented it was reachable only alongside a text query it has nothing to do with, so tag *intersection* — which `tagged()` cannot express, taking one tag — had no one-call form. Found by adversarial review of the shipped function, and gated.

**The index is an expression index, not a generated column.** RFC-003 D4 anticipated "a generated tsvector column later" at the cost of "an upgrade-script table rewrite". This RFC does not take that cost, and does not need to:

```sql
CREATE FUNCTION pgmind.search_vector(content text) RETURNS tsvector
  LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT
  AS $$ SELECT to_tsvector('english', left($1, 100000)) $$;

CREATE INDEX block_fts ON pgmind.block USING gin (pgmind.search_vector(content));
```

Three reasons, in order of weight. First, a stored generated column of type `tsvector` is a **content-derived column that `excision.rs`'s type enumeration cannot see**, which reintroduces the exact defect shape RFC-005 D7 was amended to close — a filtered enumeration producing a positive erasure attestation over an unopened lane. An index has no such column. Second, `pgmind.block` is RFC-003's table and adding a column to it is a storage change with a migration; an index is additive. Third, changing the configuration later is `REINDEX`, not a table rewrite.

**`english` is fixed, and that is a real limitation.** It is the wrong stemmer for a vault that is not in English. A settable configuration was cut: it cannot live in a generated column at all (the expression must be `IMMUTABLE`), and as a GUC it would mean the index and the query could disagree about which configuration built which row — a silently wrong answer, which is worse than a documented English bias. The single definition in `pgmind.search_vector` is what keeps the index and `search()` in agreement; **an expression index is used only when the query repeats the expression exactly**, and two spellings of a text-search expression drift into a sequential scan that still returns the right answer, which is the kind of defect that survives every test you would think to write.

**The bound is load-bearing and is a D2.10 obligation.** `to_tsvector` refuses to build a vector past 1,048,575 bytes, and a block of distinct words reaches that at **~849 KB of input** — comfortably inside the 8 MiB `pgmind.max_document_bytes` default. An unbounded expression index would therefore make the *write path* reject a note pgmind stores today, on index maintenance, with a bare `54000`. Truncating at 100,000 characters means such a block is searchable by its opening rather than unstorable. This is measured, and the gate's negative control asserts the unbounded form still raises (§5).

**A result is never an ancestor of another result.** A list item's `content` includes its own paragraph (RFC-002 D7), so `- rotate the key` matches as both the item and the paragraph, and a naive implementation bills an agent for the same sentence twice. A block is dropped when one of its children also matched. The surviving block is also the better citation and the simpler edit target (RFC-004: targeting the nested block "is the simple path").

*What this retires.* The cookbook shipped a recipe building `app.block_fts`, a materialized view over `knowledge.blocks()`, refreshed by hand. It is withdrawn and its removal documented in place, because it was **unsafe, not merely superseded**: the view carried no `vault_id`, so a `REFRESH` issued while `pgmind.vault_id` pointed elsewhere replaced one tenant's search index with another tenant's content, silently. Law 7 (incremental maintenance) is the general argument; this is the specific one.

*What is knowingly absent.* Fuzzy and typo-tolerant search. It needs `pg_trgm`, the only new external dependency anything in this area would have required, and the in-house alternative was checked and does not work — `rebind.rs`'s Dice similarity is over word tokens, so `excalation`/`escalation` score 0. Requirement 10 of the audit's §3 is **unmet, deliberately, and listed as unmet**. `mode` is absent from the signature so it can be added under D2.2.

### D5. `message` lands here, because this is the freeze

RFC-011 D2 withheld an optional `message` from every mutating function on the grounds that signatures change once, at this freeze. This is that freeze, so it is added: `message text DEFAULT NULL`, appended before `vault`, on `write`, `write_many`, the six block operations, `delete_note`, `undelete_note` and `move_note`. It populates `revision.message`, which has existed and been readable through `knowledge.history()` since Phase 2 and has been NULL for every row ever written.

Nothing else about provenance changes. `pgmind.author` remains a `Userset` string GUC and remains an assertion rather than an authenticated fact (RFC-011 D1); D7 below decides where the MCP server gets the value it asserts.

### D6. The MCP tool surface is eight tools

`read_note` · `list_notes` · `search_notes` · `write_note` · `append_to_section` · `update_block` · `delete_note` · `restore_note`.

Six are the filesystem six, mapped one for one. Two are a matched pair for the one thing a filesystem answers with `rm` and pgmind must answer with a tombstone — and therefore also with its inverse.

**D6.1 — Three documents name three numbers, and they are not really in conflict.**

The audit's §5 sets the bar at six — `read`, `write`, `list`, `edit`, `glob`, `grep` — and is the only one of the three that gives a reason: "surface *size* is itself a failure mode — more tools means more wrong choices, more tokens spent on definitions, and worse selection accuracy." The mapping is exact: `read`→`read_note`, `write`→`write_note`, `list`+`glob`→`list_notes` (the glob is an argument; a filesystem needs two tools only because it has no server to fold them), `grep`→`search_notes`, and `edit`→**two** tools. Five slots collapse to four; the `edit` slot expands to two, because `append_to_section` serializes so two concurrent appends both land, and `update_block` carries block-granular CAS so two agents patching different paragraphs never conflict. That split is the only place the surface spends a name on capability, and it is the only capability on it that a file does not have — requirement 7 is the single row where the audit's own table rates the filesystem "worse than pgmind".

> **The admission test for anything past six:** a tool is admissible only when its *absence* would make an agent reach for a tool already inside the six and do damage with it. Capability added is not a reason; misuse prevented is.

That test admits exactly two. Without `delete_note`, "forget what I saved about the old auth scheme" becomes `write_note(path, "(removed)")` — which succeeds, returns a revision, and leaves a live note in every listing, every backlink and every search result. Without `restore_note`, `delete_note` is the only irreversible verb on the surface *and its damage is invisible*: `store::note_by_path` filters `tombstoned_at IS NULL`, so a deleted note cannot be read at any point in its own history, and an agent that "fixes" its mistake by re-writing the path trips `assert_path_free` ([store.rs:732](../../extension/src/store.rs#L732)) and forecloses the operator's rescue too. `knowledge.undelete_note` already ships, takes one argument, and reconstructs from history. Shipping the destructive twin while cutting the free, already-built inverse to save a definition slot is not a size argument; it is a hazard. The pair is admitted together or not at all.

Against the plan's **ten**: six survive, four go, two are added. `patch_block` is not cut — it is `update_block`, the name the SQL took under RFC-005 D5.11, and the MCP name follows the SQL name (this discharges the ratification RFC-005 D11 asked for). `backlinks` becomes a *field* on `read_note`, because `knowledge.backlinks(path)` takes exactly the argument `read_note` was already called with. `history` becomes two fields — `read_note(as_of)` and `read_note.last_change`. `diff` is dropped: it requires two revision uuids no surviving tool emits, and its output is a review artifact for a human. `get_context` is dropped by constraint — it is RFC-008's and does not exist. Ten − 4 + 2 = 8.

Against the handbook's **five** (`read/write/append/search/context`, [PGMIND.md](../PGMIND.md):222-225, and the highest-precedence document): four of the five are here verbatim, and the fifth is `context`, which this RFC is forbidden to design. So the handbook does not describe a five-tool surface competing with an eight-tool one; it describes a four-tool core plus a forward reference. When `context` lands under RFC-008 the handbook's five is a strict subset of the surface. The three additions beyond that core are `list_notes` (requirement 4, scored 1-of-4 against today's SQL — the handbook line predates the audit), `update_block` (the handbook's own pitch calls block operations the place where "'why bother' turns into 'oh'"; a surface whose only correction tool is whole-document `write` routes every fix through the rebinder, silently, and the rebinder is this project's declared #1 research problem), and the delete/restore pair.

**D6.2 — Seven rules, so a ninth tool can be tested rather than argued about.**

- **R1 — Argument-gate.** A chained tool's required arguments MUST be obtainable from exactly one source, and that source MUST emit all of them in one response. Descriptions are advisory and get skimmed; a schema you cannot populate is a hard constraint. `update_block` cannot be selected without a `(block_id, expected_content_hash)` pair, and only `read_note` and `search_notes` mint one. Corollary, binding on the emitting side: **any response carrying a `block_id` MUST carry its `content_hash` in the same object.**
- **R2 — One name, one meaning.** `section` means the ancestor chain **plus the heading's own text**, everywhere. `revision` is always note CAS, `content_hash` always block CAS, `message` always provenance. This costs something real: `knowledge.search` projects `heading_path` verbatim, which for a heading row is the *parent* chain, so an uncorrected pass-through would send `append_to_section` into the wrong section. D6.4 pays it.
- **R3 — No always-fillable escape.** No `force`, no `overwrite`, no `mode`. A model under error pressure takes the branch that always works. Every guard is either a token a prior read minted, or an argument whose *omission* is a refusal rather than a clobber.
- **R4 — Errors are recovery steps, never a second statement.** The failure payload carries the corrected argument, built only from what the aborted statement already raised — PM009's DETAIL carries expected *and* observed head, PM016's carries expected and observed hash — plus a static, code-keyed hint. The server MUST NOT issue a follow-up query to build an error payload; that would break D7's one-statement rule.
- **R5 — Pay a cut back in the return shape of the survivor.** Fuzzy search is cut, so a mistyped tag is a silent empty result: `search_notes` returns `available_tags` when a tag filter matched nothing. Zero surface cost, one dead end closed.
- **R6 — Disclose an unresolved SQL disagreement; do not resolve it silently.** Audit §4.5 is open: `read_section` takes the first matching heading, `append_to_section` the last. `read_note`'s outline stamps `ambiguous: true` on any section array that is not unique within the note. The MCP layer is the only place in the product where that defect is visible to the party who can act on it.
- **R7 — Never fabricate a number.** `rank` is null on a tag-only hit, not zero — the same rule the SQL now follows (D4). Sizes are reported in **bytes**, which are real and free from spans the block rows already carry. No token counts: tokens are RFC-008's.

**D6.3 — The tools.** No tool takes a vault (X1). `message` is optional on every mutator and is D5's parameter. Author is a session GUC set at connection setup, never an argument.

| # | tool | arguments | returns |
|---|---|---|---|
| 1 | `read_note` | `path` · `section?` · `as_of?` | `{path, note_id, title, description, properties, revision, updated_at, last_change{author,source,message,at}, extent, bytes, content, outline[], blocks[], links[], backlinks[]}` |
| 2 | `list_notes` | `glob? = "**"` · `limit? = 100` · `offset? = 0` | `{notes[{note_id, path, title, description, revision, updated_at}], returned, more}` |
| 3 | `search_notes` | `query?` · `tags?` · `path?` · `limit? = 20` | `{hits[{path, block_id, kind, section, excerpt, content_hash, rank, tag_scope}], available_tags?}` |
| 4 | `write_note` | `path` · `content` · `expected_revision?` · `message?` | `{path, revision, created, unchanged, dangling_links[]}` |
| 5 | `append_to_section` | `path` · `section` · `content` · `message?` | `{path, section, revision, block_ids[], dangling_links[]}` |
| 6 | `update_block` | `path` · `block_id` · `content` · `expected_content_hash` · `message?` | `{path, revision, block_ids[], new_content_hash, dangling_links[]}` |
| 7 | `delete_note` | `path` · `expected_revision` · `message?` | `{path, revision, restorable: true}` |
| 8 | `restore_note` | `path` · `message?` | `{path, revision}` |

**D6.4 — The decisions inside those rows that are not obvious.**

*`read_note` returns one shape, always* — no mode with disjoint fields. `blocks[]` carries full block `content` alongside `content_hash`, because an excerpt is not a basis for a replacement: `search`'s excerpt is a 30-word `ts_headline` with `**` markers injected, and feeding it back to `update_block` would truncate the block and corrupt it with stray emphasis.

*The outline is server-computed, and this is where the audit's `outline()` can actually ship.* For heading rows, `section = heading_path ++ (attrs->>'text')` — precisely what `read_section` compares against, and precisely what content rows under that heading carry, which is what `append_to_section` compares against. One array that genuinely round-trips into both. Audit §11 records that the SQL fix was attempted, reverted, and blocked for a frozen reason: RFC-005 **X2** forbids the parser during reconstruction, so `blocks_as_of()` can never derive a heading's own text. The MCP server has the parse available for *live* reads and is therefore the only correct home for the derivation. **This RFC does not reopen X2.**

*`read_note(as_of)` is the history tool, and it returns text only.* Not a simplification — the only honest shape. `blocks_as_of` has exactly two overloads, `(path, bigint)` and `(path, uuid)`, with no `timestamptz` form, and neither returns `content_hash`. `read_as_of` *does* take `timestamptz`, so a wall-clock argument needs no revision-id discovery and therefore no `history` tool. `section` and `as_of` together are **refused**, not approximated: no `read_as_of` overload takes a heading path, and honouring the combination would mean a third implementation of "which heading delimits a section" in a product where §4.5 records the existing two already disagreeing.

*`write_note`'s guard is a binary with no wrong answer, and it needs no probe.* With `expected_revision` supplied it is replace-or-refuse (PM009 if the head moved, and PM009 too if the path has no live note — RFC-005 D5.6). Omitted, it is create-or-refuse (PM015 if a live note occupies the path). **There is no third value and no way to spell a blind overwrite.** Both derived fields fall out of the *arguments*: `created` = `expected_revision` was omitted, `unchanged` = returned revision equals the one supplied. So the tool is one function call with no pre-read and no TOCTOU. This does not reopen the alternative audit §10 rejected — that was *mandatory* `expected_head`, rejected because a first write has no head to supply; omission here is exactly that case, given its own safe meaning.

*`append_to_section` takes no guard, and that is the point.* RFC-005 D5.10: two concurrent appends serialize on the note row and both survive. A guard here manufactures the conflict the operation exists to avoid.

**D6.5 — Two structural admission rules, one of them found by measurement.**

The vault is kept out of the schema by construction rather than by review: **the server's SQL templates contain no vault argument at all**, so a grep over the template set is a complete audit of X1 and it holds even when a tool schema is edited by someone who never read this RFC.

`knowledge.vaults()`, `create_vault()` and `vault_id()` are never exposed, and the reason is stronger than X1's letter. `pgmind.vault` is the one table with no `vault_id` column, `enable_vault_rls` enumerates by `attname = 'vault_id'`, and so the registry is **structurally outside the RLS boundary**. Measured on PG 18.4: with `enable_vault_rls(force => true)` and a non-owner role scoped to one vault, `pgmind.note` returns zero rows for the others while `knowledge.vaults()` returns all of their *names*. Since names are where an application puts its tenant and user hierarchy, that is a tenant enumeration through a function that looks like a listing. Hence:

> **The exposure rule.** A `knowledge.*` function MAY be exposed as an MCP tool only if every table behind it appears in `enable_vault_rls`'s enumeration. This is mechanically checkable, and it catches a leak that "takes no vault argument" does not.

The one tenancy-crossing identifier on the surface is `update_block`'s bare `block_id`. It is defended in the extension — `load_ctx_by_block` raises PM003 when the block's vault differs from the session's — and is named here rather than passed over in silence.

### D7. One connection per (vault, author); session state is set at setup, never per call

The MCP server holds a connection per `(vault, author)` pair and sets `pgmind.vault_id` and `pgmind.author` **once, at connection setup**, from credentials it derived. It MUST NOT set either per tool call, and MUST NOT use `SET LOCAL` for either: one tool call is one autocommit statement, and `SET LOCAL` in autocommit is a measured no-op (audit §6.5) — it would silently do nothing, for every call.

One tool call is one SQL statement. Two consequences are normative rather than stylistic. Error payloads are built from the raised SQLSTATE and DETAIL plus a static code-keyed hint, never from a follow-up query (R4). And where a tool must report something the mutation produced — `new_content_hash`, `dangling_links` — the read is ordered inside the same statement with a `MATERIALIZED` CTE, not issued afterwards.

`pgmind.author` remains what RFC-011 D1 made it: an **assertion**, not an authenticated fact. The server asserts the agent identity it was configured with. This RFC adds no authentication of that claim and does not pretend to; what it adds is that the claim now has one obvious place to come from.

Transport, packaging and the config format are **not decided here**. The language is already pinned — product artifacts are Rust, and the crate lands in `tools/` converting the repo to a Cargo workspace (RFC-001 D8) — and distribution is RFC-012's.

### D8. Tenancy for 0.1.0 is a deployment pattern, and its limits are stated in both directions

Carried from the audit's §7 decision of 2026-08-10, unchanged, because it was costed and decided there:

1. The MCP server (or application) sets `pgmind.vault_id` per connection, from credentials it derived — never from anything the agent produced.
2. `SELECT pgmind.enable_vault_rls(force => true)` once per database.
3. The application role is not a superuser, holds no `BYPASSRLS`, and does not own the extension.

The argument that makes a `Userset` GUC acceptable here is not that it is secure — RFC-003 D1 says plainly it is a boundary "only when a trusted layer … owns the connection and tenants cannot issue arbitrary SQL." It is that **0.1.0's consumer is that layer**: an agent does not speak SQL, it calls tools, so the party who could abuse `SET` has no way to issue one. RLS is then real defence-in-depth against the failure that actually happens — a bug in the server, a forgotten predicate, a crafted path in a tool argument.

**Defends against:** a buggy server, a forgotten filter, a hostile tool argument, an agent that has learned another tenant's vault uuid.
**Does not defend against:** anything that can execute raw SQL on that connection, a superuser, `BYPASSRLS`, or — per D6.5 — enumeration of vault *names* through `knowledge.vaults()`.

The full fail-closed design (a `PGC_BACKEND` tenant GUC, RESTRICTIVE policies, registry-anchored resolution, per-user scope) is **deferred past 0.1.0** and becomes its own RFC. It was deferred on cost, not because it fails: measured on PG 18.4, a guessed real vault id from another tenant returns zero rows with no error and no oracle, and `SET pgmind.tenant` after connect raises `55P02`. What it implies is one connection pool per `(tenant, scope)` — thousands of pools at the stated topology — which is the same cardinality argument the design uses to reject role-per-vault, reimposed in pool form. Resolving that contradiction is research, not implementation.

### D9. What the pgmind MCP server is not

- **Not a place where a model runs.** RFC-000 D2 binds the MCP server by name. No embedding, no reranking, no query rewriting, no summarisation. Matching, ranking and excerpting are PostgreSQL's own.
- **Not a context assembler.** No tool packs, orders, budgets, deduplicates or scores across notes. `read_note` bundles one note, which is composition, not assembly. Any byte cap is a transport limit and MUST NOT become a token budget, select content, or reorder it. RFC-008 owns budgeting.
- **Not an authorisation layer.** It asserts an author and selects a vault; it authenticates neither. Authentication is the deployment's.
- **Not a second API.** Every tool is a thin wrapper over `knowledge.*`. Where a tool needs something SQL does not expose, the answer is a new `knowledge.*` function under D2.1 — not logic that exists only in the server. The one derivation the server owns is the outline's `section`, and only because X2 forbids the alternative (D6.4).
- **Not the only way in.** SQL remains the primary interface. An application that wants `move_note`, `diff`, `excise` or a cross-vault join uses SQL, and D3's `vault` parameter exists for exactly that.

### D10. Declared amendments to frozen and accepted RFCs

Precedence rules require these to be explicit rather than incidental:

- **RFC-001 D10 (frozen) — discharged.** The schema naming it proposed "ratified in RFC-007" is ratified by D1.
- **RFC-003 D1 (frozen) — discharged.** The identical naming deferral, and "RFC-007 owns per-session tenant/role selection for the MCP surface", are answered by D1, D7 and D8.
- **RFC-003 D4 (frozen) — amended.** D4 anticipated the FTS lane as "a generated tsvector column … an upgrade-script table rewrite, accepted pre-1.0 (RFC-012)". It ships instead as an expression GIN index over an `IMMUTABLE` function, for the reasons in D4 — chiefly that a stored `tsvector` column is content-derived and invisible to `excision.rs`'s type enumeration, which is the defect shape RFC-005 D7 was amended to close. **No table rewrite is required and RFC-012 inherits no migration for it.** The language-configuration decision D4 deferred here is made: `english`, fixed.
- **RFC-003 D7 (frozen) — discharged.** "RFC-007 freezes them for 0.x" is supplied by D2, which also defines what "additive" means.
- **RFC-005 D5.11 / D11 (frozen) — ratified.** `patch_block` is `update_block` in SQL and at the MCP layer. The plan's tool name does not survive; one operation has one name.
- **RFC-011 D2 (frozen) — discharged.** The `message` parameter it withheld until this freeze is added by D5.
- **Product plan §8 (lower precedence, recorded anyway) — amended.** The ten-tool sketch becomes eight, itemised in D6.1. §7.1's own rule — keep the table accurate or a deliverable goes unbuilt without anything noticing — is why the dispositions are enumerated rather than summarised.
- **Handbook §6.1 / §8 (higher precedence, deviation declared) — the five-tool list is a subset, not a contradiction.** Four of five ship verbatim; `context` is RFC-008's and becomes the ninth tool. Recorded here so the difference is a decision rather than a drift.

---

## 3. Alternatives considered

**A stored generated `tsvector` column,** as RFC-003 D4 anticipated. Rejected on three counts, the first decisive: it is a content-derived column of a type `excision.rs`'s enumeration cannot see, which reintroduces the exact defect RFC-005 D7 was amended to close — a positive erasure attestation over an unopened lane. It also costs a table rewrite to change configuration, where an index costs a `REINDEX`.

**A settable text-search configuration** (`pgmind.search_config`, or a per-vault column). Rejected: a generated column cannot host it at all, since the expression must be `IMMUTABLE`; and as a GUC the index and the query could disagree about which configuration built which row — a silently wrong answer, which is worse than a documented English bias.

**`to_tsquery` or `plainto_tsquery`** for the query parser. Rejected: `to_tsquery` raises on malformed input, and the input to this function is whatever a user or an agent typed.

**Returning every matching block, including containers.** Rejected on measurement: a list item's content includes its own paragraph, so a two-line vault returned each bullet twice and an agent pays for the same sentence twice.

**A ten-tool MCP surface**, per plan §8. Rejected against the audit's evidence that surface size degrades selection accuracy, and itemised in D6.1 — `backlinks` and `history` are fields on a call already made, `diff` needs arguments no tool emits, `get_context` is RFC-008's.

**A six-tool surface** with `edit` as one tool. Rejected: collapsing `append_to_section` and `update_block` deletes the only capability on the surface a file does not have, and requirement 7 is the one row where pgmind already beats the filesystem.

**A `force` / `overwrite` flag on `write_note`.** Rejected under R3: it is the branch that always works, advertised in the schema of the highest-blast-radius tool, and provably unnecessary because PM009's DETAIL already carries the current head, so the safe retry is one call away.

**Exposing `knowledge.vaults()` as a discovery tool.** Rejected on measurement (D6.5): the registry is outside the RLS boundary, so one call enumerates every tenant's vault names.

**Mandatory `expected_revision` on `write_note`.** Already rejected in audit §10 — a first write has no head to supply. D6.4 gives omission a safe meaning instead of making it impossible.

## 4. Consequences

*Easy after this.* An agent can be pointed at a vault and be useful without anyone writing SQL. Ranked search over a whole vault is one call and needs no materialized view, no refresh schedule and no embedding. Tag intersection is one call. A block edit carries its own conflict guard, and the guard arrives in the same response as the id it guards.

*Harder after this.* Every signature in `knowledge.*` is now load-bearing: adding a column to a `RETURNS TABLE` function needs a new function, not an `ALTER`. That is the cost of D2 and it is paid deliberately.

*Impossible without a new RFC.* Removing or renaming anything in `knowledge.*` before 1.0. Exposing the vault to a model. Putting a model inside the MCP server (RFC-000 D2, not this RFC's to reverse). A round-tripping `section_path` from `blocks_as_of()`, which RFC-005 X2 forbids and D6.4 explicitly declines to reopen.

*What a future RFC would have to do to reverse the search decision.* Drop `block_fts`, add a generated column to `pgmind.block`, ship an upgrade script, and either extend `excision.rs`'s type enumeration to `tsvector` or accept that `verify_excision` attests over a lane it does not read. The first three are mechanical; the fourth is why this RFC went the other way.

## 5. Benchmark gate

*No gate, no acceptance.* §5.0 of RFC-005 is normative for every suite here, including (b): every suite ships a negative control.

**`search-quality`** — in `eval/`, added to the Phase 5 set, and **shipped and green with this RFC's query half**. Suite id normative.

| assertion | why it can fail |
|---|---|
| a stemmed query matches the stored word | the configuration silently changed to `simple` |
| a phrase query finds the right note | positions were lost, so `ts_rank_cd` and phrase search both degrade |
| `path` excludes other notes; a frontmatter tag scopes its whole note | the filters were composed with `OR`, or the note-level tag rule was dropped |
| tags alone are a filter, not a failure | the text predicate became mandatory again and `tags` went half-dead |
| an unranked hit reports no rank rather than zero | a fabricated number a caller could sort by (R7) |
| no predicate returns nothing, not everything | an empty search box became "the whole vault" |
| no result contains another result | the innermost-match rule regressed and every bullet is billed twice |
| no query can make `search` raise | somebody swapped in `to_tsquery` |
| search stays in its vault; an unknown vault is PM018 | the vault predicate was dropped, which is how §4.1 happened |
| **negative control:** a block too big to index is still written, the unbounded expression on that same content still raises `54000`, and the block is still searchable | the bound stopped being load-bearing — or, worse, stopped being there, in which case indexing now narrows what pgmind stores |
| the planner reaches `block_fts` | the query and the index expression drifted apart, which costs correctness nothing and performance everything |

**`mcp-end-to-end`** — required before the MCP half is accepted, not before this RFC is. Walkthrough B scripted through the tool surface rather than through SQL: two agents appending to one section concurrently (both land), and a full-note rewrite that fails CAS loudly. Plus two the panel review demanded and this RFC adopts: that the post-write reads inside a `MATERIALIZED` CTE observe the mutation (`new_content_hash`, `dangling_links`), and a **selection-accuracy** measurement over the eight tool definitions — since the whole argument of D6 is that eight is small enough to choose from correctly, that claim is measured, published, and not gated on a threshold.

**`tenant-isolation`** — extended to `search` under an active RLS policy, per the plan's Phase 5 gate.

## 6. Law compliance

- **Law 1 (no AI in the product).** Nothing here invokes a model, in the extension or the server. D9 states it for the server explicitly, because RFC-000 D2 names the MCP server as bound by it and this is the RFC that creates one.
- **Law 2 (no network I/O from the extension).** Unchanged. The MCP server runs outside the database and reaches it over an ordinary client connection; the extension opens nothing.
- **Law 6 (compose with incumbents).** D4 is the clearest case in the project: matching, ranking and excerpting are PostgreSQL's own, and pgmind contributes vault scoping and the innermost-match rule. No ranking algorithm was written.
- **Law 7 (incremental maintenance).** The FTS lane is an index the write path maintains, not a materialized view with a refresh schedule — which is also what retires the unsafe cookbook recipe D4 describes.
- **Law 9 (feel like PostgreSQL).** `search()` takes `websearch_to_tsquery` syntax and returns `ts_rank_cd` and `ts_headline` output under their own semantics. A Postgres practitioner needs no pgmind-specific search vocabulary.
- **Law 11 (admin surfaces revoked).** Unchanged, and extended in spirit by D6.5's exposure rule: a function outside the RLS boundary may not be exposed to an agent even when it is not admin.
