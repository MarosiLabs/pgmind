# eval/ — benchmarks, corpora, published results

The evaluation harness enforces the phase gates defined in [PRODUCT-PLAN.md Part III](../docs/PRODUCT-PLAN.md). Run with `make eval` from the repo root; the report lands in `eval/results/latest.json` (gitignored — published numbers are committed deliberately alongside phase exits).

- `harness.py` — suite runner. Suites report `ok` / `fail` / `pending`.
- `corpora/` — fetched or curated test corpora (gitignored when downloadable; the adversarial edit corpus for rebinding will be committed, since it *is* a deliverable).
- `results/` — machine-readable reports.

Rules: gates are defined at RFC acceptance (no gate, no acceptance); numbers get published even when unflattering — the rebinding match-rate especially.
