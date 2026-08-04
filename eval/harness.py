#!/usr/bin/env python3
"""pgmind evaluation harness (Phase 0 skeleton).

Runs every registered suite and writes a machine-readable report to
eval/results/latest.json. Suite statuses:

  ok      - ran and met its thresholds
  fail    - ran and missed its thresholds (harness exits non-zero)
  pending - awaiting a phase deliverable; the harness still exercises
            corpus discovery so "pending" proves the plumbing works

Gates are defined per RFC (see docs/PRODUCT-PLAN.md Part III). Network use
here is dev-time corpus fetching only - never part of the product (Law 2).
"""

import json
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
COMMONMARK_SPEC_URL = "https://spec.commonmark.org/0.31.2/spec.json"


def suite_commonmark_conformance():
    """Phase 1 gate (RFC-002): CommonMark spec conformance of the markdown type."""
    corpus = ROOT / "corpora" / "commonmark" / "spec-0.31.2.json"
    if not corpus.exists():
        corpus.parent.mkdir(parents=True, exist_ok=True)
        print(f"  fetching {COMMONMARK_SPEC_URL} ...")
        urllib.request.urlretrieve(COMMONMARK_SPEC_URL, corpus)
    examples = json.loads(corpus.read_text())
    return {
        "status": "pending",
        "reason": "markdown type lands in Phase 1 (RFC-002)",
        "corpus": str(corpus.relative_to(ROOT)),
        "examples": len(examples),
    }


SUITES = {
    "commonmark-conformance": suite_commonmark_conformance,
    # Phase 2 (RFC-003/004): identity-semantics, extraction-correctness, tenant-isolation
    # Phase 3 (RFC-004/005): rebinding-edit-corpus, concurrency, storage-growth
    # Phase 4 (RFC-006):     sync-round-trip (incl. unicode/case collisions), torture
    # Phase 5 (RFC-007/008): context-determinism, quality-per-token, dump-restore
}


def main() -> int:
    report = {
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "suites": {},
    }
    failed = False
    for name, fn in SUITES.items():
        print(f"suite: {name}")
        try:
            result = fn()
        except Exception as exc:  # a crashed suite is a failed suite
            result = {"status": "fail", "error": repr(exc)}
        report["suites"][name] = result
        print(f"  -> {result['status']}" + (f" ({result.get('reason')})" if result.get("reason") else ""))
        failed |= result["status"] == "fail"

    out = ROOT / "results" / "latest.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {out}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
