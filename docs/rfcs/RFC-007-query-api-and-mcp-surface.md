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

`q` is `websearch_to_tsquery` syntax. That parser is chosen over `to_tsquery` because **it cannot be made to raise**: the input to a search box is whatever a user or an agent types, and a syntax error is not an answer. A query with no lexemes in it matches nothing and says so once, as Postgres's own `NOTICE`.

`tags` keeps only blocks carrying **every** tag listed, where a tag counts if it is on the block **or** on its note — so a frontmatter tag scopes the whole document, which is what a reader means by it. `path` narrows to one note in both functions and is `PM002` when the path has no live note: a tag nothing carries and a path you mistyped are different answers and must not look alike.

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

### D6. The MCP tool surface

*[To be completed from the design panel — the 5-vs-6-vs-10 argument and the final tool table.]*

### D7. The MCP connection model: one connection per (vault, author), state set at setup

*[To be completed.]*

### D8. Tenancy is a deployment pattern for 0.1.0, and its limits are stated in both directions

*[To be completed.]*

### D9. What the pgmind MCP server is not

*[To be completed.]*

### D10. Declared amendments to frozen and accepted RFCs

*[To be completed.]*

---

## 3. Alternatives considered

*[To be completed.]*

## 4. Consequences

*[To be completed.]*

## 5. Benchmark gate

*No gate, no acceptance.*

*[To be completed.]*

## 6. Law compliance

*[To be completed.]*
