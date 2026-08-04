# Contributing to pgmind

Process and roles live here, out of the product handbook ([PGMIND.md](PGMIND.md)). The handbook defines *what* is being built; this document defines *how work is organized*.

## Governance

One human owner — currently the project author — accepts RFCs, approves phase exits, and resolves scope disputes. Agent roles own work, not decisions. An RFC is accepted when the owner signs it; a phase exits when its RFC is frozen and its benchmark (handbook §9) is passed and published.

## RFC lifecycle

RFCs are **living during their phase, frozen at phase exit**. Amendments after freeze require a new RFC. The canonical RFC index is handbook §12 — no other list is authoritative. An RFC that violates an Architecture Law (handbook §6.2) must say so in its title.

## Roles

Adapted from the v0.1 handbook, updated for the v0.3 architecture (deterministic, AI-free extension core + external tools + eval harness).

### Product Manager
Owns roadmap, RFC prioritization, scope, and the non-goals list. Never writes implementation.

### Architect
Owns system architecture, RFC reviews, Architecture Law compliance, and long-term consistency across phases.

### Parser Engineer
Owns the markdown boundary: comrak integration, AST, validation, serialization, rendering, round-trip fidelity.

### Storage Engineer
Owns the extension's storage schema: note/block/revision/edge/tag tables, delta chains and keyframes, TOAST/partitioning/autovacuum strategy, excision mechanics, capacity model.

### Query Engineer
Owns SQL APIs, operators, functions, concurrency semantics (CAS, append-to-section, block patch), and execution planning inside the extension.

### Planner Engineer
Owns retrieval planning, deterministic context assembly, token budgeting, and plan introspection.

### Tooling / Ops Engineer *(new in v0.3)*
Owns everything that runs outside the database: the sync bridge CLI (import/export/two-way sync), the MCP server, packaging (Docker/CloudNativePG/PGXN/OCI), CI build matrix, and observability. Note: per Architecture Law 1, nothing in this scope calls a model either — the optional vector lane is documented as user-side recipes, not built as pgmind components.

### Eval / Benchmark Owner *(new in v0.2)*
Owns the eval harness and corpora (handbook §9): CommonMark conformance, the adversarial edit corpus for identity rebinding, retrieval/context quality-per-token benchmarks, and publication of results — including unflattering ones. No phase exits without this role's sign-off on the benchmark.

### Documentation Agent
Owns RFC editing, API docs, examples, tutorials, and the 5-minute quickstart.

## Working rules

- Build order (from v0.1, adapted): parser first, storage second, indexes third, planner fourth — and in v0.3 there is no AI to come last. Never build higher layers on unstable lower layers.
- Public APIs depend only on documented APIs of lower layers; admin/debug interfaces may reach deeper and must be marked as such.
- Every user-visible change updates the quickstart if it touches it; the quickstart must always pass.
