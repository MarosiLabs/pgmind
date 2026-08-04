# tools/ — deterministic companion tools (no AI here either)

Lands in Phases 4-5 (see [PRODUCT-PLAN.md](../docs/PRODUCT-PLAN.md)):

- `pgmind` CLI — `import` / `export` / `sync [--watch]` between a markdown folder and the vault (RFC-006).
- `pgmind-mcp` — the MCP server exposing the vault to agents (RFC-007).

When the first crate lands here, the repo converts to a Cargo workspace (profiles move to the workspace root; see RFC-001 D8).
