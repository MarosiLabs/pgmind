# pgmind — Audit Plan & Research Findings

> Working plan for auditing `PGMIND.md` v0.1 and producing the revised working documents.
> Research basis: 8-agent deep-research workflow (5 research streams + 3 adversarial critics, ~437k tokens, all claims sourced).

## Context

`PGMIND.md` (v0.1) proposes a PostgreSQL extension introducing **Knowledge** as a first-class abstraction (markdown AST type → block objects → versioning → graph → semantic indexes → hybrid search → token-budgeted `knowledge.context()` → AI editing → distributed knowledge, in 9 phases). Task: read it, audit it, research it, and deliver audited documents to start working from — as technical product manager, not assuming the doc must be followed as-is.

**Agreed parameters:**
- Deliverable: **audit report + revised handbook** (two documents)
- PM latitude: **restructure freely** (cut, reorder, rescope, add missing sections)
- Project intent: **exploration/learning** — audit weighs technical feasibility and scoping heavily
- **Market/distribution findings are included in full as dedicated "reality context" sections in both deliverables** — the market landscape, competitive positioning, managed-platform distribution wall, licensing evidence, and naming findings all appear explicitly, framed as the reality this project operates in rather than as go-to-market obligations

## Key audit findings to encode (consolidated, deduplicated)

### Validated

- **Macro thesis is real.** Postgres consolidation is the winning 2025-26 narrative ($1B Neon/Databricks, $250M Crunchy/Snowflake, Supabase at $5B, pgvector #1 vector store at 21.3%).
- **The knowledge-model layer is genuinely empty whitespace.** Nothing anywhere offers a markdown type with AST, stable block IDs, block-level immutable revisions, or a token-budgeted `context()` compiler (only prior art for a markdown type in PG: a 7-star toy from 2011). Phases 1-3 + 7 are the defensible novelty.
- **"Deterministic first, AI last" matches the ecosystem's survival pattern exactly** — deserves promotion from sequencing preference to architectural constraint.
- **Phase 7's vocabulary matches the 2025-26 "context engineering" discourse** (Karpathy, Anthropic) word-for-word; no competitor exposes token-budgeted context compilation (Zep's Context Block is closest: character-budgeted, memory-scoped only).

### Critical defects

1. **Stable block IDs cannot come from a parser.** All prior art (Notion, Yjs/Automerge, ProseMirror, GumTree/XyDiff tree-diff literature) shows identity is a *write-path* property; cross-version matching of plain text is fundamentally heuristic. Phase 1 as written commits to an impossible deliverable. Fix: hybrid strategy — block-addressed write API (deterministic) + heuristic re-matching with documented rebinding semantics for whole-doc replacement + optional serialized `^id` syntax — with its own RFC and an adversarial edit-corpus benchmark. Split/merge/move semantics must be specced.
2. **In-database AI (Phases 5, 8) repeats the pattern that killed every precedent.** Neon pgrag (archived), PostgresML/Korvus (bust 2025), pgai (retreated to an external Python worker; Tiger Cloud removes in-DB ai.* SQL by June 30, 2026; repo archived). Fix: the extension owns only deterministic logic; anything calling a model runs in a companion worker via queue tables. Hard rule: **no synchronous network I/O inside transactions** (including `context()` — query embeddings precomputed, client-supplied, or async).
3. **Storage layout undefined; "Native Markdown Type" invites the monolithic-AST-datum trap** — the documented JSONB/TOAST pathology (whole-doc detoast, no partial updates, 2-10x slowdowns past ~2KB, WAL churn). Fix: per-block relational rows keyed by surrogate UUID (Notion model), content hashes kept separate for dedup/embedding reuse; the markdown type is a thin parse/serialize/validate boundary. (The doc's own "Markdown is not storage" philosophy implies this; its layer diagram contradicts it.)
4. **"History is permanent" is unshippable as an absolute.** Datomic (excision), Dolt 2.0 (GC), XTDB 2.2 (GC), TerminusDB (squash) all added erasure for storage economics + GDPR. Fix: append-only by default + audited excision/retention; snapshot keyframes (MediaWiki precedent, ~98% compression), LZ4 TOAST, partitioning; model row-count economics (blocks × revisions × index entries — Notion needed 96-server sharding for block rows).
5. **Distribution reality absent.** Managed platforms are allowlist-only; pg_tle excludes compiled code; even pgvector took ~2 years to reach RDS. No license specified (PostgreSQL license is the only ubiquity-consistent choice; ParadeDB's AGPL backlash is the cautionary tale). Year-1 reality: self-hosted/Docker only. (Weighted as reality context given learning intent.)
6. **Success metric is a 2023 strawman.** Nobody runs "PG + Pinecone + Neo4j + Elasticsearch + LangChain" in 2026 — pgvector already won vectors, Neo4j is growing (not displaced), LangChain is in documented retreat. The real 2026 pain: ingestion/sync glue, chunk drift, freshness, versioning, eval, context assembly. The real competitor: the app-layer RAG/memory glue stack.

### Major fixes

- **Compose, don't rebuild:** hard dependency on pgvector; use an existing BM25 engine; graph = named open question (Apache AGE has production pain; native edge tables over recursive CTEs defensible; track PG18 SQL/PGQ). Novelty budget spent only on the knowledge model + planners.
- **Split Phase 4:** deterministic link graph (easy, stays early) vs LLM relationship extraction (moves after Phase 5, companion-worker scope, incremental block-diff-driven — GraphRAG's $33k full-reindex is the cautionary tale).
- **Cut Phase 9** (contradicts "we are not building another database"; Citus/Timescale multi-node precedents are grim) → speculative appendix.
- **No MVP/first-value point:** value arrives at Phase 7 of 9 = years with nothing adoptable. Fix: thin vertical slice — Phases 1-3 + a naive `context()` over pgvector/tsvector as v0.1; state per-phase what a user can newly do.
- **Missing entirely:** persona/use cases (realistic: self-hosted AI product devs + agent-tooling builders; agents consume via MCP), ingestion/sync story (THE dominant practitioner pain; also forces the identity-on-reimport question into the open), non-goals (PDF/DOCX via external converters; conversational memory in/out), competitive analysis, risks section ("why we survive where pgrag/Korvus died"), eval strategy (context-quality-per-token benchmarks; Zep's 54% token savings / −8 accuracy points shows why), technology decisions (pgrx vs C, comrak, CommonMark+GFM, tokenizer, PG version matrix).
- **Process weight:** 15 core docs + 16 immutable RFCs for a v0.1 with zero code; the doc's two RFC lists *contradict each other* (four slots assigned to different subjects outright, plus renames). Fix: one canonical RFC index, minimal doc set for v0.1, "living during phase, frozen at exit" lifecycle; AI Agent Roles move to CONTRIBUTING.md.
- **Layer diagram:** markdown AST shown as a storage layer (contradicts philosophy); "no layer bypasses another" is unimplementable for PG extensions. Redraw: per-block storage foundation, markdown parser as ingress/egress boundary, companion worker outside the extension, dependency rule instead of strict stack.
- **Timeline calibration:** PostGIS 4yr to 1.0, Citus 6yr, TimescaleDB ~4yr — set honest expectations.
- **Naming:** "pgmind" unclaimed (PGXN/crates.io empty) but adjacent to MindsDB; the doc titles itself "Knowledge Extension" and never says "pgmind" — pick one identity.
- **`context()` is one mode:** also expose composable deterministic primitives (search/traverse/expand) for iterative agentic retrieval (per Anthropic's just-in-time guidance); adopt "context engineering" positioning language.

## Deliverables (execution steps)

1. **`git init`** the repo (it isn't one) so the original doc is preserved in history — and keep the original as `docs/archive/PGMIND-v0.1.md` for side-by-side reading.
2. **Write `AUDIT.md`** (repo root) — the audit report:
   - Executive summary + overall verdict
   - Methodology (5 research streams + 3 adversarial critics, sourced)
   - What the research validates (with key source URLs)
   - **Market & distribution reality** (dedicated section): the 2025-26 Postgres consolidation landscape and M&A evidence; competitive map (extension primitives, EDB AIDB, agent-memory cohort, managed RAG APIs like Gemini File Search); the managed-platform allowlist wall (RDS/Cloud SQL/Azure/Neon/Supabase, pg_tle's compiled-code exclusion, >58% managed share); licensing evidence (PostgreSQL license vs ParadeDB's AGPL backlash vs Timescale License lockout); realistic persona (self-hosted AI devs, agent-tooling builders, MCP channel); naming findings (pgmind unclaimed, MindsDB adjacency)
   - Findings ranked Critical / Major / Minor, each with evidence + concrete fix
   - Section-by-section verdict table on the original handbook
   - Precedent post-mortems: pgrag, PostgresML/Korvus, pg_embedding, pgai
3. **Write revised `PGMIND.md`** (v0.2) — the working handbook, freely restructured:
   - Vision & positioning ("a context engine inside PostgreSQL", honest 2026 framing)
   - Users & use cases; Scope & Non-goals
   - Relationship to the ecosystem (compose over pgvector/BM25; contrast vs pgai/EDB AIDB/agent-memory platforms)
   - Philosophy (corrected: identity with defined rebinding semantics; append-only + excision; markdown as boundary)
   - Architecture (corrected diagram incl. companion worker; per-block storage; no-sync-network-I/O rule)
   - Technology decisions (pgrx recommended, comrak, CommonMark+GFM, PostgreSQL license, PG16+ matrix — defaults with rationale, marked revisitable)
   - Roadmap: restructured phases with MVP slice (1-3 + naive `context()`), per-phase user-visible value, Phase 4 split, Phase 9 → Future Work, timeline calibration
   - **Distribution & adoption reality** (dedicated chapter): license decision (PostgreSQL license) with rationale; year 1-2 audience = self-hosted/Docker/CloudNativePG; the allowlisting path and pgvector playbook if the project ever pursues adoption; PGXN v2 + OCI packaging channels; MCP server as the agent-facing delivery surface; honest reframed success metrics
   - Risks & open questions (incl. survival argument); Evaluation strategy (named benchmarks per phase)
   - Lightweight process: single canonical RFC index (adding identity, excision, companion-worker, ingestion, distribution RFCs), living-RFC lifecycle
   - Measurable success metrics reframed for a learning project, with adoption metrics as an "if this becomes a product" tier

## Verification

- Every Critical and Major finding in `AUDIT.md` has a corresponding resolution or explicit open-question entry in the revised `PGMIND.md`.
- The revised handbook's RFC list is internally consistent (the original's two lists contradicted each other — regression-check this).
- Spot-check cited facts/URLs against the research output so no claim in `AUDIT.md` is unsourced.
- Both documents render cleanly as markdown.
