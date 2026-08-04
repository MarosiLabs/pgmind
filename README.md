# pgmind

**A brain for AI agents, inside PostgreSQL.**

An Obsidian-shaped knowledge vault — notes, wiki-links, backlinks, tags, block-level history — living in the database instead of on a filesystem: safe for many agents to read and write concurrently, queryable in SQL, and able to hand any agent exactly the context it needs with one deterministic call:

```sql
SELECT knowledge.context(root => 'projects/auth', token_budget => 12000);
```

**No AI is anywhere in the middle.** pgmind never calls a model. Vector search is an optional lane you populate yourself with pgvector.

## Status

**Pre-alpha — Phase 0 (groundwork).** Nothing is usable yet. First public release (0.1.0) is the Phase 5 vertical slice: import a markdown vault → query/history in SQL → MCP server → deterministic `context()`.

## Documents

| Document | Role |
|---|---|
| [PGMIND.md](PGMIND.md) | The handbook — vision, philosophy, architecture laws (the constitution) |
| [docs/PRODUCT-PLAN.md](docs/PRODUCT-PLAN.md) | The operating blueprint — system design + phased delivery plan |
| [docs/rfcs/](docs/rfcs/README.md) | Per-phase RFCs, written and accepted before implementation |
| [AUDIT.md](AUDIT.md) | The evidence base — audit of the original vision against 2025-26 research |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Roles, governance, RFC lifecycle |

## Development

Requirements: Rust (pinned via `rust-toolchain.toml`; rustup installs it automatically), Python 3.10+ (eval harness only). On macOS: `brew install pkgconf icu4c` (needed to compile the pgrx-managed Postgres).

```bash
make setup   # install cargo-pgrx (pinned) and init a pgrx-managed Postgres
             # (compiles PG into ~/.pgrx — self-contained and writable; system installs
             #  often aren't: libpq's pg_config is client-only, and macOS blocks writing
             #  extensions into Postgres.app's protected bundle)
make build   # build the extension          (PG=18 by default; make build PG=16)
make test    # cargo pgrx test — runs tests inside a real Postgres
make lint    # fmt + clippy
make eval    # run the evaluation harness → eval/results/latest.json
```

## License

[PostgreSQL License](LICENSE).
