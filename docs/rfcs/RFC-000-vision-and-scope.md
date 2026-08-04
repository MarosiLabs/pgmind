# RFC-000: Vision & Scope

- **Status:** Living (phase active) — accepted by owner 2026-08-05; freezes at Phase 0 exit (CI matrix green)
- **Phase:** 0
- **Owner:** project author
- **Created:** 2026-08-04 · **Frozen:** —

## 1. Context

The de facto knowledge substrate for AI agents in 2026 is markdown files on a filesystem (Claude Code memory, agent memory directories, Obsidian vaults). This works for a local, single-user program and fails structurally in server backends: no transactions across concurrent writers, no queries (backlinks, tags, history), no multi-tenancy, and a growing pile of sync glue. The teams hitting this pain already run PostgreSQL.

An evidence audit of this project's original vision ([AUDIT.md](../../AUDIT.md)) established two decisive facts. First, the layer pgmind targets is empty: no extension or framework offers a markdown vault model in a database — stable block identity, block-level revisions, backlinks/tags in SQL, deterministic budgeted context assembly. Second, every precedent that put AI *inside* a Postgres extension died or retreated in 2025-26 (Neon pgrag: archived; PostgresML/Korvus: bust; TigerData pgai: forced out of the extension, then archived), while deterministic primitives (pgvector, PostGIS) thrive.

This RFC condenses the [handbook](../../PGMIND.md) §1-5 into the accepted, normative baseline that all later RFCs build on. The [product plan](../PRODUCT-PLAN.md) elaborates the system design and delivery phases.

## 2. Decision

**D1. Product.** pgmind is a PostgreSQL extension (plus a sync CLI and an MCP server, both deterministic) that makes the database a knowledge vault for AI agents: notes addressed by path, wiki-links and backlinks, tags and properties, sections and blocks with stable identity, append-only revision history with audited excision, and deterministic token-budgeted context assembly (`knowledge.context()`).

**D2. The defining constraint.** pgmind MUST NOT execute or invoke any AI model, anywhere in the product — extension, CLI, or MCP server. No embedding generation, no summarization, no extraction, no LLM calls. Vector search is supported only as an *optional lane*: pgvector-typed hook tables the user populates with their own pipeline; retrieval blends the signal when present and the caller supplies query vectors.

**D3. Users.** Primary: developers of server-side AI applications and agent systems who would otherwise model memory as markdown files. Primary consumer of the API: agents via MCP; applications via SQL. The handbook §2 scenarios (shared agent brain, server-side Claude-Code-style memory, Obsidian-not-local, SQL-joined knowledge, auditable knowledge) and the product plan's Walkthroughs A-E (Part I §3: quickstart import, concurrent-safe writes, deterministic context, history, optional vector lane) are jointly the normative experience.

**D4. Scope of v1.** Markdown-native corpus (CommonMark + GFM subset + vault syntax per RFC-002); per-block storage and identity; versioning with agent-safe concurrency (CAS, atomic section append, block patch); deterministic link/tag/property extraction; filesystem/git two-way sync; FTS + structural retrieval; deterministic `context()` with local-tokenizer budgeting; MCP server; optional vector lane.

**D5. Non-goals of v1** (each requires a handbook amendment to enter scope): model execution of any kind; PDF/DOCX/HTML parsing (external converters produce markdown); conversational/episodic fact memory; building vector-index or BM25 engines (compose with pgvector/FTS); a general document store or collaborative CMS; distributed/federated knowledge; a hosted service.

**D6. Architecture laws.** The eleven laws in handbook §6.2 are binding on every subsequent RFC. The load-bearing ones for this RFC: Law 1 (AI-free core), Law 2 (no synchronous network I/O in any transaction or API call), Law 3 (markdown is a boundary; per-block relational storage), Law 4 (identity is minted on write, never derived by parsing), Law 8 (append-only with audited excision).

**D7. Delivery discipline.** Phases per product plan Part III; each phase's RFCs are accepted before its implementation starts; a phase exits only when its published benchmark gate passes. First public release (pgmind 0.1.0) is the Phase 5 vertical slice: import → query → history → MCP → deterministic `context()` — zero AI configured.

**D8. Identity of the project.** Working name **pgmind**; PostgreSQL license (ratified with rationale in RFC-001); the name decision is [OPEN] and MUST be resolved (with registrations) no later than the 0.1.0 release.

## 3. Alternatives considered

- **Application-layer library instead of an extension.** Rejected: the differentiating guarantees (transactional writes + extraction + history in one commit; RLS multi-tenancy; SQL-joined retrieval) live where the data lives. A library reintroduces the sync-glue problem pgmind exists to delete. The audit's precedent analysis shows libraries are the right home for *AI* workloads — which is exactly why the AI stays outside (D2) while the knowledge model goes inside.
- **Files + git as the substrate, with better tooling.** Rejected for the server case: no concurrent-writer safety, no queries, no RLS; git history is per-commit, not per-block, and unusable as a live write path for many agents. Explicitly the right answer for local single-writer use — the sync bridge exists so both worlds cooperate rather than compete.
- **Full RAG pipeline inside the database** (the v0.1 handbook's implicit direction). Rejected on the strongest evidence in the audit: pgrag, PostgresML/Korvus, and pgai all died or retreated on this exact pattern (network-bound model calls in the backend; undeployable on managed platforms).
- **General document store / Notion-like product.** Rejected: crowded, different buyer, and it would spend the novelty budget on collaboration UX instead of the empty layer (identity, history, context).

## 4. Consequences

*Easier:* trust and reviewability (a deterministic, no-egress extension is the easiest artifact to audit and, eventually, allowlist); testing (everything is reproducible); composition with the existing Postgres AI stack.
*Harder:* semantic recall requires the user to bring embeddings (mitigated by the hook/queue design and recipes); "magic" demos that competitors get from built-in LLM calls are off the table by design.
*Impossible until amended:* any feature requiring pgmind to call a model. Reversing D2 requires amending handbook Law 1 first (per the precedence rule) and would forfeit the project's stated survival argument — the bar is intentionally that high.

## 5. Benchmark gate

Phase 0 gate (stated identically here, in RFC-001 §5, and in the product plan): (a) the skeleton extension builds and its tests pass via `cargo pgrx test` on PostgreSQL 16, 17, and 18 in CI; (b) `make eval` runs the harness end-to-end and emits `eval/results/latest.json` (suites may report *pending*; the harness itself must work); (c) `make lint` (fmt + clippy) is clean, run in the pg18 CI leg. Additionally, this RFC is only satisfiable at 0.1.0 if product-plan Walkthroughs A-D execute as written; that check is carried by the Phase 5 gates.

## 6. Law compliance

This RFC *establishes* the laws' authority (D6) and adds none. D2 restates Laws 1-2 as product identity. No law is violated.
