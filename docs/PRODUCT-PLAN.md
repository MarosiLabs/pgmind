# pgmind Product Plan

Version: 1.0
Date: 2026-08-04
Status: living document — the operating blueprint for implementation. The [handbook](../PGMIND.md) is the constitution (vision, laws, philosophy); the [audit](../AUDIT.md) is the evidence base; this plan is *how the product works and how we build it*. Per-phase **RFCs are written and accepted before implementation of each phase begins** — this plan defines what each RFC must decide, not the decisions themselves (except where the handbook already made them).

---

# Part I — The Big Picture

## 1. Product statement

**pgmind turns PostgreSQL into the brain for AI agents**: an Obsidian-shaped knowledge vault — notes, wiki-links, backlinks, tags, sections, block references, full history — living in the database instead of on a filesystem, safe for many agents to read and write concurrently, queryable in SQL, and able to hand any agent exactly the context it needs with one deterministic call.

No AI is anywhere in the middle. pgmind never calls a model. It is the substrate AI consumes.

## 2. The three artifacts

pgmind ships as three pieces, all deterministic:

| Artifact | Runs | Purpose |
|---|---|---|
| **`pgmind` extension** | Inside PostgreSQL | The vault model: markdown type, block storage, identity, versioning, links/tags, retrieval, context assembly, token budgeting |
| **`pgmind` CLI** | Anywhere | Import/export/two-way sync between a real folder and the vault; admin utilities |
| **`pgmind-mcp` server** | Anywhere | Exposes the vault to agents as MCP tools (read/write/append/search/context/history) |

The extension is the product; the CLI is the migration path and the humans' bridge; the MCP server is the agents' front door.

## 3. The experience — how it should feel

These walkthroughs are the product spec in narrative form. If an implementation makes any of them clumsier than shown, the implementation is wrong.

### Walkthrough A — Day 1: bring your brain (5-minute quickstart)

```bash
$ pgmind import ./my-vault --db postgres://…
  imported 412 notes · 9,381 blocks · 1,204 links · 87 tags   (4.2s)
```

```sql
-- It's just SQL now.
SELECT path, title FROM knowledge.notes('projects/**');

SELECT * FROM knowledge.backlinks('projects/auth');      -- who points here?
SELECT * FROM knowledge.tagged('architecture');           -- everything #architecture
SELECT * FROM knowledge.orphans();                        -- notes nothing links to
```

```bash
$ pgmind export ./my-vault-copy      # and you can always leave
$ diff -r ./my-vault ./my-vault-copy # byte-faithful
```

### Walkthrough B — Many agents, one brain, no clobbering

Agent 1 and Agent 2 both learned something about the auth service, at the same time:

```sql
-- Agent 1: atomic append — the most common memory write, made safe
SELECT knowledge.append_to_section(
  'projects/auth', ARRAY['Decisions'],
  '- 2026-08-04: rate limiter moved to middleware (see [[incidents/2026-08-03]])');

-- Agent 2, simultaneously, same section — both appends land, no lost update.
```

```sql
-- Full-note rewrite uses compare-and-swap: fails loudly instead of clobbering
SELECT knowledge.write('projects/auth', $md$…$md$,
                       expected_head => 'rev_01J2…');
-- ERROR:  pgmind: head moved (now rev_01J3…) — re-read and retry
```

### Walkthrough C — Context, deterministically

```sql
SELECT knowledge.context(
    root         => 'projects/auth',
    token_budget => 12000
);
```

Returns markdown: the note, everything it transcludes, then linked notes by link-distance, priority, and recency, deduplicated, packed to ≤ 12,000 tokens, every included section carrying block-level citations (`path#^block @ revision`). Same vault state + same arguments ⇒ **byte-identical output**, every time. `knowledge.context_explain(...)` returns the manifest: which blocks made it in, why, and the token accounting.

### Walkthrough D — The brain remembers how it changed

```sql
SELECT * FROM knowledge.history('projects/auth');            -- every revision
SELECT knowledge.diff('projects/auth', 'rev_01J1…', 'rev_01J3…');
SELECT * FROM knowledge.blame('projects/auth');              -- per block: who, when, which revision
SELECT knowledge.read('projects/auth', at => 'rev_01J1…');   -- time travel
```

### Walkthrough E — Optional vector lane (yours, not ours — lands in Phase 6, after 0.1.0)

```sql
-- pgmind exposes what needs (re)embedding; YOUR pipeline fills it.
SELECT block_id, content_hash, content
FROM   knowledge.embedding_queue('my-model')      -- only changed blocks, ever
-- …your code embeds and writes back…
INSERT INTO pgmind.embedding_hook (block_id, content_hash, model, vector) VALUES …;

-- Retrieval blends the lane in when it exists — caller supplies the query vector:
SELECT * FROM knowledge.search('token rotation', query_embedding => $1);
```

## 4. The conceptual model

```
vault ─── the whole knowledge base in one database (RLS-partitionable)
 └── note ─── addressed by path ('projects/auth'), has title + frontmatter properties
      └── section ─── heading-delimited subtree (addressable by heading path)
           └── block ─── paragraph / list item / table / code block / …
                         (quotes and lists are containers; their children are blocks)
                         · permanent surrogate ID (minted on write)
                         · content hash (changes with content)
links: [[note]] · [[note#Section]] · [[note#^block]] · markdown links · transclusions (![[…]])
tags:  #tag inline and in frontmatter        properties: frontmatter key/values
history: every change = a new revision (append-only) · excision = audited erasure
```

Two identities per block, never conflated (handbook Law 5): the **surrogate ID** answers "is this the same block as yesterday?" (survives edits); the **content hash** answers "did it change?" (drives sync, dedup, and the embedding queue).

---

# Part II — How it works (system design)

Everything here is the *design intent* the per-phase RFCs refine into normative specs. Handbook §6 laws bind all of it.

## 5. Schemas and naming *(proposed default — ratified in RFC-007)*

- `knowledge` — the public API schema: stable, documented functions. What users touch.
- `pgmind` — internal storage schema: tables, admin, hooks. Direct access is possible (it's Postgres) but versioned only via the API contract.

## 6. Data model (expands handbook §6.3; normative home: RFC-003)

```sql
pgmind.note        (id, vault_id DEFAULT NULL,                         -- optional multi-tenant / RLS column
                    path text, title, properties jsonb,
                    head_revision, created_at, tombstoned_at,
                    UNIQUE (vault_id, path))                           -- path-uniqueness is per-vault
pgmind.block       (id uuid, note_id, kind, ord, heading_path text[],
                    content text, content_hash bytea, attrs jsonb)
pgmind.revision    (id, note_id, parent_revision, author, source,      -- 'api'|'sync'|'rebind'
                    message, created_at)                               -- append-only
pgmind.block_revision (block_id, revision_id, op,                      -- create|update|move|split|merge|delete
                    content_delta bytea | keyframe bool, confidence real)
pgmind.edge        (id, src_block, dst_path text,                      -- target as written; dangling links are first-class:
                    dst_note NULL when unresolved,                     -- resolved lazily when the note appears
                    dst_block, dst_heading,
                    kind,                                              -- wikilink|mdlink|transclusion|blockref
                    revision_id)
pgmind.tag         (note_id, block_id,                                 -- block_id NULL ⇒ frontmatter tag (note-level)
                    tag text)
pgmind.embedding_hook (block_id, content_hash, model text, vector,     -- pgvector type; user-populated
                    created_at)
pgmind.excision_log(id, target, reason, actor, created_at)             -- the audited eraser
```

Mechanics fixed by the handbook and audit: small per-block rows (never a monolithic AST datum), LZ4 TOAST, insert-only autovacuum tuning, delta chains with periodic keyframes (every N revisions, N tunable, default 20), history partitioning, capacity model published with RFC-003. **Path grammar is a first-class decision** (charset, case sensitivity, Unicode normalization — NFC canonical, since macOS filesystems emit NFD — length limits, glob semantics for `notes()`): specced in RFC-002 alongside wiki-link target resolution, table-enforced in RFC-003; the path↔filename mapping belongs to RFC-006.

## 7. The API surface v1 (normative home: RFC-007; block-op semantics: RFC-004/005; vector-lane items: RFC-009)

Grouped catalog — signatures indicative, RFCs finalize. The Phase 5 freeze (RFC-007) covers signature *shape*; 0.x changes thereafter are additive-only. Items marked ⁶ (`embedding_queue`, the `query_embedding` parameters) are Phase 6 additions whose semantics RFC-009 decides — absent (not no-ops) in 0.1.0:

**Read & navigate**
```
knowledge.read(path, at => revision DEFAULT NULL)             → markdown
knowledge.read_section(path, heading_path)                    → markdown
knowledge.notes(glob DEFAULT '**')                            → SETOF note
knowledge.blocks(path)                                        → SETOF block
knowledge.links(path) / knowledge.backlinks(path)             → SETOF edge+context
knowledge.orphans() / knowledge.tags() / knowledge.tagged(tag)→ SETOF …
```

**Write (the database-strength part)**
```
knowledge.write(path, markdown, expected_head DEFAULT NULL)   → revision   -- upsert; CAS when expected_head given
knowledge.append_to_section(path, heading_path, markdown)     → revision   -- atomic, append-serializable
knowledge.patch_block(block_id, markdown,
                      expected_hash DEFAULT NULL)             → revision   -- block-level CAS
knowledge.move(path, new_path) / knowledge.delete(path)       → revision   -- delete = tombstone revision
-- lower-level block ops with defined identity semantics (RFC-004):
knowledge.insert_block / update_block / move_block / split_block / merge_blocks
```

**Search & traverse (planner primitives — all public, Law 10)**
```
knowledge.search(query, tags DEFAULT NULL, properties DEFAULT NULL,
                 paths DEFAULT NULL, query_embedding⁶ DEFAULT NULL, limit …) → ranked results
knowledge.traverse(root, max_depth DEFAULT 2, kinds DEFAULT ALL)            → SETOF (note, distance, via)
knowledge.expand(block_ids uuid[])                                          → surrounding context blocks
```

**History**
```
knowledge.history(path) / knowledge.diff(path, rev_a, rev_b)
knowledge.blame(path) / knowledge.read(path, at => rev)
```

**Context (the headline)**
```
knowledge.context(root DEFAULT NULL, query DEFAULT NULL,
                  token_budget, pins DEFAULT NULL, max_depth DEFAULT 2,
                  query_embedding⁶ DEFAULT NULL, format DEFAULT 'markdown') → text
knowledge.context_explain(same args)                                        → jsonb manifest
```

**Admin**
```
knowledge.stats() · knowledge.excise(target, reason) · knowledge.embedding_queue⁶(model)
```

Error contract: conflicts raise typed errors (`pgmind_head_moved`, `pgmind_hash_moved`) with the current head in the detail — agents retry by re-reading, never by forcing.

## 8. The MCP surface (normative home: RFC-007)

Roughly ten tools, mirroring what an agent does with files today — plus what files can't do:

`read_note` · `write_note` (carries `expected_head` for CAS) · `append_to_section` · `patch_block` · `search` · `backlinks` · `get_context` (root/query + token_budget) · `history` · `diff` · `list_notes`

Design rules: tool descriptions teach the vault model in one sentence each; every mutating tool returns the new revision ID so agents can chain CAS writes; `get_context` returns the manifest alongside the markdown so agents can cite.

## 9. Context assembly — the deterministic algorithm (normative home: RFC-008)

The CLAUDE.md `@`-imports pattern, generalized. Guarantee: **same vault state + same arguments ⇒ byte-identical output.**

1. **Seed.** `root` note (or top FTS/tag hits when called query-first), plus `pins` argument, plus any note with property `pgmind-pin: true`.
2. **Closure.** Transclusions (`![[…]]`) always follow, cycle-safe. Wiki/md links BFS to `max_depth` (default 2).
3. **Priority (deterministic).** Tier order: pins → root → transclusions → linked at distance 1 → distance 2 … Ties: backlink count desc, then **recency** (latest revision timestamp desc — a property of vault state, so determinism holds), then path ascending. RFC-008 decides whether recency also enters the score as a decay factor and fixes its constants. Query-mode blends FTS rank (and vector rank iff `query_embedding` supplied — never computed by pgmind).
4. **Packing.** Greedy by priority at *section* granularity; a note's title+lead always precede its sections; local tokenizer counts (Law 2 — no network); when budget forces truncation, a marker names what was cut and why.
5. **Dedup.** Content-hash set — transcluded content appears once, at its highest-priority occurrence.
6. **Citations.** Block-level, inline: every included section carries `path#^block @ revision` anchors (the handbook's verifiable-after-edit commitment); the manifest (`context_explain`) records every block: included/cut, reason, tokens.

## 10. Concurrency semantics (normative home: RFC-005)

- **`write` with `expected_head`** — compare-and-swap on the note's head revision. Loud, typed failure; never last-writer-wins silently.
- **`append_to_section`** — serialized per (note, section) via a transaction-scoped advisory lock on `hash(note_id, heading_path)` (sections have no row of their own to lock); final lock granularity is an RFC-005 decision. Concurrent appends both land, ordered by commit order.
- **`patch_block` with `expected_hash`** — CAS at block granularity, so two agents editing *different* paragraphs of one note never conflict at all.
- **Sync vs API races** — the sync bridge's state file supplies the three-way merge base (§12), while `source` provenance marks sync-originated revisions for downstream consumers; the API always wins the current transaction, sync rebases.

This section is the core "why not files" payoff — files give none of these.

## 11. Identity & rebinding (normative home: RFC-004; the project's #1 research problem)

Identity is minted by the write path (Law 4). When a whole document arrives from outside (import, sync, `write` without block ops), the **rebinding pipeline** matches new content to existing block IDs:

*Shipped 2026-08-06; the stage list below is what RFC-004 Part B accepted after corpus tuning, which is shorter than the four stages this section originally sketched.*

- **Stage 0 — deterministic (RFC-004 A3):** `^id` claims, then exact content match, section-first then by document order. No threshold, no score, no confidence recorded.
- **Stage 1 — the heuristic, all of it:** unmatched old/new blocks aligned by similarity (Dice over unigrams ∪ bigrams, same-kind, order-monotonic, τ = 0.5) ⇒ ID carried, confidence = score. Split detection is a *candidate filter inside this stage*, not a stage after it: a fragment resembles its parent, so an aligner left to itself gives the ID to whichever fragment scores highest instead of the first one, which is what A2 says gets it.
- ~~Split/merge as a separate stage~~ — **dropped, measured.** Splits moved into stage 1 (above); merges needed nothing, because a merged block is overwhelmingly similar to its dominant source and stage 1 already binds it.
- **Stage 2 — everything else:** new IDs; unmatched old blocks tombstone. Also the fallback when the residual exceeds the alignment budget.

Every rebind records confidence and `bind='rebind'` — downstream consumers (blame, citations, the embedding queue) can see exactly where identity is inferred rather than known. The adversarial edit corpus (eval/) publishes the match-rate honestly, tracked over time. Optional escape hatch: serialized `^block-id` markers for users who want deterministic round-trips through external editors.

## 12. Sync bridge (normative home: RFC-006)

- `pgmind import DIR` / `export DIR` / `sync DIR [--watch]`.
- Local state file (`.pgmind/state`) records per file: note ID, head revision, content hash at last sync ⇒ classic three-way merge: local-only change → push (through rebinding); remote-only → pull; both → conflict, default **fail with a list** (`--strategy ours|theirs|fork` opt-in; `fork` writes `name.conflict-<rev>.md`).
- `.pgmindignore` (gitignore syntax). Git-friendly: sync never touches `.git`, and exported files are stable so diffs stay clean.
- Import is idempotent: re-importing an unchanged vault is a no-op (hash short-circuit).

## 13. Security & multi-tenancy (normative home: RFC-003 + RFC-007)

- All API functions `SECURITY INVOKER`; storage tables carry an optional `vault_id` column with documented RLS policies — multi-tenant brains out of the box, one policy per tenant pattern in the docs.
- Excision is a privileged, audited operation (`pgmind.excision_log`); retention policies are declarative.
- No network egress exists anywhere in the extension, which is most of the security story (Law 1/2).

## 14. Performance targets & capacity model (validated with RFC-003; published in eval/)

Design targets (to validate, not promises): vaults to **100k notes / 10M blocks / 100 avg revisions per note** on a single node; `read` in single-digit ms; `backlinks`/`tagged` index-only; `context()` p95 < 250 ms at 12k budget on the reference vault; initial import ≥ 2k notes/s. The capacity model (blocks × revisions × edges, keyframe cadence, partition scheme) ships as a spreadsheet-with-benchmarks, not vibes.

---

# Part III — Delivery plan

## 15. Phase overview

| Phase | Name | RFCs first | Ships | Effort band |
|---|---|---|---|---|
| 0 | Groundwork | 000, 001 | eval harness, CI matrix | S (weeks) |
| 1 | Markdown type & parser | 002 | `markdown` type, AST fns, round-trip | M |
| 2 | The vault model | 003, 004 (write-path part accepted) | notes/blocks/links/tags in SQL | L |
| 3 | Versioning & concurrency | 004 (final), 005, 011 | history, CAS, append, rebinding, excision | L (the hard one) |
| 4 | Sync bridge | 006 | import/export/sync CLI, quickstart | M |
| 5 | MCP + context ⇒ **pgmind 0.1.0** | 007, 008, 012 | MCP server, `context()` v1, packaging | M |
| 6 | Optional vector lane | 009 | embedding hooks + queue, blended search | S |
| 7 | Retrieval & context maturation | 010, 008-rev | planner, fusion, explain, BM25 adapter | M/L |

Rules binding all phases (handbook §8/§9): build order parser → storage → indexes → planner; an RFC is accepted before its phase's implementation starts; a phase exits only when its benchmark gate passes *and is published*; the quickstart must pass continuously from Phase 4 on. Effort bands deliberately avoid dates — the honest calibration is in the handbook (comparable extensions: 4-6 years to maturity; phases 0-5 ≈ 12-18 months for a small team).

## 16. Phase details

### Phase 0 — Groundwork
**Goal:** every later phase has a benchmark to pass and a platform to build on.
**RFCs:** RFC-000 Vision & Scope (condenses handbook §1-5 into the accepted baseline); RFC-001 Implementation Platform (ratifies pgrx/comrak/CommonMark+GFM/PG16+/PostgreSQL-license decisions with rationale).
**Deliverables:** repo scaffolding per handbook §12 layout; pgrx skeleton building on PG16/17/18 in CI; eval/ harness skeleton with the first corpus (CommonMark spec suite); RFC template & index live in docs/rfcs/.
**Gate** (stated identically in RFC-000 §5 and RFC-001 §5): (a) skeleton builds and tests pass via `cargo pgrx test` on PG16/17/18 in CI; (b) `make eval` runs end-to-end and emits `eval/results/latest.json` (suites may report *pending*); (c) `make lint` clean, run in the pg18 CI leg.

### Phase 1 — Markdown type & parser
**Goal:** markdown becomes a typed, structurally queryable value with a byte-faithful boundary.
**RFC-002 must decide:** exact block taxonomy (what is a block); wiki-link/tag/block-ref/transclusion syntax (Obsidian-compatible subset — what we accept, what we normalize); frontmatter handling; sourcepos representation; how content hashes are computed (normalization rules — this quietly determines rebinding quality).
**Deliverables:** `markdown` type (parse/validate/serialize); AST access functions; per-block hashes; internal conformance renderer (no public HTML render function — RFC-002 D9).
**Gate:** CommonMark conformance suite; byte-faithful round-trip on the corpus (including gnarly real vaults); property-based tests (parse∘serialize = id).
**Risks:** Obsidian syntax has no spec — RFC-002 defines *our* spec explicitly rather than chasing bug-compat.

### Phase 2 — The vault model
**Goal:** an Obsidian-shaped vault lives in Postgres and answers questions no folder can.
**RFC-003 must decide:** final table shapes (§6), index strategy, vault/RLS column, path grammar enforcement (charset, case, NFC normalization, length, glob semantics — specced with RFC-002's link-target resolution), dangling-link representation and how `orphans()`/`backlinks()` treat unresolved targets, frontmatter-tag modeling (note-level vs block-level), capacity model with published math. **RFC-004** — its *write-path identity* sections (what each block op does to IDs) must be **accepted (living)** before Phase 2 implementation; its rebinding sections remain draft until Phase 3 acceptance.
**Deliverables:** storage schema; write path with ID minting; deterministic extraction of links/tags/properties into edge/tag tables at write time (incremental, Law 7); read/navigate API group incl. `read_section` and `stats()`; backlinks/orphans/tagged; the documented one-policy-per-tenant RLS pattern.
**Gate:** identity-semantics suite (split/merge/move/copy); extraction-correctness corpus; **tenant-isolation suite** (storage-level: no cross-`vault_id` reads under an active RLS policy); capacity model v1 published.

### Phase 3 — Versioning & agent-safe concurrency  *(the hard one — schedule accordingly)*
**Goal:** history for everything; many writers, zero silent clobbers; honest rebinding.
**RFCs:** RFC-004 (final — rebinding pipeline, thresholds, split/merge policy); RFC-005 (revisions, delta/keyframe format, CAS + append semantics, excision mechanics); RFC-011 (provenance model: authors, sources, confidence).
**Deliverables:** revision engine (append-only, deltas + keyframes); history/diff/blame/as-of; `write` CAS, `append_to_section`, `patch_block`; `move`/`delete` (tombstone revisions); rebinding pipeline; excision + retention + audit log.
**Gate:** adversarial edit corpus with published match-rate (the number is the deliverable — start honest, improve); concurrency suite (CAS conflicts, concurrent appends, interleaved rebind-source (`source='sync'`) and API writes — true bridge interleaving lands in Phase 4's torture suite); storage-growth benchmark under revision load.
**Risks:** rebinding quality (mitigation: corpus-driven iteration, `^id` escape hatch); delta-chain read cost (mitigation: keyframe cadence tuning in the benchmark).

### Phase 4 — Sync bridge
**Goal:** one-command migration in; one-command exit; humans keep their editors.
**RFC-006 must decide:** state-file format, three-way merge semantics, conflict strategies, ignore rules, watch mode, git interaction, freshness metadata, and the path↔filename mapping (`.md` extension handling, Windows-illegal characters, case-insensitive-filesystem and NFC/NFD normalization collisions on export).
**Deliverables:** `pgmind import/export/sync [--watch]`; `.pgmindignore`; freshness metadata on synced notes; the **5-minute quickstart**, tested in CI from here forever.
**Gate:** round-trip fidelity import→export (incl. Unicode-normalization and case-collision cases); sync torture suite (rename storms, concurrent edits both sides, partial failures); quickstart passes on a clean machine.

### Phase 5 — MCP + deterministic context ⇒ **first public release: pgmind 0.1.0**
**Goal:** the headline demo end-to-end, zero AI configured.
**RFCs:** RFC-007 (API/MCP surface freeze for 0.x — additive-only thereafter; MCP connection/auth model; per-agent `author` attribution flowing into revision provenance, coordinated with RFC-011; per-session tenant/role selection interacting with the §13 RLS design; ratifies §5 schema naming); RFC-008 (context assembly algorithm §9 — every constant, order, tie-break, and the recency factor normative); RFC-012 (packaging: PGXN v2, OCI, Docker, versioning policy, **and the extension upgrade path** — `ALTER EXTENSION … UPDATE` scripts, storage-schema migrations between versions, downgrade policy).
**Deliverables:** `context()` + `context_explain()`; search (FTS/tags/properties); traverse/expand; `pgmind-mcp` with the ten tools; packaging + install docs.
**Gate:** context determinism test (byte-identical across runs/replicas); quality-per-token vs "cat the folder" and naive top-k FTS baselines, published; MCP end-to-end scenario test (Walkthrough B scripted); tenant-isolation suite extended to `context()`/`search`/`traverse` under an active RLS policy; `pg_dump`/`pg_restore` round-trip of a reference vault (+ `pg_upgrade` check in CI).
**Release criteria for 0.1.0:** all prior gates green; quickstart < 5 minutes measured; docs cover every public function; LICENSE file present (PostgreSQL — decided, handbook §7); name decision resolved **[OPEN until here at the latest]**.

### Phase 6 — Optional vector lane
**Goal:** RAG-style recall for users who bring their own embeddings — without pgmind ever calling a model.
**RFC-009 must decide:** hook-table contract, `embedding_queue` semantics (content-hash keying so unchanged blocks are never re-embedded), how blended ranking composes with FTS (weights, normalization), pgvector version matrix.
**Deliverables:** hook tables + queue view; `search`/`context` blending when `query_embedding` supplied; two documented recipes (external batch pipeline; Supabase-style trigger pipeline) — recipes, not components.
**Gate:** blended-retrieval benchmark vs FTS-only on the reference vault; queue correctness under edit storms.

### Phase 7 — Retrieval & context maturation
**Goal:** `context()` becomes trustworthy as the default retrieval surface, with inspectable decisions.
**RFCs:** RFC-010 (retrieval planner: intent → strategy selection; fusion incl. RRF; BM25 adapter decision); RFC-008 revision (packing v2: compression strategies, ordering experiments — deterministic only).
**Deliverables:** planner with `EXPLAIN`-style output; fusion across FTS/tags/links/vector-lane; published tradeoff curves (tokens vs answer quality on public benchmarks).
**Gate:** planner beats Phase 5 naive assembly on the published benchmark at equal budgets; explain output covers 100% of inclusion decisions.

### Beyond (Future Work — quarantined per handbook)
External enrichment recipes (summaries/entities via user pipelines), instruction-driven AI editing through the write API, conversational memory layer, distributed knowledge. Each requires its own RFC and a handbook amendment to leave quarantine.

---

# Part IV — Process, measures, governance

## 17. The RFC discipline (because it should be a work of art)

- Template and index live in [docs/rfcs/](rfcs/) — every RFC follows the template: Context → Decision → Alternatives considered → Consequences → Benchmark gate → Law-compliance statement (which Architecture Laws it touches and how).
- Lifecycle (handbook §12): living during its phase, frozen at phase exit; amendments get a new RFC.
- An RFC is *accepted* when the owner signs it **and its benchmark gate is defined** — no gate, no acceptance (audit M7).
- Writing quality bar: an RFC should be readable by a Postgres practitioner who has never seen pgmind, and every "must" traceable to a law, an audit finding, or a benchmark.

## 18. Success measures

- **Per phase:** the gates above — published, including unflattering numbers (the rebinding match-rate especially).
- **Product (0.1.0):** a stranger completes the quickstart in under 5 minutes; Walkthrough A-D each work as written.
- **Adoption tier (if pursued):** a real agent product swaps file-based memory for pgmind in under an hour with no behavior loss; first external production deployment; first managed-platform allowlisting.

## 19. Change control

- This plan changes by PR + owner sign-off; changes that touch handbook laws/philosophy require a handbook amendment first.
- Conflicts resolve in this order: **handbook laws > accepted RFCs > this plan > code comments**.
- The audit is never edited for content (it is evidence); new evidence gets an addendum.
