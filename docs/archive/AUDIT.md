# Audit of the pgmind Product Handbook (v0.1)

**Audited document:** [`PGMIND-v0.1.md`](PGMIND-v0.1.md) ("Knowledge Extension for PostgreSQL — Project Handbook, Version 0.1")
**Audit date:** 2026-08-04
**Method:** 8-agent deep-research workflow — five sourced research streams (PostgreSQL AI-extension landscape; RAG/knowledge-framework competition; extension-engineering feasibility; block-identity & versioning prior art; market validation) followed by three independent adversarial critique passes (architecture, product, adoption). Every factual claim below carries at least one source; the full research corpus with all URLs is preserved in the session research output.
**Companion document:** [`PGMIND.md`](../PGMIND.md) — the revised handbook that resolves or explicitly tracks every Critical and Major finding here. (v0.2 was the direct post-audit revision; v0.3 sharpened the vision per author direction — agent-brain-first, strictly AI-free core — which *strengthens* every resolution below, since the audit's central recommendation was exactly "keep AI out of the core.")

---

## 1. Executive summary

**Verdict: the vision is validated; the spec is not buildable as written. Six critical defects must be fixed before any Phase 1 work — all six are fixable, and the revised handbook fixes them.**

The handbook's macro thesis — that a "knowledge layer" belongs inside PostgreSQL — is strongly supported by 2025-26 evidence. Postgres consolidation is the dominant database narrative (Databricks acquired Neon for ~$1B; Snowflake acquired Crunchy Data for ~$250M; Supabase reached a $5B valuation; pgvector is the most-installed vector store). And the specific layer pgmind targets is genuinely empty: **no extension or framework anywhere offers a markdown type with a parsed AST, stable block identifiers, block-level immutable revisions, or a token-budgeted `context()` compiler.** The only prior "markdown type" for Postgres is a 7-star toy from 2011.

At the same time, the handbook is a manifesto wearing an engineering handbook's clothes. It takes no position on any existential technical decision (language, parser, storage layout, where AI compute runs, license), commits to one deliverable that is provably impossible as specced (parser-derived permanent block IDs), embeds the exact architecture pattern that killed or forced the retreat of every direct precedent (in-database AI execution — pgrag, PostgresML/Korvus, pgai), and measures success against a stack ("PG + Pinecone + Neo4j + Elasticsearch + LangChain") that almost nobody runs in 2026.

The genuinely defensible novelty is narrower and better than the handbook claims: **Phases 1-3 (block-identified, versioned knowledge objects) and Phase 7 (token-budgeted context compilation)**. Everything else is either commoditized (vectors, BM25), contested (graph), or inadvisable (in-DB AI, distributed knowledge).

---

## 2. What the research validates

These claims in the handbook survive scrutiny and can be kept — now with evidence:

| Handbook claim | Verdict | Evidence |
|---|---|---|
| "Knowledge should live inside PostgreSQL" (consolidation thesis) | **Validated** | "Just Use Postgres" is the mainstream 2025-26 narrative ([TigerData, 532-pt HN thread](https://www.tigerdata.com/blog/its-2026-just-use-postgres)); Pavlo's retrospective: most database energy now flows to Postgres ([CMU](https://www.cs.cmu.edu/~pavlo/blog/2026/01/2025-databases-retrospective.html)); Neon→Databricks ~$1B, Crunchy→Snowflake ~$250M, Supabase $5B |
| A new first-class abstraction can succeed as an extension (PostGIS/JSONB/pgvector analogy) | **Validated, with caveat** | Microsoft's DocumentDB (BSON-on-Postgres) joined the Linux Foundation with AWS/Google backing ([LF](https://www.linuxfoundation.org/press/linux-foundation-welcomes-documentdb-to-advance-open-developer-first-nosql-innovation)). Caveat: those winners are *value types with closed algebras*; "Knowledge" as specced is a whole subsystem — see finding C2 |
| The knowledge-model layer is unoccupied | **Validated** | No markdown AST type, stable block IDs, block-level revisions, or context compiler exists in any extension, commercial or open. Only prior markdown type: [sycobuny/pg_markdown](https://github.com/sycobuny/pg_markdown), 7 stars, last pushed 2011. EDB's commercial AIDB "knowledge bases" are managed chunk-embedding tables — no identity, versioning, provenance, or budgeting ([EDB docs](https://www.enterprisedb.com/docs/aidb/latest/overview/)) |
| Token-budgeted `knowledge.context()` has no competitor | **Validated — strongest claim in the doc** | Closest analogs: Zep v3's Context Block (character-budgeted, agent-memory-scoped only — [Zep](https://blog.getzep.com/zep-v3-context-engineering-takes-center-stage/)); GraphRAG's `max_data_tokens` config knob ([Microsoft](https://microsoft.github.io/graphrag/query/global_search/)). Budget-constrained context assembly is an active open research problem (AdaGReS et al.) |
| Stable block identity + append-only revisions is a real gap | **Validated** | Industry best practice is doc_id + content-hash + *delete-all-chunks-and-reinsert* on every update; chunk IDs don't survive re-chunking ([Oracle](https://blogs.oracle.com/developers/how-to-detect-rag-index-drift-deleted-docs-stale-chunks-and-duplicate-embeddings), [Pixeltable](https://www.pixeltable.com/blog/embedding-management-guide)). Nobody does block-level identity across revisions |
| "Deterministic APIs first, AI-enhanced second" | **Validated — promote to architectural law** | The ecosystem's survival pattern exactly: deterministic primitives (pgvector, PostGIS) thrive; in-database AI layers died (see §4). This is the handbook's best sentence and it under-uses it |
| Phase 7 vocabulary (token budgeting, dedup, compression, ordering) | **Validated** | Matches the 2025-26 "context engineering" discourse word-for-word ([Anthropic](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents), Karpathy, [RAGFlow's "RAG → Context Engine"](https://ragflow.io/blog/rag-review-2025-from-rag-to-context)). The handbook never uses the term — it should, since that's the vocabulary developers actually search and budget under |

---

## 3. Market & distribution reality

Included per audit scope as *reality context* — this is the world the project operates in, whether or not adoption is ever pursued.

### 3.1 The consolidation war is already being won — by others

- **Vectors:** pgvector is the #1 vector store (21.3% in Retool's State of AI survey), preinstalled on RDS, Aurora, Cloud SQL, Azure, Supabase, Neon. Pinecone explored a sale in 2025 ([The Information](https://www.theinformation.com/articles/top-funded-ai-database-startup-pinecone-considers-sale)). pgvectorscale adds disk-resident ANN "as fast as Pinecone."
- **Search:** BM25-in-Postgres is a funded three-way race: ParadeDB pg_search ($12M Series A, 250k+ installs, AGPL), TigerData pg_textsearch (v1.0 GA, PostgreSQL license), VectorChord-bm25.
- **Graph:** Neo4j is *not* being displaced — it passed $200M revenue growing on GraphRAG. Apache AGE survives but is immature with real production pain (LightRAG issues [#2122](https://github.com/HKUDS/LightRAG/issues/2122), [#2255](https://github.com/HKUDS/LightRAG/issues/2255)); PG18's native SQL/PGQ is the anticipated fix.
- **Frameworks:** LangChain is in documented retreat (abstraction-bloat backlash; [Octomind's canonical case study](https://octomind.dev/blog/why-we-no-longer-use-langchain-for-building-our-ai-agents)).
- **From above:** managed one-call RAG (Gemini File Search at $0.15/M tokens indexed, OpenAI file_search) makes simple "chat with my docs" nearly free — commoditizing pgmind's easiest use case.

**Consequence:** the 2026 pain is not "too many databases." It is ingestion/sync glue, chunk drift, freshness, versioning, evaluation, and context assembly — practitioners call production RAG "a fragile dance of glue code and faith" ([HF thread](https://discuss.huggingface.co/t/why-does-rag-still-feel-clunky-in-2025/164650)). That is the pain pgmind should claim.

### 3.2 The distribution wall

- Managed platforms run curated allowlists. Cloud SQL: "You cannot create your own extensions" ([Google docs](https://docs.cloud.google.com/sql/docs/postgres/extensions)). Azure requires allowlisting; Neon disallows custom compiled extensions; Supabase is a curated set. **>58% of enterprises running production Postgres use a managed service for at least part of their fleet** (2025).
- pg_tle (Trusted Language Extensions) categorically **excludes compiled C/Rust code** ([AWS](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL_trusted_language_extension.html)) — a custom-type parser extension cannot ship that way.
- Even pgvector took ~2 years plus the GenAI wave to reach RDS. Funded ParadeDB is still absent from the big three clouds.
- Extension-owned background workers need `shared_preload_libraries`, which managed platforms lock down.
- The registry landscape is itself unstable: Trunk shut down July 2025; PGXN v2 is being rebuilt as the canonical registry ([PGXN v2 wiki](https://wiki.postgresql.org/wiki/PGXN_v2)).

**Consequence:** year-1 reality is self-hosted / Docker / CloudNativePG only. "I installed the Knowledge extension" is unreachable for most of the market for years, regardless of quality.

### 3.3 Licensing evidence

Every allowlisted, ubiquitous extension is permissively licensed (pgvector, PostGIS, AGE, pg_textsearch). ParadeDB's AGPL keeps it off Supabase and drew sustained backlash TigerData exploited ([HN](https://news.ycombinator.com/item?id=40348443)); the Timescale License locks TimescaleDB off managed platforms. **For a ubiquity thesis, the PostgreSQL license is the only consistent choice.**

### 3.4 Persona and channel

The realistic first users are **self-hosted AI product developers and agent-tooling builders** (Supabase: ~30% of new signups are AI builders; 15%+ of new databases enable pgvector), not enterprise RDS users (who can't install it). Demand vocabulary has shifted to **"agent memory" and MCP** — every major DBMS added MCP support in 2025; Neon reports 80% of its databases are created by agents. `knowledge.context()` maps naturally onto an MCP tool, and MCP servers run *outside* the database, sidestepping part of the distribution wall.

### 3.5 Naming

"pgmind" is effectively unclaimed — nothing on PGXN or crates.io; one dead 0-star GitHub fork. But it sits conceptually adjacent to **MindsDB**, an active funded "AI data" company, creating positioning confusion and possible trademark friction. Note the v0.1 handbook never actually says "pgmind" — it calls itself "Knowledge Extension," which is generic and collides with EDB AIDB's "knowledge bases" vocabulary. Pick one identity and register it.

---

## 4. Precedent post-mortems (required reading)

Four direct precedents of "AI/RAG pipeline inside a Postgres extension" — all dead or retreated by 2025-26:

| Project | What it was | Outcome | Lesson |
|---|---|---|---|
| **Neon pgrag** | End-to-end RAG extensions: PDF/DOCX extraction, chunking, in-DB embedding models (>100MB), reranking, LLM calls | **Archived June 2026**, self-described "Deprecated/unmaintained" ([repo](https://github.com/neondatabase-labs/pgrag)) | Models inside the database process don't survive |
| **PostgresML + Korvus** | "Entire RAG pipeline in a single SQL query" — the closest precedent to `knowledge.context()` | **Company bust 2025** ([Pavlo](https://www.cs.cmu.edu/~pavlo/blog/2026/01/2025-databases-retrospective.html)); repos stale | Single-query RAG is *feasible* and still failed — convenience alone isn't a moat |
| **Neon pg_embedding** | Vector index extension | **Deprecated Sept 2023** once pgvector shipped HNSW | Don't compete with the community primitive; compose with it |
| **TigerData pgai** | Declarative in-DB vectorizer + `ai.*` LLM SQL functions | Vectorizer moved **out** of the extension into a Python worker (v0.10.0); Tiger Cloud removes `ai.*` helpers by **June 30, 2026**; repo archived ([deprecation notice](https://www.tigerdata.com/docs/deploy/tiger-cloud/vectorizer-deprecation)) | The market leader concluded LLM/embedding calls belong in application/worker code, not the DB |

**Common causes of death:** (1) network-bound AI work inside the Postgres backend — blocking HTTP in transactions holds locks and snapshots, stalls autovacuum, ties up connections ([pg_net exists precisely because of this](https://supabase.com/docs/guides/database/extensions/pg_net)); (2) uninstallable on the managed platforms where the users are.

**Why pgmind can survive where they died** (the argument v0.1 lacks and v0.2 makes): keep *all* model execution out of the extension; spend the novelty budget on the deterministic knowledge model (identity, versioning, provenance, planning, budgeting) that is safe in-process, useful without AI, and allowlist-compatible; compose with pgvector/BM25 instead of competing.

---

## 5. Findings

### Critical (spec-breaking — must change before Phase 1)

**C1. "Stable block IDs" cannot be a parser deliverable — identity is a write-path property.**
The handbook promises "a permanent identifier" for every block and places "stable block ids" inside Phase 1's *parser*. All prior art contradicts this: Notion mints UUIDs at block creation and mutates by ID ([Notion data model](https://www.notion.com/blog/data-model-behind-notion)); CRDTs (Yjs, Automerge, Loro) assign operation IDs at edit time; ProseMirror deliberately refuses built-in node IDs because split/merge makes identity ill-defined ([Haverbeke](https://discuss.prosemirror.net/t/data-structure-with-ids/33)); and matching blocks across two plain-text versions is a fundamentally heuristic tree-diff problem with 20+ years of literature (GumTree, XyDiff, difftastic) documenting irreducible ambiguity on splits, merges, move+edit, and near-duplicates. A parser can emit positions and content hashes — never persistent identity. Notion's markdown *export loses block IDs entirely*: round-tripping through plain text destroys identity even for the best-funded implementation.
**Fix (adopted in v0.2):** a specced hybrid — (a) block-addressed write API (`knowledge.update_block(id, …)`) where identity is deterministic; (b) heuristic re-matching with explicit confidence/rebinding semantics for whole-document replacement; (c) optional serialized-ID syntax (Obsidian `^id` precedent) for deterministic round-trips. Identity gets its own RFC with split/merge/move/copy semantics and an adversarial edit-corpus benchmark. Misassigned IDs silently corrupt provenance — this is the project's #1 research problem, not a bullet point.

**C2. Phases 5 and 8 repeat the in-database-AI pattern that killed every precedent.**
As written, embedding generation, entity extraction, summaries, and LLM-driven editing run inside the database. See §4: pgrag archived, PostgresML/Korvus dead, pgai retreated. Synchronous network I/O in transactions is operationally hostile, and in-extension background workers need `shared_preload_libraries` access managed platforms deny.
**Fix (adopted in v0.2):** hard architectural boundary — the extension owns only deterministic logic (parsing, IDs, versioning, graph storage, planning, token counting, context assembly over precomputed indexes); anything calling a model runs in a **companion worker** coordinated via queue tables. Product rule: *no synchronous network I/O inside any transaction or API call* — `context()` must never lazily embed the query via blocking HTTP. The shipping artifact is honestly "extension + companion worker."
**v0.3 delta:** the author subsequently removed model execution from the product entirely — no companion worker, no in-product enrichment; vectors are an optional user-populated lane (pgvector hooks). That is a *superset* of this fix. Where this audit's resolutions (here, M2, M10, m3, and the verdict table) mention the companion worker, they describe the v0.2 resolution; v0.3 supersedes them in the stricter direction.

**C3. Storage layout is undecided, and "Native Markdown Type" invites the monolithic-AST trap.**
A single varlena datum holding a parsed AST reproduces the documented JSONB/TOAST pathology: whole-document detoast on any block access, no partial updates at storage level, 2-10x slowdowns past ~2KB ([evanjones.ca measurements](https://www.evanjones.ca/postgres-large-json-performance.html)), TOAST/WAL write amplification on every revision — catastrophic for a system whose entire point is block-level addressing and per-block revisions. The handbook's own philosophy ("Markdown is not storage") implies the right answer; its layer diagram then contradicts it by making "Markdown AST" a storage layer.
**Fix (adopted in v0.2):** per-block relational rows keyed by **surrogate UUID**, with **content hashes kept separate** for dedup and embedding reuse (Dolt's content-addressing shows why conflating the two breaks one goal or the other); the markdown type is a thin parse/serialize/validate boundary. RFC-003 must cover LZ4 TOAST compression, insert-only autovacuum tuning, and history-table partitioning.

**C4. "History is permanent" is unshippable as an absolute.**
Every shipped immutable database was forced to add erasure: Datomic excision, Dolt 2.0 GC (July 2026), XTDB 2.2 GC, TerminusDB squash — for storage economics and GDPR right-to-erasure. Per-block revision granularity is novel (all surveyed products version at document level) and multiplies row counts: blocks × revisions × index entries — Notion needed 96-server sharding for block rows.
**Fix (adopted in v0.2):** "append-only by default, with audited excision and retention policies"; snapshot keyframes to bound delta-chain reconstruction (MediaWiki's revision-chain model achieves ~98% compression — [Wikimedia](https://wikitech.wikimedia.org/wiki/External_storage)); explicit capacity modeling. Postgres actually *rewards* insert-only design (avoids TOAST update write-amplification) — a genuine argument for append-only worth stating, once qualified.

**C5. No distribution strategy, no license.**
See §3.2-3.3. RFC-013 "Extension Packaging" is a bare title. The handbook's success metric assumes an install path most of the market doesn't have.
**Fix (adopted in v0.2):** PostgreSQL license as a stated non-negotiable; year-1 audience = self-hosted/Docker/CloudNativePG; PGXN v2 + OCI packaging; allowlisting treated as a later milestone with the pgvector playbook if ever pursued; MCP server as the agent-facing surface that runs outside the DB.

**C6. The success metric is a 2023-era strawman.**
"Developers stop saying PostgreSQL + Pinecone + Neo4j + Elasticsearch + LangChain" — but pgvector already won vectors, Elasticsearch-displacement is a funded existing category, Neo4j is growing (not being displaced), LangChain is in retreat. The metric is also unfalsifiable (no number, no timeframe, no observable event). Informed reviewers — the exact audience pgmind needs — will read it as fighting a war that's over.
**Fix (adopted in v0.2):** the real competitor is the **application-layer glue stack** (pgvector + LlamaIndex/LangChain + chunking scripts + an app-layer memory service). Metrics become measurable milestones (benchmarks published, working phase deliverables, and — in a product tier — deployments/allowlistings).

### Major (materially wrong or missing — fix in the same revision)

**M1. No compose-vs-rebuild position.** Read literally, Phases 4-6 + the Mission commit a team of zero to rebuilding pgvector + ParadeDB + AGE. Fix: hard dependency on pgvector; use an existing BM25 engine (tsvector fallback); graph backend is a named open question (AGE's production pain makes native edge tables over recursive CTEs defensible; track PG18 SQL/PGQ). Novelty budget is spent **only** on the knowledge model and planners.

**M2. Phase 4 conflates a deterministic feature with an AI feature.** Explicit link graphs (markdown links, transclusions, declared dependencies) are easy and belong early; "automatic relationship generation" requires LLM extraction and inherits every C2 problem — it moves to the companion worker after semantic indexing, mandatorily incremental (GraphRAG's full-reindex cost — up to $33k for 5GB — is the cautionary tale; LazyGraphRAG/LightRAG's incremental answers are the fix).

**M3. Phase 9 (Distributed Knowledge) contradicts the doc's own vision.** "We are not building another database" — then Phase 9 builds replication/federation. Precedents are grim (Timescale abandoned multi-node; Microsoft scaled back Citus); Neon branching and Fluid Storage already cover the generic need. Cut to a speculative appendix.

**M4. No MVP or first-value point.** Value arrives at Phase 7 of 9; comparable extensions took 4-6+ years *with funded teams* (PostGIS 4yr to 1.0, Citus 6yr, TimescaleDB ~4yr). Fix: thin vertical slice — after Phases 1-3, ship a naive `context()` composing over user-installed pgvector/tsvector; every phase states what a user can *newly do*.

**M5. No ingestion/sync story anywhere.** The dominant practitioner pain (§3.1) — how documents get *into* the system from git/filesystem/S3, idempotent re-import, freshness — is unaddressed by any phase or RFC. It also forces the identity-on-reimport question into the open (good). Fix: Ingestion & Sync RFC + CLI/worker sync tool + a 5-minute quickstart as an explicit deliverable.

**M6. No persona, use cases, or non-goals.** "Developers" is the only persona word in the document. Fix: named persona (self-hosted AI product dev, agent-tooling builder), 3-5 concrete scenarios, and explicit non-goals (PDF/DOCX via external converters producing markdown; conversational/agent memory explicitly out of v1 scope or named as future layer; not a general document store).

**M7. No evaluation strategy.** "Benchmarks passed" names no benchmark. Context quality has measurable tradeoffs (Zep: 54% token savings cost ~8 accuracy points on LoCoMo; "context rot" research shows accuracy degrades with token count) and block identity is untestable without an adversarial edit corpus. Fix: named per-phase benchmarks with thresholds set at RFC acceptance (parser: CommonMark conformance + round-trip fidelity; identity: published corpus match-rate; context: quality-per-token vs naive top-k baseline).

**M8. No foundational technology decisions.** For a doc that decrees "no implementation without documentation," it never chooses: language (pgrx vs C), parser (comrak vs pulldown-cmark vs tree-sitter), markdown flavor (CommonMark vs GFM — this determines the entire block taxonomy), tokenizer for budgeting, PG version matrix. Fix: decide as revisitable defaults — pgrx (proven by pg_search, pgvectorscale, pg_graphql; pre-1.0 caveat noted), comrak (production CommonMark+GFM with sourcepos), CommonMark+GFM subset, PG16+.

**M9. Process weight is a delivery risk, and the doc contradicts itself.** 15 core docs + 16 RFCs, immutable after acceptance, for a v0.1 with zero code — stricter than IETF or Rust practice. Worse, the "RFC Strategy" list and the "Documents To Write" RFC list **assign four slots to different subjects outright** (RFC-004 Versioning vs Block Model; RFC-005 Query Planner vs Version Engine; RFC-007 Hybrid Search vs Search Planner; RFC-009 Graph Model vs Query API), with two more differing in name — the project's canonical planning artifact is internally inconsistent. Fix: one canonical RFC index; ~5 documents for v0.1; RFCs "living during phase, frozen at exit"; identity diff-engine explicitly run as an experimental track with a benchmark corpus, not spec-first.

**M10. The layer diagram is wrong twice.** (1) "Markdown AST" appears as a persistent layer beneath Knowledge Objects, contradicting "Markdown is not storage." (2) "No layer bypasses another" is unimplementable — indexes attach to Postgres AMs and tables directly; the planner consults Postgres statistics; workers cut across layers. Fix: redraw with per-block storage as the foundation, the parser as an ingress/egress boundary at the side, companion worker outside the extension, and a dependency rule ("public APIs depend only on documented APIs of lower layers") instead of a strict stack.

**M11. `context()` is positioned as the only mode; agents increasingly want iterative retrieval.** Anthropic's guidance favors just-in-time retrieval over one pre-compiled blob; Letta/MemGPT agents self-manage context. Fix: `context()` is one mode; the composable deterministic primitives beneath it (search, traverse, expand, follow-references) are first-class SQL functions agents can call iteratively; expose the lot via an MCP server.

**M12. The AI Agent Roles section is an org chart inside a product doc.** A third of the handbook defines team roles ("Storage Engineer owns WAL" — implying scope the product disclaims), names no decision authority for "immutable" RFC acceptance, and substitutes for the missing product content. Fix: move to CONTRIBUTING.md; one-paragraph governance statement in the handbook; add the missing roles the corrected architecture needs (companion-worker/ops, eval/benchmark ownership).

### Minor

- **m1.** Naming inconsistency: file is PGMIND.md, doc says "Knowledge Extension," neither identity secured (§3.5).
- **m2.** Documentation hierarchy roots at PROJECT.md but the handbook lives in PGMIND.md with no stated relationship.
- **m3.** Directory layout (`extension/ parser/ planner/ storage/`) has no `worker/` component and no one-line definition of what lives where.
- **m4.** The handbook never uses "context engineering" — the term its Phase 7 describes and the vocabulary developers search under.
- **m5.** Phase 3×5 interaction unstated: naive re-embedding of every block of every revision is a cost bomb; the block-diff + content-hash design from Phases 1-3 is precisely what enables incremental index maintenance — v0.2 states "no full reindex on document update" as an invariant.

---

## 6. Section-by-section verdict on v0.1

| v0.1 section | Verdict | Disposition in v0.2 |
|---|---|---|
| Project Vision | Sound thesis, weak analogy | Kept; analogy qualified; "context engine" positioning added |
| Mission | Right instinct, outdated pain framing | Reframed around glue/lifecycle pain; honest artifact ("extension + worker") |
| Long-Term Goal (`knowledge.context()`) | Strongest idea in the doc | Kept and centered; iterative primitives added alongside |
| Philosophy: Knowledge is source of truth | Fine | Kept |
| Philosophy: Markdown is an interface | Correct — and load-bearing | Kept; now actually enforced by the storage design |
| Philosophy: AI is a consumer / provenance | Fine | Kept and strengthened: AI is a consumer, never a component |
| Philosophy: Context is the product | Fine | Kept |
| Philosophy: Semantic APIs | Fine | Kept; extended with composable, individually-callable primitives (M11) |
| Philosophy: Immutable knowledge | Unshippable as absolute | Qualified: append-only + audited excision (C4) |
| Philosophy: Stable identities | Impossible as stated | Rewritten with identity strategy + rebinding semantics (C1) |
| Architectural Principles (layer stack) | Wrong twice | Redrawn (M10) |
| Project Structure | Incomplete | `worker/`, `eval/` added; one-line definitions |
| Documentation Strategy / RFC Strategy | Overweight + self-contradictory | Slimmed; single canonical RFC index; living-RFC lifecycle (M9) |
| Documents To Write | Overweight | Cut to 3 core docs + per-phase RFCs |
| Documentation Hierarchy | Overweight | Retired; absorbed into the process section |
| Phases 1-3 | Right scope, one fatal spec error | Kept as core wedge; identity fixed (C1); storage fixed (C3) |
| Phase 4 | Conflates deterministic + AI | Split (M2) |
| Phase 5 | In-DB AI pattern | Moved out of the database (C2); in v0.3, out of the product — optional user-populated vector lane |
| Phase 6 | Feasible but commoditized | Compose over pgvector/BM25; novelty = planner (M1) |
| Phase 7 | The differentiator | Kept; eval harness + composable primitives added (M7, M11) |
| Phase 8 | In-DB AI pattern | Split: automatic maintenance → optional external enrichment, incremental-only; instruction-driven editing → Future Work (C2, m5) |
| Phase 9 | Contradicts own vision | Cut to speculative appendix (M3) |
| Product Rules | Mostly good | Kept; "no sync network I/O in transactions" added as law |
| Coding Rules | Good ordering | Kept as build-order rule in CONTRIBUTING.md; enforced by roadmap ordering |
| AI Agent Roles | Wrong document | Moved to CONTRIBUTING.md (M12) |
| Definition of Done | Unmeasurable | Concrete per-phase benchmarks (M7) |
| Success Metric | Strawman, unfalsifiable | Rewritten measurable (C6) |
| Motto | Fine | Kept in spirit; second line rewritten to the context-engine positioning |

---

## 7. Sources

Primary sources are linked inline above. The full research corpus — five research streams and three critique passes with complete URL lists (~180 tool calls) — is preserved in the session's workflow output. Highest-value primary sources for ongoing reference:

- [Andy Pavlo — 2025 Databases Retrospective](https://www.cs.cmu.edu/~pavlo/blog/2026/01/2025-databases-retrospective.html) (PostgresML death, consolidation, MCP wave)
- [Tiger Cloud vectorizer deprecation notice](https://www.tigerdata.com/docs/deploy/tiger-cloud/vectorizer-deprecation) (the in-DB AI retreat, primary evidence)
- [Notion's data model](https://www.notion.com/blog/data-model-behind-notion) (block-UUID-in-Postgres precedent)
- [Anthropic — Effective context engineering for AI agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents) (Phase 7 problem framing + just-in-time counter-current)
- [PostgreSQL TOAST documentation](https://www.postgresql.org/docs/current/storage-toast.html) + [large-JSON measurements](https://www.evanjones.ca/postgres-large-json-performance.html) (storage-layout evidence)
- [AWS pg_tle docs](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL_trusted_language_extension.html) (distribution wall)
- [Wikimedia external storage](https://wikitech.wikimedia.org/wiki/External_storage) (delta-chain revision storage at scale)
