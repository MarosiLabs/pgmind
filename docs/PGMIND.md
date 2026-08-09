# pgmind — A Brain for AI Agents, Inside PostgreSQL

## Project Handbook
Version: 0.3
Status: working document — decisions marked **[DECIDED]** are revisitable defaults with rationale; items marked **[OPEN]** need resolution in the named RFC.

> v0.1 ([archived](archive/PGMIND-v0.1.md)) was audited against 2025-26 evidence — see [AUDIT.md](archive/AUDIT.md). v0.2 resolved the audit findings. **v0.3 sharpens the vision per the author's direction: pgmind is the knowledge base and memory substrate — the brain — for AI agents in server backends, replacing markdown-files-on-the-filesystem. The core is strictly AI-free.** This direction *strengthens* every audit resolution: the audit's central finding was that deterministic cores survive and in-database AI dies.

---

# 1. The problem

Today, the de facto knowledge substrate for AI agents is **markdown files on a filesystem**: Claude Code memory files, agent memory directories, Obsidian vaults, `docs/` folders fed to LLMs. This works beautifully for a local, single-user, offline program — and falls apart the moment the agent lives in a server backend:

| Filesystem markdown | The server-backend reality |
|---|---|
| One user, one writer | Many agents/requests writing concurrently → races, lost updates, no transactions |
| "Find related notes" = the agent greps | No queries: backlinks, tags, "what mentions X" all require scanning or bolt-on index glue |
| History = git, maybe | No revision history in the write path; git is manual and heavyweight inside a server |
| The vault is one user's disk | No multi-tenancy, no access control, no row-level security |
| Files + a separate vector DB | Knowledge and its indexes drift apart; nothing is atomic |
| Backup = another system | Ops burden the team's database already solved |

Meanwhile the team almost certainly already runs PostgreSQL. **pgmind teaches that PostgreSQL to be the brain**: an Obsidian-shaped knowledge base — notes, wiki-links, backlinks, tags, sections, block references, history — living in the database instead of on a disk, safe for many agents to read and write at once, queryable in SQL.

## The rule that defines the product

**No AI is in the middle. Ever.** pgmind never calls a model, never embeds, never summarizes. It is a deterministic knowledge substrate that AI *consumes*. Vectorization for RAG stays where it already lives — pgvector, populated by the user however they like — as an **optional lane** pgmind's retrieval can use when present, never a dependency and never the point.

This is not just the author's taste; it is the strongest finding of the audit: every project that put AI inside the Postgres extension died or retreated (pgrag archived, PostgresML/Korvus bust, pgai forced out of the extension — [AUDIT.md §4](archive/AUDIT.md)). Deterministic primitives are what survive.

## The one-line goal

```sql
SELECT knowledge.context(
    root         => 'projects/auth',
    token_budget => 12000
);
```

Deterministic, budgeted context assembly: the note, what it transcludes, what it links to — by link distance, pinning, and recency — packed to a token budget with block-level citations. Exactly the pattern Claude Code uses with `CLAUDE.md` and `@`-imports, generalized and made transactional. No embeddings required; vectors join the party only if the user populated the optional lane.

---

# 2. Users & use cases

**Primary persona:** the developer of a server-side AI application or agent system — someone who today would model agent memory as markdown files and knows that won't survive production. Secondary: teams wanting an Obsidian-like shared knowledge base that agents and humans both use, without it living on one person's disk.

**Primary consumer of the API: agents, via MCP** (and application code via SQL).

Scenarios the roadmap must serve end-to-end:

1. **The shared agent brain (the headline).** A backend runs many agent instances; they read and write one knowledge base concurrently — project decisions, learned facts, task state, conventions. Writes are transactional (no clobbered memory), reads are queryable ("everything tagged `#architecture` touched this week"), and every change has history.
2. **Claude-Code-style memory, server-side.** The exact filesystem-memory pattern (memory files, imports, topic notes) relocated into Postgres for a hosted product — same mental model, none of the filesystem drawbacks.
3. **Obsidian-not-local.** A team knowledge vault with wiki-links, backlinks, tags, and block references — in the database, with RLS for access control, agents as first-class readers/writers. Humans who want their local editor export a folder and work in it; pgmind is not trying to be a live mirror of one person's disk (§11).
4. **SQL-joined knowledge.** Context assembly filtered by operational data in the same database (`WHERE customer_id = …`) — impossible for filesystem vaults and managed RAG APIs alike.
5. **Auditable knowledge.** Block-level history, diff, and blame; answers cite block IDs at specific revisions, verifiable after the knowledge has changed.

---

# 3. Scope & non-goals

**In scope (v1):**
- Markdown-native vault model: notes with paths/titles, CommonMark + GFM, wiki-links `[[note]]` / `[[note#section]]` / block refs, tags, frontmatter properties, transclusion.
- Per-block storage with stable identity; append-only versioning with audited excision.
- Agent-safe write operations (compare-and-swap, append-to-section, block-level patch).
- Deterministic retrieval and context assembly (links, structure, tags, FTS, recency) with token budgeting.
- MCP server. *(A filesystem/git two-way sync bridge was in scope through 2026-08-09 and was cut; §11 records why.)*
- **Optional vector lane:** schema hooks for pgvector embeddings the *user* populates; retrieval uses them when present.

**Non-goals (v1) — explicit, so scope creep has to argue with this list:**
- **Any model execution, anywhere in the product.** No embedding generation, no summaries, no entity extraction, no LLM calls — not even in a companion process. External enrichment is the user's business; pgmind gives it clean hooks (content-hash-keyed tables) and nothing more.
- **PDF/DOCX/HTML parsing.** External converters produce markdown; pgmind ingests markdown.
- **Conversational/episodic fact memory** (Mem0/Zep-style temporal facts and contradiction resolution). The block model could host it later — see Future Work.
- **Vector index engines, BM25 engines.** pgvector and existing FTS own those layers.
- **A general document store / collaborative CMS.** Notion/Outline territory — collaborative editing UX, rich media, publishing. pgmind stores knowledge for agents and retrieval, not a wiki product.
- **Distributed/federated knowledge.** Cut (audit M3); see Future Work.
- **A hosted service.** pgmind is software you run.

---

# 4. Design principles: why it will feel natural

The adoption question is "why would people choose this over a folder of md files?" The answer has to be *zero conceptual tax, strictly more capability*:

1. **The vault model, not the database model.** The API's nouns are the ones agent developers already think in: notes, paths, sections, links, tags, blocks. `knowledge.read('projects/auth')` returns markdown. Nobody needs to learn a "knowledge object" ontology to write a note.
2. **Markdown in, markdown out — always.** Every note round-trips as plain markdown, byte-faithfully. Any editor, any diff tool, any LLM prompt template that speaks markdown works unchanged. The database is invisible until you want it.
3. **File-shaped reads, database-strength writes.** Reading feels like reading files. Writing gives what files never could: transactions, compare-and-swap on revision (two agents can't silently clobber each other's memory), `append_to_section` (the single most common agent memory operation, made atomic), block-addressed patches.
4. **No lock-in, ever: the vault is always exportable.** Every note is markdown at a path, stored in ordinary tables; `knowledge.read()` returns the source bytes exactly, and `pg_dump` is a complete backup. Leaving is a folder of `.md` files produced by [`scripts/export-vault.sh`](../scripts/export-vault.sh), arriving is [`scripts/import-vault.sh`](../scripts/import-vault.sh), and the round trip is a gate (`folder-round-trip`) over a corpus of paths chosen to break it — not a claim. People trust a brain they can walk away with. *(pgmind deliberately ships no two-way sync daemon; see §11.)*
5. **Queries you couldn't ask a folder.** Backlinks, orphans, tag intersections, "notes linking to this block," link-distance neighborhoods, history/blame — one SQL call or MCP tool each. This is where "why bother" turns into "oh."
6. **Deterministic context assembly.** `context()` walks pins, transclusions, and links — explainable, reproducible, no embeddings needed. The CLAUDE.md-imports pattern, generalized.
7. **Vectors are a lane, not a lane change.** If the user populates the embedding hooks (pgvector), search and `context()` blend that signal in. If not, everything works. pgmind never generates an embedding.

---

# 5. Philosophy (revised where the audit falsified it)

**Knowledge is the source of truth.** Not files, not vectors, not prompts. *(unchanged from v0.1)*

**Markdown is a boundary, not storage.** Markdown is the serialization format at the edges; internally, knowledge lives as per-block rows. The parser emits structure, positions, and content hashes — the write path assigns identity. *(audit C3)*

**Identity is minted on write, kept by policy.** Every block has a permanent surrogate ID assigned when it is created through the write path. When whole documents are replaced from outside (a re-import, or any `write()` of a full note), IDs are *re-bound* heuristically with explicit confidence semantics — never silently. Split/merge/move each have defined rules (RFC-004). We do not claim what no system on earth delivers: deterministic identity recovered from plain-text diffing. *(audit C1)*

**Append-only by default; forgetting is a feature, audited.** Updates create revisions; history is the default. Excision (legal erasure, retention, compaction) is a first-class, audited operation — every shipped immutable database (Datomic, Dolt, XTDB) learned this; we start with it. *(audit C4)*

**AI is a consumer, never a component.** LLMs read from and write to the brain through the same deterministic APIs as everyone else — with provenance. Nothing inside pgmind invokes a model. *(strengthened in v0.3; audit C2)*

**Context is the product.** SQL returns rows; pgmind returns context — assembled, deduplicated, ordered, budgeted, cited. *(unchanged)*

**Semantic APIs, deterministic bones.** Callers declare intent; the planner chooses among deterministic strategies (and the optional vector lane when present). Every primitive the planner uses — search, traverse, expand, follow-references — is a public, individually-callable SQL function, because agents increasingly retrieve iteratively and debuggability beats magic. *(audit M11)*

---

# 6. Architecture

## 6.1 Shape

```
   Humans ── editors / Obsidian ──┐
                                  │ export / import (scripts/)
   Applications ───── SQL ────┐   │        ┌──── MCP ──── Agents
                              │   │        │
        ┌─────────────────────▼───▼────────▼─────────────────────┐
        │            pgmind extension — DETERMINISTIC, AI-FREE   │
        │                                                        │
        │  Knowledge API   read() · write() · cas_write() ·      │
        │                  append_to_section() · patch_block() · │
        │                  search() · backlinks() · traverse() · │
        │                  context() · history() · diff()        │
        │  Planner         retrieval planning · context assembly │
        │                  · token budgeting (local tokenizer)   │
        │  Vault model     notes · sections · blocks · links ·   │
        │                  tags · properties · revisions ·       │
        │                  provenance                            │
        │  Storage         per-block rows · append-only          │
        │                  revisions · edge tables               │
        │  Markdown        parse ⇄ serialize ⇄ validate          │
        │                  (boundary)                            │
        └────────────────────────┬───────────────────────────────┘
                                 │ PostgreSQL · FTS · [pgvector]
                                 │
             optional lane: embedding hook tables keyed by
             (block_id, content_hash) — populated by THE USER's
             own pipeline, never by pgmind
```

Companion tools (the import/export scripts, MCP server) run outside the database and are equally deterministic. There is no pgmind process that calls a model.

## 6.2 Architecture laws

Constraints, not preferences. An RFC that violates one must say so in its title.

1. **AI-free core.** Nothing in pgmind — extension or tools — executes or invokes a model. External enrichment gets hooks, not a home.
2. **No synchronous network I/O inside any transaction or API call.** `context()` and `search()` never phone anywhere; the optional vector lane is read from local tables, and query-time vectors are client-supplied.
3. **Markdown is a boundary.** The system of record is per-block relational rows. A monolithic AST datum as storage is prohibited (JSONB/TOAST pathology — audit C3).
4. **Parsing yields structure, hashes, and positions — never identity.** Identity is minted by the write path and re-bound by documented policy.
5. **Surrogate IDs and content hashes are different things, and we keep both.** IDs give identity across edits; hashes give dedup and change detection (and key the optional embedding hooks so users never re-embed unchanged blocks).
6. **Compose with incumbents.** pgvector for vectors (optional), Postgres FTS baseline for text search, external converters for document formats. Novelty budget goes to the vault model, concurrency semantics, and planners only.
7. **Incremental maintenance only.** Link graph, tag index, FTS, and hook tables update from block diffs — no operation requires a full rebuild on document update.
8. **Append-only with audited excision.** Revisions are inserts; excision is explicit, logged, and policy-driven.
9. **Feel like PostgreSQL — and like a vault.** SQL idioms, no hidden behavior; API nouns are notes/links/tags/blocks, not ontology jargon.
10. **`context()` is a mode, not a monopoly.** Every planner primitive is public and individually callable.
11. **Layers by contract, not by wall.** Public APIs depend only on documented APIs of lower layers; admin/debug interfaces may reach deeper and must be marked as such. *(replaces v0.1's "no layer bypasses another")*

## 6.3 Storage sketch (normative for RFC-003)

- `note(id, path, title, properties jsonb, head_revision, …)` — the vault namespace
- `block(id uuid, note_id, kind, content, content_hash, attrs, …)` — small rows, LZ4 TOAST, insert-only autovacuum tuning
- `block_revision(block_id, revision_id, content_delta | keyframe, …)` — delta chains with periodic keyframes (MediaWiki precedent, ~98% compression); history tables partitioned
- `revision(id, note_id, parent, author, source, created_at, …)` — append-only; `source` records how the write arrived (`'api'` today; `'sync'` and `'rebind'` are legacy CHECK values nothing can set, retired by RFC-012)
- `edge(src_block, dst_note | dst_block, kind, …)` — wiki-links, md links, transclusions, block refs; **native edge tables, traversed with recursive CTEs [DECIDED]** — the v0.2 graph-backend question dissolves because the core graph is deterministic links only (no semantic graph engine needed); PG18 SQL/PGQ tracked for query ergonomics
- `tag(block_id, tag)` / properties from frontmatter — extracted at write time
- `embedding_hook(block_id, content_hash, model, vector, …)` — **optional**, pgvector-typed, populated only by the user
- **Capacity model is a deliverable:** blocks × revisions × edges; Notion needed 96-server sharding for block rows — RFC-003 publishes single-node sizing guidance.

---

# 7. Technology decisions

| Decision | Choice | Rationale | Status |
|---|---|---|---|
| Extension language | **Rust / pgrx** | Proven by pg_search, pgvectorscale, pg_graphql; memory safety for a parser-heavy extension; accepted costs: pre-1.0 churn, per-PG-major builds | **[DECIDED]** |
| Markdown parser | **comrak** | Production CommonMark+GFM in Rust with sourcepos; wiki-link/block-ref syntax as a documented extension on top | **[DECIDED]** |
| Spec anchor | **CommonMark + GFM subset** (tables, task lists, strikethrough) + wiki-link/tag/block-ref extensions specced in RFC-002 | Determines the block taxonomy | **[DECIDED]** |
| Link graph | Native edge tables + recursive CTEs | See §6.3 | **[DECIDED]** |
| Tokenizer (budgeting) | Local, in-extension; cl100k/o200k-class BPE, pluggable | Budgeting must not require network calls (Law 2) | **[DECIDED]** |
| PG version matrix | **PG16+** | pgrx support window | **[DECIDED]** |
| License | **PostgreSQL license** | The only choice consistent with allowlisting evidence (audit §3.3) | **[DECIDED]** |
| BM25 adapter | tsvector baseline now; optional adapter (pg_textsearch is license-compatible) later | Optional-lane philosophy, same as vectors | **[OPEN — RFC-010]** |
| Name | **pgmind** (working) — unclaimed on PGXN/crates.io; MindsDB adjacency noted as trademark risk | Register org/PGXN/crates names before first public release | **[OPEN]** |

---

# 8. Roadmap

Every phase states **what a user can newly do** — a phase with no user-visible sentence doesn't ship. The first public release lands at Phase 5. Build order (kept from v0.1): parser → storage → indexes → planner; and in v0.3 there is no "AI last" — there is no AI.

**Timeline honesty:** comparable extensions took 4-6+ years to maturity with funded teams (PostGIS 4yr to 1.0, Citus 6yr, TimescaleDB ~4yr). Dropping the AI/worker scope makes this program materially smaller than v0.2's; phases 0-5 remain a realistic 12-18 month program for a small team. Treat faster estimates as a red flag in review.

### Phase 0 — Groundwork
RFC-000/001, eval-harness skeleton, CI matrix per PG version.
*User can:* nothing yet — this phase exists so every later phase has a benchmark to pass.

### Phase 1 — Markdown type & parser
`markdown` boundary type (parse/validate/serialize), AST access functions, sourcepos, per-block content hashes; wiki-link/tag/block-ref syntax extensions.
*User can:* store validated markdown; query document structure in SQL; round-trip byte-faithfully.
*Benchmark:* CommonMark conformance; round-trip fidelity on a public corpus.

### Phase 2 — The vault model
Notes with paths; per-block storage; surrogate IDs minted by the write path (`insert_block`, `update_block`, `move_block`, `split_block`, `merge_blocks` — each with defined identity semantics); deterministic extraction of links, tags, and properties into edge/tag tables; backlinks.
*User can:* create an Obsidian-shaped vault in Postgres; query backlinks, orphans, and tag intersections in SQL.
*Benchmark:* identity-semantics suite (split/merge/move/copy cases from RFC-004); extraction correctness corpus.

### Phase 3 — Versioning & agent-safe concurrency
Append-only revisions; delta chains + keyframes; history/diff/blame; **compare-and-swap writes, `append_to_section`, block-level patch** (the concurrency semantics files can't give); heuristic rebinding for whole-document replacement (confidence-scored, provenance-marked); excision & retention (audited).
*User can:* run many agents against one brain without lost updates; see block-level history; legally erase.
*Benchmark:* published adversarial edit corpus with rebinding match-rate targets (the project's #1 research problem — experimental track with measured progress); concurrency test suite.

### Phase 4 — **cut 2026-08-09**
Was: a two-way filesystem/git sync bridge (`pgmind sync --watch`). Removed after measurement; §11 has the argument, [RFC-006](rfcs/RFC-006-sync-bridge-and-import-export.md) the record. What the law-4 promise actually needed — byte-exact export and import — ships as gated shell scripts instead, and landed with Phase 3. Phase numbering is unchanged: Phase 5 follows Phase 3, and the published Phase 0-3 gates keep their numbers.

### Phase 5 — MCP + deterministic context ⇒ **first public release, pgmind 0.1.0**
`knowledge.context()` v1: pins + transclusions + link-distance traversal + recency, deduplicated by content hash, ordered, packed to a token budget with block-ID citations; composable primitives public (`search` over FTS/tags/properties, `traverse`, `expand`, `backlinks`); **MCP server** exposing the lot (read/write/append/search/context as tools).
*User can:* the headline demo end-to-end: import a vault, point an agent at the MCP server, get budgeted, cited, reproducible context — zero AI configured.
*Benchmark:* context-assembly determinism + quality-per-token vs "cat the whole folder" and naive top-k FTS baselines, published.

### Phase 6 — Optional vector lane
`embedding_hook` tables (pgvector-typed, content-hash-keyed); retrieval/`context()` blend vector signal when rows exist; documented integration recipes (external embedding pipelines, Supabase-style triggers) — recipes, not features; pgmind still never calls a model.
*User can:* add RAG-style semantic recall to the brain with their own embedding pipeline, without re-embedding unchanged blocks — or ignore this phase entirely.

### Phase 7 — Retrieval & context maturation
Intent-driven planner over links + FTS + tags + metadata (+ vector lane when present); fusion; `EXPLAIN`-style plan introspection; optional BM25 adapter; published tradeoff curves (tokens vs answer quality).
*User can:* declare intent, inspect why the planner retrieved what it did, and trust `context()` as the default retrieval surface with measured quality.

### Future Work (explicitly speculative — not on the roadmap)
- **Enrichment patterns** (summaries, entity extraction) as *external* reference pipelines writing through the public API — if ever, following the architecture every surviving project converged on.
- **Instruction-driven AI editing** through the write API with provenance.
- **Conversational/episodic memory** as a layer over the block model.
- **Distributed knowledge:** cut (audit M3) — contradicts "we are not building another database"; storage-layer branching (Neon, Fluid Storage) covers the generic need.

---

# 9. Evaluation strategy

Benchmarks are defined at RFC acceptance, not phase end. Per-phase gates in §8; cross-cutting:

- **Parser:** CommonMark conformance; byte-faithful round-trip.
- **Identity/rebinding:** public adversarial edit corpus (splits, merges, move+edit, near-duplicates, rewrites) with published match-rate, tracked over time.
- **Concurrency:** multi-writer test suite (CAS conflicts, concurrent appends to one section, interleaved sync + API writes).
- **Context:** determinism (same vault, same call → same context), quality-per-token vs baselines.
- **Storage:** capacity model validated on synthetic vaults; TOAST/vacuum behavior under append-only load.
- **Definition of Done (per phase):** RFC accepted → benchmark defined → implementation → benchmark passed and published → API docs + example → quickstart still passes.

---

# 10. Distribution & adoption (reality context)

This project is currently exploration/learning; this section keeps reality in view so no decision forecloses the adoption path.

- **Year-1 reality:** self-hosted Postgres, Docker, CloudNativePG. Managed platforms (RDS, Cloud SQL, Azure, Neon, Supabase) are allowlist-only; pg_tle cannot carry compiled extensions; >58% of enterprises running production Postgres use a managed service for at least part of their fleet. We do not pretend otherwise.
- **The v0.3 scope helps here:** an AI-free, deterministic, permissively-licensed extension with no background workers and no network I/O is the *easiest possible* artifact to allowlist — the exact opposite of what pgrag/pgai asked platforms to swallow. The MCP server and sync CLI run outside the database and work everywhere from day one.
- **Channels:** PGXN v2 listing; OCI packaging; per-PG-major builds in CI from day one.
- **If adoption is ever pursued:** the pgvector playbook — permissive license (decided), community-primitive positioning, early partnership with one platform.
- **Success metrics:**
  - *Learning tier (now):* each phase's benchmark published; the 5-minute quickstart (built in Phase 4, re-gated at the Phase 5 release) passes on a clean machine; rebinding match-rate improves release over release.
  - *Adoption tier (if pursued):* a real agent product swaps file-based memory for pgmind with no behavior loss in under an hour; first external production deployment; first managed-platform allowlisting.
  - The v0.1 metric ("developers stop saying PG + Pinecone + Neo4j + Elasticsearch + LangChain") is retired as a 2023 strawman (audit C6). The competitor is **markdown files on the filesystem** — and the glue people write around them.

---

# 11. Risks & open questions

| Risk | Mitigation |
|---|---|
| **Rebinding quality plateaus** (heuristic identity matching too lossy under external edits) | Experimental track with public corpus from Phase 3; the write API remains the deterministic path; **cutting two-way sync removes the largest source of whole-document replaces** (see below); optional serialized `^id` mode as escape hatch |
| **"Why not just files + SQLite/git?"** (the low end fights back) | Be honest in positioning: single-writer local use should stay on files; pgmind starts winning at *concurrent agents, server backends, multi-tenancy* — lead with those |
| **Row-count economics of per-block revisions** | Capacity model as a Phase 2 deliverable (RFC-003), revalidated under Phase 3 revision load; partitioning + keyframes + excision from the start |
| **Precedent mortality pattern** (pgrag/Korvus/pgai) | Law 1 removes the fault line entirely; the "why we survive" argument ([AUDIT.md §4](archive/AUDIT.md)) reviewed at every phase gate |
| **Scope gravity toward AI features** | Laws 1-2; Future Work quarantine; RFC titles must declare law violations |
| **pgrx pre-1.0 churn** | Pin versions; budget upgrade time per release |
| **Naming/trademark (MindsDB adjacency)** | Decide before first public release; register names early |

### Why there is no sync bridge *(decided 2026-08-09)*

The roadmap carried a Phase 4 two-way filesystem sync bridge — `pgmind sync ./vault --watch`, a state file, three-way merge, conflict strategies — from v0.1 until it was cut. Three arguments, in increasing order of weight:

1. **It served the user this handbook says to cede.** The risk table above answers "why not just files?" with *single-writer local use should stay on files; pgmind starts winning at concurrent agents, server backends, multi-tenancy.* A continuous two-way bridge exists to serve one human editing one local folder — exactly the case we decline to compete for. We were building the losing half of our own positioning.
2. **It manufactured the risk it was cited as mitigating.** The rebinding-plateau row above used to read "sync bridge minimizes full-document replaces." It does the opposite: every file save is a whole-document replace, so continuous sync is the single largest generator of heuristic rebinding — the project's #1 research problem — and it generates it from the least controlled source, a human's editor.
3. **The complexity was concentrated in the half we cut.** Of RFC-006's nine decisions, four (state file, three-way merge, conflict strategies, watch mode) and two of its four benchmark suites existed only for two-way sync. Byte-exact import and export — the part law 4 actually needs — is two shell scripts and one gate.

What replaced it is smaller and honest: [`scripts/export-vault.sh`](../scripts/export-vault.sh) and [`scripts/import-vault.sh`](../scripts/import-vault.sh), gated by `folder-round-trip` over a corpus of legal-but-hostile paths. The measurement that prompted the cut is worth keeping in view — the naive shell loop this repo shipped in its own cookbook lost **2 of 8 notes** on such a vault and printed **one** error doing it, so "it's just a bash command" was true only for tidy ASCII paths. Refusing to write anything when two paths collide on a case-insensitive filesystem is the behaviour that makes the difference.

Reversing this means a new RFC that argues past point 2, and it inherits a `revision.source` CHECK that still permits `'sync'` with nothing able to set it (RFC-012 retires it alongside `'rebind'`).

**Open questions routed to RFCs:** wiki-link/block-ref syntax details (RFC-002); rebinding algorithm family and confidence thresholds (RFC-004); excision vs provenance guarantees (RFC-005/011); sync-bridge conflict semantics (RFC-006); BM25 adapter (RFC-010); name **[OPEN]**.

---

# 12. Process (right-sized)

**Documents:** this handbook (the constitution), the [product plan](PRODUCT-PLAN.md) (the operating blueprint: system design detail + per-phase delivery plan), [AUDIT.md](archive/AUDIT.md) (the evidence base), [CONTRIBUTING.md](../CONTRIBUTING.md) (roles & governance), and RFCs written *per phase* before implementation ([index](rfcs/README.md), [template](rfcs/TEMPLATE.md)). ([PLAN.md](archive/PLAN.md) is the working plan from the audit session, kept for provenance.) Precedence: handbook laws > accepted RFCs > product plan > code. (v0.1's 15-document list is retired; its two contradictory RFC lists are replaced by this single canonical index.)

**RFC lifecycle:** *living during its phase, frozen at phase exit.* Amendments after freeze get a new RFC.

| RFC | Title | Phase |
|---|---|---|
| 000 | Vision & Scope | 0 |
| 001 | Implementation Platform | 0 |
| 002 | Markdown Type, AST & Vault Syntax (wiki-links, tags, block refs) | 1 |
| 003 | Vault & Block Storage Layout (incl. edge/tag tables) | 2 |
| 004 | Block Identity & Rebinding Semantics | 2-3 |
| 005 | Version Engine, Concurrency Semantics & Excision | 3 |
| 006 | ~~Sync Bridge & Import/Export~~ — **withdrawn 2026-08-09** (§11) | — |
| 007 | Query API & MCP Surface | 5 |
| 008 | Deterministic Context Assembly & Token Budgeting | 5, matured 7 |
| 009 | Optional Vector Lane (pgvector hooks) | 6 |
| 010 | Retrieval Planner (incl. BM25 adapter decision) | 7 |
| 011 | Provenance | 3+ |
| 012 | Packaging & Distribution | 5+ |

**Repository layout** (target structure; the handbook and audit live at the repo root):

```
docs/          RFCs, archived handbook versions
extension/     the pgrx extension (type, vault model, storage, planner, API)
tools/         pgmind-mcp, the MCP server (Phase 5)
scripts/       export-vault.sh, import-vault.sh — the law-4 round trip
eval/          benchmark corpora, harnesses, published results
tests/         extension + integration tests
```

**Governance:** one human owner (currently: the project author) accepts RFCs and phase exits — see [CONTRIBUTING.md](../CONTRIBUTING.md).

---

# Motto

> Files hold notes. PostgreSQL holds the brain.

An Obsidian-shaped knowledge base with database-strength guarantees: identity minted on write, history with an audited eraser, writes that don't clobber, queries a folder could never answer, and one call that hands an agent exactly the context it needs — with no AI anywhere in the middle.
