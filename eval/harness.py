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

import glob
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
COMMONMARK_SPEC_URL = "https://spec.commonmark.org/0.31.2/spec.json"
FUZZ_COUNT = "100000"
STORAGE_FUZZ_COUNT = "10000"
CAPACITY_NOTES = 10_000


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


# ---------------------------------------------------------------------------
# Phase 2 infrastructure: a scratch Postgres cluster with the extension
# installed (RFC-003/004 suites run against real storage).
# ---------------------------------------------------------------------------


def find_pg_config() -> Path:
    env = os.environ.get("PGMIND_PG_CONFIG")
    if env:
        return Path(env)
    candidates = sorted(glob.glob(str(Path.home() / ".pgrx" / "*" / "pgrx-install" / "bin" / "pg_config")))
    if not candidates:
        raise RuntimeError("no pg_config found; set PGMIND_PG_CONFIG or run `make setup`")
    return Path(candidates[-1])


class PgCluster:
    """A throwaway cluster (unix-socket only) with pgmind installed."""

    def __init__(self):
        self.pg_config = find_pg_config()
        self.bindir = Path(
            subprocess.run([str(self.pg_config), "--bindir"], check=True, capture_output=True, text=True).stdout.strip()
        )
        version = subprocess.run([str(self.pg_config), "--version"], check=True, capture_output=True, text=True).stdout
        self.major = re.search(r"PostgreSQL (\d+)", version).group(1)
        self.dir = Path(tempfile.mkdtemp(prefix="pgmind-eval-"))
        self.data = self.dir / "data"
        self.sock = self.dir

    def install_extension(self):
        subprocess.run(
            [
                "cargo", "pgrx", "install", "--release",
                "--no-default-features", "--features", f"pg{self.major}",
                "--pg-config", str(self.pg_config),
            ],
            cwd=REPO / "extension",
            check=True,
            capture_output=True,
        )

    def start(self):
        subprocess.run(
            [str(self.bindir / "initdb"), "-D", str(self.data), "-U", "pgmind", "--no-sync", "-A", "trust"],
            check=True, capture_output=True,
        )
        conf = self.data / "postgresql.conf"
        conf.write_text(
            conf.read_text()
            + f"\nlisten_addresses = ''\nunix_socket_directories = '{self.sock}'\n"
            + "fsync = off\nfull_page_writes = off\n"
        )
        subprocess.run(
            [str(self.bindir / "pg_ctl"), "-D", str(self.data), "-l", str(self.dir / "log"), "start"],
            check=True, capture_output=True,
        )

    def createdb(self, name: str):
        subprocess.run(
            [str(self.bindir / "createdb"), "-h", str(self.sock), "-U", "pgmind", name],
            check=True, capture_output=True,
        )
        self.psql(name, "CREATE EXTENSION pgmind;")

    def psql(self, db: str, sql: str = None, file: Path = None, tuples_only: bool = False) -> str:
        cmd = [str(self.bindir / "psql"), "-X", "-q", "-v", "ON_ERROR_STOP=1",
               "-h", str(self.sock), "-U", "pgmind", "-d", db]
        if tuples_only:
            cmd.append("-tA")
        if file is not None:
            cmd += ["-f", str(file)]
        else:
            cmd += ["-c", sql]
        out = subprocess.run(cmd, capture_output=True, text=True)
        if out.returncode != 0:
            raise RuntimeError(f"psql failed: {out.stderr[-2000:]}")
        return out.stdout

    def dump(self, db: str, target: Path):
        subprocess.run(
            [str(self.bindir / "pg_dump"), "-h", str(self.sock), "-U", "pgmind", "-f", str(target), db],
            check=True, capture_output=True,
        )

    def restore_plain(self, db: str, dump_file: Path):
        """Plain autocommit psql restore — deliberately no --single-transaction
        (RFC-003 §5 gate 6 forbids the crutch)."""
        self.psql(db, file=dump_file)

    def stop(self):
        subprocess.run(
            [str(self.bindir / "pg_ctl"), "-D", str(self.data), "stop", "-m", "immediate"],
            capture_output=True,
        )
        shutil.rmtree(self.dir, ignore_errors=True)


_CLUSTER: "PgCluster | None" = None


def cluster() -> PgCluster:
    global _CLUSTER
    if _CLUSTER is None:
        c = PgCluster()
        print(f"  installing extension (pg{c.major}) + starting scratch cluster ...")
        c.install_extension()
        c.start()
        _CLUSTER = c
    return _CLUSTER


def dollar_quote(text: str) -> str:
    n = 0
    while f"$pgdq{n}$" in text:
        n += 1
    return f"$pgdq{n}${text}$pgdq{n}$"


def load_vault_sql(docs: list[tuple[str, str]]) -> str:
    """SQL that stages (path, source) pairs and writes them all through
    knowledge.write — the real Phase 2 write path, one server-side pass."""
    lines = ["CREATE TABLE IF NOT EXISTS public.rt_staging (path text PRIMARY KEY, src text);"]
    for path, src in docs:
        lines.append(f"INSERT INTO public.rt_staging VALUES ({dollar_quote(path)}, {dollar_quote(src)});")
    lines.append("SELECT count(knowledge.write(path, src::markdown)) FROM public.rt_staging;")
    return "\n".join(lines)


def repo_docs() -> list[tuple[str, str]]:
    files = [REPO / "PGMIND.md", REPO / "AUDIT.md", REPO / "README.md", REPO / "CONTRIBUTING.md"]
    files += sorted((REPO / "docs").rglob("*.md"))
    docs = []
    for i, f in enumerate(files):
        text = f.read_text()
        if len(text.encode()) < 8 * 1024 * 1024:
            docs.append((f"repo/doc-{i}", text))
    return docs


# ---------------------------------------------------------------------------
# Phase 2 suites (RFC-003 §5, RFC-004 §5)
# ---------------------------------------------------------------------------

# The pg_test binary is the executable form of the identity/extraction/tenant
# goldens; the harness classifies its tests into the RFC's suite names.
PGRX_TEST_SUITES = {
    "identity-semantics": [
        "idempotent_write_returns_head_no_new_revision",
        "edited_paragraph_mints_untouched_carry",
        "pure_reorder_carries_all",
        "duplicate_content_pairs_kth_to_kth",
        "ref_claim_beats_hash_and_collisions_resolve",
        "kind_change_via_claim",
        "update_block_keeps_id_changes_hash",
        "update_item_checkbox_toggle",
        "update_inner_paragraph_directly",
        "move_block_separator_synthesis",
        "move_last_block_earlier_and_back",
        "insert_blocks_at_end_and_anchored",
        "split_first_keeps_id",
        "merge_keeps_chosen_id",
        "child_carried_while_parent_removed",
        "typed_error_sqlstates",
    ],
    "extraction-correctness": [
        "write_read_byte_faithful",
        "resolution_lifecycle_missing_then_resolved",
        "backlinks_tags_orphans",
        "churn_discipline_one_paragraph_edit",
        "read_section_first_match",
    ],
    "tenant-isolation": [
        "tenant_scoping_and_grant_boundary",
    ],
}

_PGRX_TEST_RESULT: "dict | None" = None


def pgrx_test_results() -> dict:
    """Run `cargo pgrx test` once; return {test_name: passed}."""
    global _PGRX_TEST_RESULT
    if _PGRX_TEST_RESULT is None:
        major = cluster().major
        out = subprocess.run(
            ["cargo", "pgrx", "test", f"pg{major}"],
            cwd=REPO / "extension", capture_output=True, text=True,
        )
        results = {}
        for line in out.stdout.splitlines():
            m = re.match(r"test \S*tests::pg_(\w+) \.\.\. (ok|FAILED)", line.strip())
            if m:
                results[m.group(1)] = m.group(2) == "ok"
        _PGRX_TEST_RESULT = {"tests": results, "raw_ok": out.returncode == 0}
    return _PGRX_TEST_RESULT


def pg_test_suite(name: str):
    res = pgrx_test_results()
    wanted = PGRX_TEST_SUITES[name]
    missing = [t for t in wanted if t not in res["tests"]]
    failed = [t for t in wanted if not res["tests"].get(t, False)]
    ok = not missing and not failed
    return {
        "status": "ok" if ok else "fail",
        "total": len(wanted),
        "passed": len(wanted) - len(failed),
        "missing": missing,
        "failed": failed,
    }


def suite_identity_semantics():
    """RFC-004 §5 Part A: per-op identity outcomes + A3 carry, 100%."""
    return pg_test_suite("identity-semantics")


def suite_extraction_correctness():
    """RFC-003 §5 gate 2: extraction through storage + resolution lifecycle + churn."""
    return pg_test_suite("extraction-correctness")


def suite_tenant_isolation():
    """RFC-003 §5 gate 4: RLS scoping + grant-anchored boundary."""
    return pg_test_suite("tenant-isolation")


def suite_storage_round_trip():
    """RFC-003 §5 gate 3: write() → read() byte-identical through the tables,
    repo corpus + seeded fuzz sample; verify_note empty everywhere."""
    c = cluster()
    c.createdb("pgmind_rt")
    fuzz = json.loads(
        subprocess.run([str(eval_bin()), "emit-fuzz", STORAGE_FUZZ_COUNT],
                       check=True, capture_output=True, text=True).stdout
    )
    docs = repo_docs() + [(f"fuzz/{i}", d) for i, d in enumerate(fuzz) if d.strip()]
    sql = ROOT / "results" / "_rt_load.sql"
    sql.parent.mkdir(parents=True, exist_ok=True)
    sql.write_text(load_vault_sql(docs))
    c.psql("pgmind_rt", file=sql)
    sql.unlink()
    mismatches = int(c.psql(
        "pgmind_rt",
        "SELECT count(*) FROM public.rt_staging s WHERE knowledge.read(s.path)::text <> s.src;",
        tuples_only=True,
    ).strip())
    violations = int(c.psql(
        "pgmind_rt",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())
    ok = mismatches == 0 and violations == 0
    return {"status": "ok" if ok else "fail", "documents": len(docs),
            "read_mismatches": mismatches, "verify_violations": violations}


def synthetic_note(rng: random.Random, i: int) -> str:
    """~23 blocks calibrated to the Walkthrough-A shape (RFC-003 D8)."""
    tag = f"t{i % 20}"
    other = rng.randrange(CAPACITY_NOTES)
    parts = [f"# Note {i}\n\n"]
    for p in range(4):
        parts.append(f"Paragraph {p} of note {i} links [[cap/{(other + p) % CAPACITY_NOTES}]] and mentions #{tag}.\n\n")
    parts.append("## Details\n\n")
    parts.append("".join(f"- point {j} of note {i}\n" for j in range(8)))
    parts.append("\n```\ncode sample\n```\n")
    return "".join(parts)


def suite_capacity_model():
    """RFC-003 §5 gate 5 / D8: measured bytes + throughput + latency, published
    honestly with extrapolation to the plan-§14 design target."""
    c = cluster()
    c.createdb("pgmind_cap")
    rng = random.Random(0xC0FFEE)
    docs = [(f"cap/{i}", synthetic_note(rng, i)) for i in range(CAPACITY_NOTES)]
    sql = ROOT / "results" / "_cap_load.sql"
    sql.write_text(load_vault_sql(docs))
    t0 = time.monotonic()
    c.psql("pgmind_cap", file=sql)
    elapsed = time.monotonic() - t0
    sql.unlink()

    sizes = json.loads(c.psql("pgmind_cap", """
        SELECT json_object_agg(t.relname, json_build_object(
                 'total_bytes', pg_total_relation_size(t.oid),
                 'heap_bytes', pg_relation_size(t.oid),
                 'index_bytes', pg_indexes_size(t.oid)))
        FROM (SELECT c.oid, c.relname FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'pgmind' AND c.relkind = 'r') t;
    """, tuples_only=True).strip())
    counts = json.loads(c.psql("pgmind_cap", """
        SELECT row_to_json(s) FROM knowledge.stats() s;
    """, tuples_only=True).strip())
    latencies = json.loads(c.psql("pgmind_cap", """
        CREATE TEMP TABLE lat (fn text, ms double precision);
        DO $$
        DECLARE t0 timestamptz; i int;
        BEGIN
          FOR i IN 1..100 LOOP
            t0 := clock_timestamp();
            PERFORM knowledge.read('cap/' || (i * 97 % 10000));
            INSERT INTO lat VALUES ('read', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.backlinks('cap/' || (i * 89 % 10000));
            INSERT INTO lat VALUES ('backlinks', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.tagged('t' || (i % 20));
            INSERT INTO lat VALUES ('tagged', extract(epoch FROM clock_timestamp() - t0) * 1000);
          END LOOP;
        END $$;
        SELECT json_object_agg(fn, p95) FROM (
          SELECT fn, percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95
          FROM lat GROUP BY fn) x;
    """, tuples_only=True).strip().splitlines()[-1])

    blocks = counts["blocks"]
    total_bytes = sum(v["total_bytes"] for v in sizes.values())
    report = {
        "status": "ok",  # gate 5 publishes honest numbers; no flattery threshold
        "scale": {"notes": counts["notes"], "blocks": blocks,
                  "edges": counts["edges_resolved"] + counts["edges_dangling"],
                  "tags": counts["tags"]},
        "write_throughput_notes_per_s": round(counts["notes"] / elapsed, 1),
        "design_target_notes_per_s": 2000,
        "bytes": sizes,
        "bytes_per_block_all_in": round(total_bytes / blocks, 1),
        "latency_p95_ms": latencies,
        "extrapolation_100k_notes_10m_blocks": {
            "assumption": "linear in blocks; revision-load behavior modeled only until Phase 3 measures it",
            "projected_total_gb": round(total_bytes / blocks * 10_000_000 / 1e9, 2),
        },
    }
    out = ROOT / "results" / "capacity-model.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    return report


def suite_dump_restore():
    """RFC-003 §5 gate 6: pg_dump → plain autocommit psql restore → equal
    counts, advancing sequences, verify_note clean, post-restore write works."""
    c = cluster()
    c.createdb("pgmind_ref")
    sql = ROOT / "results" / "_ref_load.sql"
    sql.write_text(load_vault_sql(repo_docs()))
    c.psql("pgmind_ref", file=sql)
    sql.unlink()

    dump_file = c.dir / "ref.dump.sql"
    c.dump("pgmind_ref", dump_file)
    subprocess.run([str(c.bindir / "createdb"), "-h", str(c.sock), "-U", "pgmind", "pgmind_restored"],
                   check=True, capture_output=True)
    c.restore_plain("pgmind_restored", dump_file)

    count_sql = ("SELECT json_build_object("
                 "'note', (SELECT count(*) FROM pgmind.note),"
                 "'revision', (SELECT count(*) FROM pgmind.revision),"
                 "'tile', (SELECT count(*) FROM pgmind.tile),"
                 "'block', (SELECT count(*) FROM pgmind.block),"
                 "'edge', (SELECT count(*) FROM pgmind.edge),"
                 "'tag', (SELECT count(*) FROM pgmind.tag));")
    before = json.loads(c.psql("pgmind_ref", count_sql, tuples_only=True).strip())
    after = json.loads(c.psql("pgmind_restored", count_sql, tuples_only=True).strip())
    violations = int(c.psql(
        "pgmind_restored",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())
    # sequences must advance past restored PKs: a post-restore write succeeds
    post = c.psql(
        "pgmind_restored",
        "SELECT knowledge.write('post-restore/probe', ('after restore [[repo/doc-0]] #ok')::markdown);",
        tuples_only=True,
    ).strip()
    ok = before == after and violations == 0 and len(post) == 36
    return {"status": "ok" if ok else "fail", "counts_before": before,
            "counts_after": after, "verify_violations": violations,
            "post_restore_write": bool(len(post) == 36)}


SUITES = {
    "commonmark-conformance": suite_commonmark_conformance,
    "round-trip": suite_round_trip,
    "hash-stability": suite_hash_stability,
    "vault-syntax-extraction": suite_vault_syntax_extraction,
    "parse-performance": suite_parse_performance,
    # Phase 2 (RFC-003 §5 / RFC-004 §5)
    "identity-semantics": suite_identity_semantics,
    "extraction-correctness": suite_extraction_correctness,
    "storage-round-trip": suite_storage_round_trip,
    "tenant-isolation": suite_tenant_isolation,
    "capacity-model": suite_capacity_model,
    "dump-restore": suite_dump_restore,
    # Phase 3 (RFC-004/005): rebinding-edit-corpus, concurrency, storage-growth
    # Phase 4 (RFC-006):     sync-round-trip (incl. unicode/case collisions), torture
    # Phase 5 (RFC-007/008): context-determinism, quality-per-token
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

    if _CLUSTER is not None:
        _CLUSTER.stop()

    out = ROOT / "results" / "latest.json"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {out}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
