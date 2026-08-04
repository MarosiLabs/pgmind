#!/usr/bin/env python3
"""pgmind evaluation harness.

Runs every registered gate suite and writes a machine-readable report to
eval/results/latest.json. Suite statuses:

  ok      - ran and met its thresholds
  fail    - ran and missed its thresholds (harness exits non-zero)
  pending - awaiting a phase deliverable

Phase 1 suites (RFC-002 §5) delegate to the pgmind-eval binary in core/
(pure Rust, no Postgres needed). Network use here is dev-time corpus
fetching only - never part of the product (Law 2).
"""

import json
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
COMMONMARK_SPEC_URL = "https://spec.commonmark.org/0.31.2/spec.json"
FUZZ_COUNT = "100000"


def eval_bin():
    """Build (once) and return the pgmind-eval binary path."""
    subprocess.run(
        ["cargo", "build", "--release", "--features", "conformance", "--bin", "pgmind-eval"],
        cwd=REPO / "core",
        check=True,
        capture_output=True,
    )
    return REPO / "core" / "target" / "release" / "pgmind-eval"


def run_eval(*args):
    out = subprocess.run([str(eval_bin()), *args], check=True, capture_output=True, text=True)
    return json.loads(out.stdout)


def commonmark_corpus():
    corpus = ROOT / "corpora" / "commonmark" / "spec-0.31.2.json"
    if not corpus.exists():
        corpus.parent.mkdir(parents=True, exist_ok=True)
        print(f"  fetching {COMMONMARK_SPEC_URL} ...")
        urllib.request.urlretrieve(COMMONMARK_SPEC_URL, corpus)
    return corpus


def suite_commonmark_conformance():
    """RFC-002 gate 1: all 652 spec examples, pure CommonMark configuration."""
    return run_eval("conformance", str(commonmark_corpus()))


def suite_round_trip():
    """RFC-002 gate 2: byte-identical tiling on repo docs + 100k fuzz documents."""
    corpus = run_eval(
        "roundtrip",
        str(REPO / "docs"),
        str(REPO / "PGMIND.md"),
        str(REPO / "AUDIT.md"),
        str(REPO / "README.md"),
        str(REPO / "CONTRIBUTING.md"),
    )
    fuzz = run_eval("fuzz-roundtrip", FUZZ_COUNT)
    status = "ok" if corpus["status"] == "ok" and fuzz["status"] == "ok" else "fail"
    return {"status": status, "corpus": corpus, "fuzz": fuzz}


def suite_hash_stability():
    """RFC-002 gate 3: golden vectors for D7 normalization."""
    return run_eval("hash-goldens", str(ROOT / "corpora" / "pgmind" / "hash-goldens.json"))


def suite_vault_syntax_extraction():
    """RFC-002 gate 4: golden corpus for D3/D4 extraction."""
    return run_eval(
        "extraction-goldens", str(ROOT / "corpora" / "pgmind" / "extraction-goldens.json")
    )


def suite_parse_performance():
    """RFC-002 gate 5: pathological constructions parse within fixed bounds."""
    return run_eval("perf")


SUITES = {
    "commonmark-conformance": suite_commonmark_conformance,
    "round-trip": suite_round_trip,
    "hash-stability": suite_hash_stability,
    "vault-syntax-extraction": suite_vault_syntax_extraction,
    "parse-performance": suite_parse_performance,
    # Phase 2 (RFC-003/004): identity-semantics, extraction-correctness (seeded by
    #                        vault-syntax-extraction), tenant-isolation
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
