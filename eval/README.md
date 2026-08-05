# eval/ — benchmarks, corpora, published results

The evaluation harness enforces the phase gates defined in [PRODUCT-PLAN.md Part III](../docs/PRODUCT-PLAN.md). Run with `make eval` from the repo root; the report lands in `eval/results/latest.json` (gitignored — published numbers are committed deliberately alongside phase exits).

- `harness.py` — suite runner. Suites report `ok` / `fail` / `pending`. Phase 1 suites delegate to the pure-Rust `pgmind-eval` binary; Phase 2 suites (RFC-003 §5 / RFC-004 §5) install the extension into a throwaway unix-socket cluster (pg_config from `PGMIND_PG_CONFIG` or the pgrx-managed install) and exercise real storage: `identity-semantics` / `extraction-correctness` / `tenant-isolation` run the pg_test goldens, `storage-round-trip` pushes the repo corpus + 10k seeded fuzz documents through `write()`/`read()`/`verify_note`, `capacity-model` measures and writes `results/capacity-model.json` (published honestly — including the throughput number when it misses the design target), `dump-restore` proves `pg_dump` → plain autocommit `psql` restore.
- `corpora/` — fetched or curated test corpora (gitignored when downloadable; the adversarial edit corpus for rebinding will be committed, since it *is* a deliverable).
- `results/` — machine-readable reports (gitignored).
- `published/` — committed gate results at phase exits (`phase-1-gates.json`: all five RFC-002 suites ok).

Rules: gates are defined at RFC acceptance (no gate, no acceptance); numbers get published even when unflattering — the rebinding match-rate especially.
