# RFC-001: Implementation Platform

- **Status:** Frozen (Phase 0 exited: CI matrix green on PG16/17/18, eval + lint passing — run 30930266479/30930762648)
- **Phase:** 0
- **Owner:** project author
- **Created:** 2026-08-04 · **Accepted:** 2026-08-05 · **Frozen:** 2026-08-05

## 1. Context

The audit faulted the original handbook for starting a "documentation drives code" project with zero foundational technology decisions: no language, no parser, no markdown flavor, no tokenizer, no version matrix (finding M8) — and no license (finding C5). The [handbook](../PGMIND.md) §7 subsequently made these decisions as revisitable defaults; this RFC ratifies them as the accepted platform with the evidence and the accepted costs on record, so no later phase relitigates them casually.

## 2. Decision

**D1. Extension language & framework: Rust on pgrx, pinned at the current release (0.19.2 at acceptance).**
`pgrx = "=0.19.2"` and `cargo-pgrx 0.19.2` (kept in lockstep — pgrx requires it). Rationale: pgrx is the proven modern path (ParadeDB pg_search, Timescale pgvectorscale, Supabase pg_graphql ship on it); memory safety matters more than usual for a parser-heavy extension whose panics must become clean Postgres errors, not backend crashes. Accepted costs, on the record: pgrx is pre-1.0 (breaking changes expected), builds are per-PG-major-version, and compile times are heavy. Version bumps are deliberate in-repo amendments (Cargo.toml + Makefile + CI + rust-toolchain.toml together), not new RFCs. (Phase 0 empirical note: 0.18.1 was tried first to spare an older local rustc, and failed — its own test harness's dependency graph requires rustc ≥ 1.95 — so "old pin for old toolchain" is not a real option; stay current.)

**D2. Rust toolchain.** Stable channel, pinned exactly via `rust-toolchain.toml` (1.97.1 at acceptance) so local and CI builds agree; rustup installs it automatically and the repo pin never touches a contributor's default toolchain. Bumps ride the same process as D1.

**D3. Markdown engine: comrak.** The production-grade CommonMark + GFM implementation in Rust, with per-node source positions (line/column-based; byte spans are derived via a line-offset table — see RFC-002 D5; required for the block model and rebinding). Pinned in `Cargo.toml` when Phase 1 starts. Vault syntax (wiki-links `[[…]]`, tags, block refs `^id`, transclusions `![[…]]`) is *not* upstream comrak behavior: it is specced in RFC-002 and implemented as our own deterministic pass over comrak's AST — we define our spec explicitly rather than chasing Obsidian bug-compatibility.

**D4. Spec anchor: CommonMark + GFM subset** (tables, task lists, strikethrough) as the block taxonomy's foundation; anything beyond (including the vault syntax) requires RFC-002 treatment. The CommonMark conformance suite is a permanent CI fixture from Phase 1.

**D5. PostgreSQL version matrix: 16, 17, 18.** All three built and smoke-tested in CI from Phase 0 onward. New PG majors are added when pgrx supports them; dropping a major requires owner sign-off.

**D6. License: the PostgreSQL License**, in-repo as `LICENSE` from Phase 0. Rationale (audit §3.3): every ubiquitous, allowlisted extension is permissively licensed; ParadeDB's AGPL demonstrably capped distribution; the entire eventual-allowlisting thesis depends on this. Any commercial layer, if ever, lives in tooling/services — never the extension.

**D7. Tokenizer strategy (for Law 2 compliance).** Token budgeting uses vendored BPE vocabularies executed locally in the extension (cl100k/o200k-class), pluggable per model family. Candidate crates are evaluated at Phase 5; RFC-008 fixes the vocabularies and constants. No tokenizer may fetch anything at runtime.

**D8. Repository & build layout.** `extension/` is a standalone pgrx crate (named `pgmind`) for Phases 0-3 — pgrx's required build profiles live in a crate manifest, and Cargo ignores member-level profiles in a workspace. When the first `tools/` crate lands (Phases 4-5: sync CLI, MCP server), the repo converts to a Cargo workspace with profiles at the workspace root. `eval/` is a Python harness — dev-time tooling may use Python; product artifacts are Rust. `Makefile` targets `build`, `test`, `lint`, `eval` are the canonical entry points; CI runs exactly what contributors run.

**D9. CI.** GitHub Actions: a `{pg16, pg17, pg18}` matrix job building and running `cargo pgrx test` (with `make lint` in the pg18 leg), plus an `eval` job running the harness. PGDG apt provides Postgres in CI — both the server package and `-server-dev` headers (the dev package alone lacks `initdb`/`postgres`, which the test cluster needs); `cargo pgrx init` binds to the matrix version's `pg_config`. Caching keeps the pgrx compile cost tolerable.

**D10. Schema naming (proposed here, ratified in RFC-007):** public API in schema `knowledge`, internal storage in schema `pgmind`.

## 3. Alternatives considered

- **C.** The classic path (PostGIS, pgvector). Rejected: for a parser-and-planner-heavy codebase, memory safety and the Rust markdown ecosystem (comrak) outweigh C's lower-level control; the pgrx precedents prove production viability. Cost accepted: pgrx pre-1.0 churn.
- **pulldown-cmark** (parser). Rejected: event/pull-based with no materialized AST by default and weaker GFM/sourcepos ergonomics; comrak's tree + sourcepos maps directly onto the block model.
- **tree-sitter-markdown.** Rejected as the parser (split block/inline grammars, conformance gaps), noted as prior art for future incremental re-parsing ideas — its node reuse is an in-memory optimization, not identity, which is exactly the audit's C1 lesson.
- **PL/pgSQL-only "trusted language" variant** (pg_tle-deployable). Rejected for v1: cannot implement a custom type, a real parser, or the planner. Recorded as a possible future degraded tier — a decision for a future RFC, not this one.
- **Apache-2.0 license.** Viable, but the PostgreSQL License is the community's native choice for extensions and removes even theoretical friction; nothing about pgmind needs Apache's patent clause more than it needs frictionless adoption.

## 4. Consequences

*Easier:* safety, testing (pgrx's `#[pg_test]` runs tests inside a real Postgres), recruiting from the modern extension ecosystem, eventual allowlisting conversations (deterministic Rust, no egress, permissive license).
*Harder:* contributors need the Rust toolchain + cargo-pgrx at the pinned versions (mitigated: `make setup` documents it; CI is authoritative); per-PG-major builds triple CI cost (mitigated: caching); pgrx upgrades are periodic deliberate work.
*Impossible until amended:* shipping to pg_tle-only environments; using parser features comrak lacks without patching upstream or post-processing.

## 5. Benchmark gate

Phase 0 gate (stated identically here, in RFC-000 §5, and in the product plan): (a) the skeleton extension (`pgmind_version()` smoke function + `#[pg_test]`) builds and its tests pass via `cargo pgrx test` on PostgreSQL 16, 17, and 18 in CI; (b) `make eval` runs the harness end-to-end and emits `eval/results/latest.json` (suites may report *pending*; the harness itself must work); (c) `make lint` (fmt + clippy) is clean, run in the pg18 CI leg. Local development on any one PG major suffices; the matrix is CI's job.

## 6. Law compliance

- **Law 1 (AI-free core):** nothing in the platform links or calls model runtimes; the eval harness is dev-time only.
- **Law 2 (no sync network I/O):** D7 mandates vendored, local tokenization; no build artifact fetches at runtime.
- **Law 6 (compose with incumbents):** comrak/pgvector/FTS are dependencies, not reimplementations.
- **Law 9 (feel like PostgreSQL):** D6 license, D5 version matrix, and D10 schema naming all serve Postgres-native expectations.
No law is violated.
