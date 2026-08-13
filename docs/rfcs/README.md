# pgmind RFC Index

Canonical index — mirrors [handbook §12](../PGMIND.md) and the [product plan Part III](../PRODUCT-PLAN.md), which defines what each RFC must decide. RFCs follow [TEMPLATE.md](TEMPLATE.md); lifecycle is *living during phase, frozen at phase exit*. An RFC is accepted only when its benchmark gate is defined.

| RFC | Title | Phase | Status |
|---|---|---|---|
| [000](RFC-000-vision-and-scope.md) | Vision & Scope | 0 | **Frozen 2026-08-05** (Phase 0 exited) |
| [001](RFC-001-implementation-platform.md) | Implementation Platform | 0 | **Frozen 2026-08-05** (Phase 0 exited) |
| [002](RFC-002-markdown-type-ast-vault-syntax.md) | Markdown Type, AST & Vault Syntax | 1 | **Frozen 2026-08-05** (Phase 1 exited) |
| [003](RFC-003-vault-and-block-storage-layout.md) | Vault & Block Storage Layout | 2 | **Frozen 2026-08-05** (Phase 2 exited) |
| [004](RFC-004-block-identity-and-rebinding.md) | Block Identity & Rebinding Semantics | 2-3 | **Frozen 2026-08-06** (Phase 3 exited). Part A accepted 2026-08-05; Part B rewritten from corpus measurement, accepted and shipped 2026-08-06 |
| [005](RFC-005-version-engine-concurrency-and-excision.md) | Version Engine, Concurrency Semantics & Excision | 3 | **Frozen 2026-08-06** (Phase 3 exited) |
| [006](RFC-006-sync-bridge-and-import-export.md) | ~~Sync Bridge & Import/Export~~ | ~~4~~ | **Withdrawn 2026-08-09** — never accepted, never built. Phase 4 cut with it; the exportability law 4 promises ships as [`scripts/`](../../scripts/) + the `folder-round-trip` gate |
| [007](RFC-007-query-api-and-mcp-surface.md) | Query API & MCP Surface | 5 | **Draft 2026-08-13** — proposed for acceptance; query half implemented and gated, MCP half designed and not built |
| 008 | Deterministic Context Assembly & Token Budgeting | 5, matured 7 | not started |
| 009 | Optional Vector Lane (pgvector hooks) | 6 | not started |
| 010 | Retrieval Planner (incl. BM25 adapter decision) | 7 | not started |
| [011](RFC-011-provenance.md) | Provenance | 3 | **Frozen 2026-08-06** (Phase 3 exited) |
| 012 | Packaging & Distribution | 5+ | not started |
