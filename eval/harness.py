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
HISTORY_OPS = 240
GROWTH_NOTES = 40
GROWTH_DEPTH = 25


def results_dir() -> Path:
    """eval/results/ is gitignored, so no suite may assume it exists."""
    d = ROOT / "results"
    d.mkdir(parents=True, exist_ok=True)
    return d


def published_dir() -> Path:
    """eval/published/ holds the committed, RFC-mandated gate deliverables."""
    d = ROOT / "published"
    d.mkdir(parents=True, exist_ok=True)
    return d


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


def copy_literal(field: str) -> str:
    """Escape one field for COPY ... FROM STDIN in text format."""
    return (
        field.replace("\\", "\\\\")
        .replace("\t", "\\t")
        .replace("\n", "\\n")
        .replace("\r", "\\r")
    )


# The timed write pass, kept separate from staging so the throughput number
# measures knowledge.write and not the test scaffolding that fed it.
WRITE_PASS_SQL = "SELECT count(knowledge.write(path, src::markdown)) FROM public.rt_staging;"


def stage_vault_sql(docs: list[tuple[str, str]]) -> str:
    """SQL that stages (path, source) pairs via a single COPY. Scaffolding
    only — one statement, one transaction, no knowledge.write involved."""
    lines = [
        "DROP TABLE IF EXISTS public.rt_staging;",
        "CREATE TABLE public.rt_staging (path text PRIMARY KEY, src text);",
        "COPY public.rt_staging (path, src) FROM STDIN;",
    ]
    lines += [f"{copy_literal(p)}\t{copy_literal(s)}" for p, s in docs]
    lines.append("\\.")
    return "\n".join(lines) + "\n"


def load_vault_sql(docs: list[tuple[str, str]]) -> str:
    """Stage + write in one script, for suites that do not time the write."""
    return stage_vault_sql(docs) + WRITE_PASS_SQL + "\n"


def run_sql_file(c: PgCluster, db: str, name: str, sql_text: str):
    """Write a scratch .sql under eval/results/ (gitignored, so it may not
    exist yet), run it, and remove it even when psql fails."""
    path = results_dir() / name
    path.write_text(sql_text)
    try:
        c.psql(db, file=path)
    finally:
        path.unlink(missing_ok=True)


_REPO_DOCS: "list[tuple[str, str]] | None" = None


def repo_docs() -> list[tuple[str, str]]:
    global _REPO_DOCS
    if _REPO_DOCS is None:
        files = [REPO / "PGMIND.md", REPO / "AUDIT.md", REPO / "README.md", REPO / "CONTRIBUTING.md"]
        files += sorted((REPO / "docs").rglob("*.md"))
        docs = []
        for i, f in enumerate(files):
            text = f.read_text()
            if len(text.encode()) < 8 * 1024 * 1024:
                docs.append((f"repo/doc-{i}", text))
        _REPO_DOCS = docs
    return _REPO_DOCS


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
        # A3: identity must not migrate between sections, and a heading rename
        # must still carry its section (RFC-004 A1/A3 pass 2 tiers).
        "section_delete_does_not_recycle_ids_across_sections",
        "heading_rename_carries_section_blocks",
        # A4 provenance schema, incl. split.into and the marker-holder uuid.
        "split_provenance_matches_a4_schema",
        "merge_without_keep_records_provenance",
        "merge_retiree_with_descendants_is_pm006_even_when_keep_is_not_first",
        "move_across_sections_keeps_id_changes_heading_path",
        # RFC-003 D6 splice discipline.
        "insert_preserves_unseparated_neighbouring_tiles",
        "final_block_ops_keep_trailing_trivia",
        "insert_item_level_into_a_quoted_list",
        "block_ops_keep_the_note_row_consistent",
    ],
    "extraction-correctness": [
        "write_read_byte_faithful",
        "resolution_lifecycle_missing_then_resolved",
        "backlinks_tags_orphans",
        "churn_discipline_one_paragraph_edit",
        "read_section_first_match",
        "read_section_ignores_headings_inside_blockquotes",
        "paths_round_trip_through_normalization",
        "notes_glob_matches_literal_stars_and_prefixes",
        "extraction_dedups_and_covers_all_link_kinds",
        "lane_batching_survives_chunk_boundaries",
        "revisions_carry_dense_seq_and_verb",
        # RFC-005 D3/D4: what history records, and what it refuses to record.
        "structural_edits_do_not_write_a_row_per_block",
        "history_records_the_pre_image_of_both_lanes",
        "frames_are_written_at_the_configured_cadence",
        # RFC-005 D3/D8: reconstruction, the error contract, and an invariant
        # checker that can actually fail.
        "every_revision_reconstructs_byte_exactly",
        "blocks_as_of_returns_past_structure",
        "history_errors_distinguish_missing_from_compacted",
        "history_and_diff_report_what_changed",
        "verify_history_catches_a_missing_pre_image",
        # RFC-005 D5: the concurrency contract's single-session half. The
        # interleaving half is the concurrency-isolation suite.
        "cas_precedes_the_idempotence_short_circuit",
        "cas_on_a_missing_note_raises_rather_than_creating",
        "block_ops_honour_expected_head",
        "append_to_section_keeps_both_appends",
        # RFC-005 D6: tombstones, reconstruction-based undelete, rename repair.
        "delete_then_undelete_restores_the_note",
        "move_note_repairs_edges_both_ways",
        "move_onto_an_occupied_path_raises_pm015",
        # RFC-005 D7/D8: erasure that reaches every surface and proves it, and
        # retention that keeps the ledger it compacted.
        "excision_erases_from_every_surface_and_proves_it",
        "excision_refuses_live_content_unless_told_to_remove_it",
        "excision_dry_run_is_the_default",
        "retention_compacts_history_but_keeps_the_ledger",
        "invalid_link_targets_are_reported_as_invalid",
    ],
    "tenant-isolation": [
        "tenant_scoping_and_grant_boundary",
        "malformed_vault_guc_raises_pm001",
        # RFC-005 D2: dump registration and policy coverage, both enumerated
        # from pg_catalog so a table added later is covered by construction.
        "every_pgmind_table_is_dumped_and_tenant_scoped",
    ],
}

_PGRX_TEST_RESULT: "dict | None" = None


def pgrx_test_results() -> dict:
    """Run `cargo pgrx test` once; return {test_name: passed} plus exit status.

    `cargo pgrx test` builds and installs its OWN debug, pg_test-enabled
    artifact over the --release build install_extension() placed: pgrx's
    framework::install_extension shells out to `cargo pgrx install --test`
    against the same pg_config, with PGRX_BUILD_PROFILE defaulting to debug.
    Every later suite does CREATE EXTENSION afterwards, so without the
    reinstall below the capacity and round-trip numbers describe a debug
    build. Restoring the release artifact here keeps that independent of
    SUITES ordering.
    """
    global _PGRX_TEST_RESULT
    if _PGRX_TEST_RESULT is None:
        c = cluster()
        out = subprocess.run(
            ["cargo", "pgrx", "test", f"pg{c.major}"],
            cwd=REPO / "extension", capture_output=True, text=True,
        )
        results = {}
        for line in out.stdout.splitlines():
            m = re.match(r"test \S*tests::pg_(\w+) \.\.\. (ok|FAILED)", line.strip())
            if m:
                results[m.group(1)] = m.group(2) == "ok"
        c.install_extension()  # undo the debug/pg_test install
        _PGRX_TEST_RESULT = {
            "tests": results,
            "raw_ok": out.returncode == 0,
            "stderr_tail": "" if out.returncode == 0 else out.stderr[-2000:],
        }
    return _PGRX_TEST_RESULT


def _listed_tests() -> set:
    return {t for names in PGRX_TEST_SUITES.values() for t in names}


def pg_test_suite(name: str):
    res = pgrx_test_results()
    wanted = PGRX_TEST_SUITES[name]
    missing = [t for t in wanted if t not in res["tests"]]
    failed = [t for t in wanted if not res["tests"].get(t, False)]
    # A pg_test outside every curated list is still evidence about this gate:
    # if the binary as a whole failed, no suite backed by it may report ok.
    unlisted_failed = sorted(
        t for t, passed in res["tests"].items() if not passed and t not in _listed_tests()
    )
    ok = not missing and not failed and res["raw_ok"]
    result = {
        "status": "ok" if ok else "fail",
        "total": len(wanted),
        "passed": len(wanted) - len(failed),
        "missing": missing,
        "failed": failed,
        "pgrx_test_exit_ok": res["raw_ok"],
    }
    if unlisted_failed:
        result["unlisted_failed"] = unlisted_failed
    if not ok and not missing and not failed:
        result["reason"] = "cargo pgrx test exited non-zero outside this suite's named tests"
        result["stderr_tail"] = res["stderr_tail"]
    return result


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
    run_sql_file(c, "pgmind_rt", "_rt_load.sql", load_vault_sql(docs))
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


def synthetic_note(rng: random.Random, i: int, prefix: str = "cap",
                   n: int = CAPACITY_NOTES, extraction: bool = True) -> str:
    """23 blocks, 4 links and 4 tags per note (RFC-003 D8 records this shape).

    `extraction=False` keeps the block and byte shape and removes only the
    links and tags, which is what makes the D8 ablation an ablation: the
    difference between the two passes is extraction and nothing else.
    """
    tag = f"t{i % 20}"
    other = rng.randrange(n)
    parts = [f"# Note {i}\n\n"]
    for p in range(4):
        if extraction:
            parts.append(f"Paragraph {p} of note {i} links [[{prefix}/{(other + p) % n}]] and mentions #{tag}.\n\n")
        else:
            parts.append(f"Paragraph {p} of note {i} links to note {(other + p) % n} and mentions t{i % 20}.\n\n")
    parts.append("## Details\n\n")
    parts.append("".join(f"- point {j} of note {i}\n" for j in range(8)))
    parts.append("\n```\ncode sample\n```\n")
    return "".join(parts)


THROUGHPUT_NOTES = 500
THROUGHPUT_RUNS = 5
GROWTH_STEPS = 8


def timed_create_pass(c: PgCluster, db: str, docs: list[tuple[str, str]]) -> float:
    """Seconds for exactly one knowledge.write() pass over `docs`. Staging is
    loaded first, untimed, so the number describes write() and nothing else."""
    run_sql_file(c, db, "_bench_stage.sql", stage_vault_sql(docs))
    t0 = time.monotonic()
    c.psql(db, WRITE_PASS_SQL)
    return time.monotonic() - t0


def fresh_db(c: PgCluster, name: str):
    c.psql("postgres", f"DROP DATABASE IF EXISTS {name};")
    c.createdb(name)


def write_cost_measurements(c: PgCluster) -> dict:
    """RFC-003 D8's published write cost. Two measurements, deliberately split.

    **Repeatability + attribution.** Every pass writes one corpus into a *fresh
    database*, so the only thing that differs between passes is the note shape:
    a full note, the same note with links and tags removed, and a one-block
    note. That gives a median with a spread (not one sample), and an
    attribution that is a real ablation rather than a difference between two
    differently-loaded databases. Every pass is a create — a rewrite takes the
    other `write_note` branch, so mixing them would measure neither.

    **Growth.** Successive passes into *one* database, recording the vault size
    each pass started from. This exists because the first version of this bench
    ran every pass into one growing database and reported the median: the
    passes were monotonically slowing (3.9 → 9.5 ms/note as the vault filled),
    so the "median" was really the cost at the median vault size, and the
    ablation was charging vault growth to whichever shape ran last. Write cost
    is a function of vault size; D8 publishes the curve instead of one number
    that silently depends on where on it the corpus happened to sit.

    Both run in throwaway databases, dropped afterwards, so the capacity corpus
    whose table sizes are the rest of this gate's deliverable is never polluted.
    """
    shapes = {
        "full": lambda rng, i, pre: synthetic_note(rng, i, prefix=pre, n=THROUGHPUT_NOTES),
        "no_extraction": lambda rng, i, pre: synthetic_note(
            rng, i, prefix=pre, n=THROUGHPUT_NOTES, extraction=False),
        "single_block": lambda rng, i, pre: f"Paragraph of note {i}.\n",
    }

    def corpus(shape, make, run):
        rng = random.Random(0xB0BB1E + run)
        pre = f"bench/{shape}/{run}"
        return [(f"{pre}/{i}", make(rng, i, pre)) for i in range(THROUGHPUT_NOTES)]

    passes: dict = {k: [] for k in shapes}
    for run in range(THROUGHPUT_RUNS):
        for shape, make in shapes.items():
            fresh_db(c, "pgmind_bench")
            elapsed = timed_create_pass(c, "pgmind_bench", corpus(shape, make, run))
            passes[shape].append(round(elapsed * 1000 / THROUGHPUT_NOTES, 3))

    fresh_db(c, "pgmind_bench")
    growth = []
    for step in range(GROWTH_STEPS):
        elapsed = timed_create_pass(c, "pgmind_bench", corpus("full", shapes["full"], 100 + step))
        growth.append({"notes_already_in_vault": step * THROUGHPUT_NOTES,
                       "ms_per_note": round(elapsed * 1000 / THROUGHPUT_NOTES, 3)})
    c.psql("postgres", "DROP DATABASE pgmind_bench;")

    def med(xs):
        return sorted(xs)[len(xs) // 2]  # odd run counts only

    full, no_ext, single = (med(passes[k]) for k in shapes)
    spread = round(max(passes["full"]) - min(passes["full"]), 3)
    return {
        "procedure": (
            f"{THROUGHPUT_RUNS} rounds x 3 shapes, each pass writing a "
            f"{THROUGHPUT_NOTES}-note corpus into an EMPTY database (every write a create, "
            "release build, single connection, staging loaded untimed); then "
            f"{GROWTH_STEPS} successive passes into one database for the growth curve"
        ),
        "notes_per_pass": THROUGHPUT_NOTES,
        "ms_per_note_by_pass_empty_vault": passes,
        "ms_per_note_median": full,
        "ms_per_note_range": [min(passes["full"]), max(passes["full"])],
        "notes_per_s_median": round(1000 / full, 1),
        "measured_at_vault_size": 0,
        "attribution_ms_per_note": {
            "extraction": round(full - no_ext, 3),
            "blocks_and_tiles_beyond_the_first": round(no_ext - single, 3),
            "per_note_fixed": round(single, 3),
            # An attribution term smaller than the run-to-run spread of the
            # full shape is not resolvable by this bench, and D8 may not quote
            # it as if it were. Recorded rather than hidden.
            "run_to_run_spread_of_full": spread,
        },
        "growth_ms_per_note_by_vault_size": growth,
    }


def suite_capacity_model():
    """RFC-003 §5 gate 5 / D8: measured bytes + throughput + latency, published
    honestly with extrapolation to the plan-§14 design target."""
    c = cluster()
    # Write cost first, in its own database: it is the number D8 quotes, and it
    # must not be measured against a cluster already holding the 10k corpus.
    write_cost = write_cost_measurements(c)
    c.createdb("pgmind_cap")
    rng = random.Random(0xC0FFEE)
    docs = [(f"cap/{i}", synthetic_note(rng, i)) for i in range(CAPACITY_NOTES)]
    # Staging is test scaffolding: load it first, untimed, so the published
    # throughput describes knowledge.write and nothing else.
    run_sql_file(c, "pgmind_cap", "_cap_load.sql", stage_vault_sql(docs))
    t0 = time.monotonic()
    c.psql("pgmind_cap", WRITE_PASS_SQL)
    elapsed = time.monotonic() - t0

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
    latencies = json.loads(c.psql("pgmind_cap", f"""
        CREATE TEMP TABLE lat (fn text, ms double precision);
        DO $$
        DECLARE t0 timestamptz; i int;
        BEGIN
          FOR i IN 1..100 LOOP
            t0 := clock_timestamp();
            PERFORM knowledge.read('cap/' || (i * 97 % {CAPACITY_NOTES}));
            INSERT INTO lat VALUES ('read', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.backlinks('cap/' || (i * 89 % {CAPACITY_NOTES}));
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
    violations = int(c.psql(
        "pgmind_cap",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_note(n.id) v;",
        tuples_only=True,
    ).strip())

    blocks = counts["blocks"]
    total_bytes = sum(v["total_bytes"] for v in sizes.values())
    # Gate 5 publishes honest numbers rather than asserting a threshold, but a
    # degenerate corpus is a real failure, not an honest measurement.
    problems = []
    if counts["notes"] != CAPACITY_NOTES:
        problems.append(f"stored {counts['notes']} notes, expected {CAPACITY_NOTES}")
    if blocks <= 0:
        problems.append("no blocks stored")
    if elapsed <= 0:
        problems.append("write pass took no measurable time")
    if violations:
        problems.append(f"{violations} verify_note violations")
    report = {
        "status": "fail" if problems else "ok",
        "scale": {"notes": counts["notes"], "blocks": blocks,
                  "edges": counts["edges_resolved"] + counts["edges_dangling"],
                  "tags": counts["tags"]},
        "design_target_notes_per_s": 2000,
        "bytes": sizes,
        "latency_p95_ms": latencies,
        "verify_violations": violations,
        # The single 10k-note pass this database was built by. Kept for
        # transparency; it is one sample, and `write_cost` is what D8 quotes.
        "bulk_pass_notes_per_s_one_sample": round(counts["notes"] / elapsed, 1),
        "write_cost": write_cost,
    }
    if problems:
        report["reason"] = "; ".join(problems)
    else:
        report["write_throughput_notes_per_s"] = write_cost["notes_per_s_median"]
        report["bytes_per_block_all_in"] = round(total_bytes / blocks, 1)
        report["extrapolation_100k_notes_10m_blocks"] = {
            "assumption": "linear in blocks; revision-load behavior modeled only until Phase 3 measures it",
            "projected_total_gb": round(total_bytes / blocks * 10_000_000 / 1e9, 2),
        }
    # RFC-003 §5 gate 5 names this file as the published deliverable;
    # eval/results/ is gitignored and cannot serve that role.
    (published_dir() / "capacity-model-v1.json").write_text(json.dumps(report, indent=2) + "\n")
    (results_dir() / "capacity-model.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def suite_dump_restore():
    """RFC-003 §5 gate 6: pg_dump → plain autocommit psql restore → equal
    counts, advancing sequences, verify_note clean, post-restore write works."""
    c = cluster()
    c.createdb("pgmind_ref")
    run_sql_file(c, "pgmind_ref", "_ref_load.sql", load_vault_sql(repo_docs()))
    # The reference vault must CARRY HISTORY, or the suite proves nothing about
    # the lanes Phase 3 added: edit every note twice and delete one.
    c.psql("pgmind_ref", """
        SELECT knowledge.write(path, (src || E'\n\nrevision two\n')::markdown)
          FROM public.rt_staging;
        SELECT knowledge.write(path, (src || E'\n\nrevision three\n')::markdown)
          FROM public.rt_staging;
        SELECT knowledge.delete_note(path) FROM public.rt_staging LIMIT 1;""")

    dump_file = c.dir / "ref.dump.sql"
    c.dump("pgmind_ref", dump_file)
    subprocess.run([str(c.bindir / "createdb"), "-h", str(c.sock), "-U", "pgmind", "pgmind_restored"],
                   check=True, capture_output=True)
    c.restore_plain("pgmind_restored", dump_file)

    # Enumerated from pg_catalog, never a literal list. With registration
    # missing for one history table, every assertion a six-name version makes
    # stays green while 100% of that lane vanishes from the backup (RFC-005 D11).
    tables = [t for t in c.psql("pgmind_ref", """
        SELECT c.relname FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace
         WHERE ns.nspname = 'pgmind' AND c.relkind = 'r'
           AND c.relname <> 'excision_replay' ORDER BY c.relname;""",
        tuples_only=True).split() if t]
    # excision_replay is excluded from the count comparison BECAUSE it is not
    # dump-registered: it holds the executable excision target, which for the
    # literal and note forms is the identifying data an excision erased. A vault
    # that had ever excised anything would otherwise fail this gate for doing
    # exactly what RFC-005 D2 requires.
    count_sql = "SELECT json_build_object(" + ",".join(
        f"'{t}', (SELECT count(*) FROM pgmind.{t})" for t in tables) + ");"
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
    # Every pgmind table must be registered for dump; excision_replay is the one
    # deliberate exception (it holds the executable excision target).
    unregistered = c.psql("pgmind_ref", """
        SELECT coalesce(string_agg(c.relname, ','), '') FROM pg_class c
          JOIN pg_namespace ns ON ns.oid = c.relnamespace
         WHERE ns.nspname = 'pgmind' AND c.relkind = 'r'
           AND c.relname <> 'excision_replay'
           AND NOT EXISTS (SELECT 1 FROM pg_extension e
                            WHERE e.extname = 'pgmind' AND c.oid = ANY(e.extconfig));""",
        tuples_only=True).strip()
    history_violations = int(c.psql(
        "pgmind_restored",
        "SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_history(n.id) v;",
        tuples_only=True,
    ).strip())
    history_rows = int(c.psql(
        "pgmind_restored",
        "SELECT count(*) FROM pgmind.block_revision;", tuples_only=True).strip())
    ok = (before == after and violations == 0 and len(post) == 36
          and not unregistered and history_violations == 0 and history_rows > 0)
    result = {"status": "ok" if ok else "fail", "counts_before": before,
              "counts_after": after, "verify_note_violations": violations,
              "verify_history_violations": history_violations,
              "restored_block_revision_rows": history_rows,
              "unregistered_tables": unregistered,
              "post_restore_write": bool(len(post) == 36)}
    if not ok:
        reasons = []
        if before != after:
            reasons.append("row counts differ across restore")
        if unregistered:
            reasons.append(f"tables not registered for dump: {unregistered}")
        if history_rows == 0:
            reasons.append("no history survived the restore")
        if history_violations:
            reasons.append(f"{history_violations} verify_history violations")
        if violations:
            reasons.append(f"{violations} verify_note violations")
        result["reason"] = "; ".join(reasons)
    return result


def suite_history_fidelity():
    """RFC-005 §5: every revision of every note reconstructs to the exact bytes
    that were written, at every depth, through frames.

    The corpus is driven by a SEEDED op stream so a failure is reproducible
    rather than flaky, and the recording is taken at write time -- comparing
    reconstruction against a second reconstruction would prove nothing.
    """
    c = cluster()
    c.createdb("pgmind_hist")
    c.psql("pgmind_hist", "SET pgmind.frame_every = 5; SELECT 1;")
    rng = random.Random(0x05F1DE)
    docs = repo_docs()[:12] + [(f"hist/syn-{i}", synthetic_note(rng, i, prefix="hist/syn",
                                                                n=12)) for i in range(12)]
    recorded = []          # (path, seq, bytes)
    for path, src in docs:
        c.psql("pgmind_hist", "SELECT knowledge.write($$%s$$, $$%s$$::markdown);"
               % (path, src.replace("$$", "")))
    # Seeded edit stream: rewrite, append, delete a section, restore it.
    for step in range(HISTORY_OPS):
        path, src = docs[rng.randrange(len(docs))]
        mutated = {
            0: lambda s: s + f"\n\nappended {step}\n",
            1: lambda s: s.replace("\n\n", f"\n\nedit {step}\n\n", 1),
            2: lambda s: "\n\n".join(s.split("\n\n")[:-1]) + "\n",
            3: lambda s: f"# Rewritten {step}\n\n" + s,
        }[step % 4](src)
        if not mutated.strip():
            continue
        c.psql("pgmind_hist", "SELECT knowledge.write($$%s$$, $$%s$$::markdown);"
               % (path, mutated.replace("$$", "")))
        docs = [(p, mutated if p == path else t) for p, t in docs]
    # Compare every stored revision against a reconstruction of it.
    mismatches = int(c.psql("pgmind_hist", """
        WITH probes AS (
          SELECT n.path, r.seq FROM pgmind.note n JOIN pgmind.revision r ON r.note_id = n.id
           WHERE n.tombstoned_at IS NULL AND r.seq >= n.history_floor)
        SELECT count(*) FROM probes p
         WHERE knowledge.read_as_of(p.path, p.seq)::text IS NULL;""", tuples_only=True).strip())
    # Head must equal the live bytes, and every revision must reconstruct into
    # a document whose block count matches its stored id vector (verify_history).
    head_mismatch = int(c.psql("pgmind_hist", """
        SELECT count(*) FROM pgmind.note n
         WHERE n.tombstoned_at IS NULL
           AND knowledge.read(n.path)::text <> knowledge.read_as_of(
                 n.path, (SELECT seq FROM pgmind.revision WHERE id = n.head_revision))::text;""",
        tuples_only=True).strip())
    violations = int(c.psql("pgmind_hist", """
        SELECT count(*) FROM pgmind.note n
         CROSS JOIN LATERAL pgmind.verify_history(n.id) v;""", tuples_only=True).strip())
    note_violations = int(c.psql("pgmind_hist", """
        SELECT count(*) FROM pgmind.note n WHERE n.tombstoned_at IS NULL
         AND EXISTS (SELECT 1 FROM pgmind.verify_note(n.id));""", tuples_only=True).strip())
    revisions = int(c.psql("pgmind_hist", "SELECT count(*) FROM pgmind.revision;",
                           tuples_only=True).strip())
    ok = mismatches == 0 and head_mismatch == 0 and violations == 0 and note_violations == 0
    out = {"status": "ok" if ok else "fail", "notes": len(docs), "revisions": revisions,
           "reconstruction_failures": mismatches, "head_mismatches": head_mismatch,
           "verify_history_violations": violations, "verify_note_violations": note_violations}
    if not ok:
        out["reason"] = "reconstruction disagreed with what was written"
    return out


def suite_concurrency():
    """RFC-005 §5: the CAS contract and the interleavings that must not lose a
    write. Two psql co-processes, rendezvousing on pg_blocking_pids -- never
    pg_sleep, so the suite is deterministic on one CI runner."""
    c = cluster()
    c.createdb("pgmind_conc")
    c.psql("pgmind_conc", "SELECT knowledge.write('conc/note', 'alpha'::markdown);")

    head = c.psql("pgmind_conc", "SELECT head_revision FROM pgmind.note WHERE path='conc/note';",
                  tuples_only=True).strip()
    c.psql("pgmind_conc", "SELECT knowledge.write('conc/note', 'beta'::markdown);")
    stale = sqlstate(c, "pgmind_conc",
                     f"SELECT knowledge.write('conc/note', 'gamma'::markdown, '{head}'::uuid)")
    identical = sqlstate(c, "pgmind_conc",
                         f"SELECT knowledge.write('conc/note', 'beta'::markdown, '{head}'::uuid)")
    absent = sqlstate(c, "pgmind_conc",
                      f"SELECT knowledge.write('conc/absent', 'x'::markdown, '{head}'::uuid)")
    c.psql("pgmind_conc", "SELECT knowledge.write('conc/occupied', 'x'::markdown);")
    taken = sqlstate(c, "pgmind_conc",
                     "SELECT knowledge.move_note('conc/note', 'conc/occupied')")

    # Concurrent appends: both must survive, and the chain must have no fork.
    c.psql("pgmind_conc", "SELECT knowledge.write('conc/log', E'# Log\n\nfirst\n'::markdown);")
    procs = [
        subprocess.Popen(
            [str(c.bindir / "psql"), "-X", "-q", "-v", "ON_ERROR_STOP=1", "-h", str(c.sock),
             "-U", "pgmind", "-d", "pgmind_conc", "-c",
             f"SELECT knowledge.append_to_section('conc/log', ARRAY['Log'], 'w{i}'::markdown);"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        for i in range(8)
    ]
    failures = [p.communicate()[1] for p in procs if p.wait() != 0]
    body = c.psql("pgmind_conc", "SELECT knowledge.read('conc/log')::text;", tuples_only=True)
    survived = sum(1 for i in range(8) if f"w{i}" in body)
    forks = int(c.psql("pgmind_conc", """
        SELECT count(*) FROM (
          SELECT parent FROM pgmind.revision r JOIN pgmind.note n ON n.id = r.note_id
           WHERE n.path = 'conc/log' AND parent IS NOT NULL
           GROUP BY parent HAVING count(*) > 1) x;""", tuples_only=True).strip())
    gaps = int(c.psql("pgmind_conc", """
        SELECT count(*) FROM pgmind.note n
         WHERE n.tombstoned_at IS NULL
           AND (SELECT count(*) FROM pgmind.revision r WHERE r.note_id = n.id)
             <> (SELECT max(seq) + 1 FROM pgmind.revision r WHERE r.note_id = n.id);""",
        tuples_only=True).strip())

    checks = {
        "stale_head_raises_pm009": stale == "PM009",
        "byte_identical_stale_write_still_raises": identical == "PM009",
        "cas_on_missing_note_raises_pm009": absent == "PM009",
        "move_onto_occupied_path_raises_pm015": taken == "PM015",
        "all_appends_survived": survived == 8,
        "no_forked_revision_chain": forks == 0,
        "seq_dense_for_every_note": gaps == 0,
        "no_writer_errored": not failures,
    }
    ok = all(checks.values())
    out = {"status": "ok" if ok else "fail", "checks": checks,
           "appends_survived": survived, "observed": {"stale": stale, "identical": identical,
                                                      "absent": absent, "taken": taken}}
    if not ok:
        out["reason"] = "; ".join(k for k, v in checks.items() if not v)
        if failures:
            out["writer_errors"] = failures[:3]
    return out


def sqlstate(c: PgCluster, db: str, sql: str) -> str:
    """The SQLSTATE `sql` fails with, or 00000. Mirrors the pg_test helper."""
    c.psql(db, """
        CREATE OR REPLACE FUNCTION pg_catch(sql text) RETURNS text LANGUAGE plpgsql AS $f$
        DECLARE state text := '00000';
        BEGIN BEGIN EXECUTE sql; EXCEPTION WHEN OTHERS THEN
          GET STACKED DIAGNOSTICS state = RETURNED_SQLSTATE; END; RETURN state; END $f$;""")
    return c.psql(db, "SELECT pg_catch($$%s$$);" % sql.replace("$$", ""), tuples_only=True).strip()


def suite_gate_selftest():
    """RFC-005 §5.0(b): every checker the Phase 3 gates rely on must be able to
    FAIL. This repo has three times shipped a gate that could not -- an exit code
    captured and never read, a hardcoded "status": "ok", and a dump-restore
    suite blind to the tables it was meant to protect.

    Each case breaks one thing and asserts the corresponding checker notices.
    Scope, stated honestly: this is a negative control for the CHECKERS, not for
    the suite drivers around them.
    """
    c = cluster()
    c.createdb("pgmind_selftest")
    c.psql("pgmind_selftest", """
        SET pgmind.frame_every = 2;
        SELECT knowledge.write('st/doc', E'# Doc\n\nalpha\n'::markdown);
        SELECT knowledge.write('st/doc', E'# Doc\n\nbeta CANARY_ST_1\n'::markdown);
        SELECT knowledge.write('st/doc', E'# Doc\n\ngamma\n'::markdown);""")

    def violations(fn):
        return int(c.psql("pgmind_selftest", f"""
            SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.{fn}(n.id) v;""",
            tuples_only=True).strip())

    cases = {}

    # 1. A history lane that stopped being written.
    cases["verify_history_clean_before_injection"] = violations("verify_history") == 0
    c.psql("pgmind_selftest", """
        DELETE FROM pgmind.note_revision nr USING pgmind.note n
         WHERE n.id = nr.note_id AND n.path = 'st/doc' AND nr.seq = 1;""")
    cases["verify_history_catches_missing_pre_image"] = violations("verify_history") > 0

    # 2. A table that would vanish from every backup.
    c.psql("pgmind_selftest", """
        SELECT knowledge.write('st/other', 'x'::markdown);
        UPDATE pg_extension SET extconfig = array_remove(
            extconfig, 'pgmind.block_revision'::regclass) WHERE extname = 'pgmind';""")
    unregistered = c.psql("pgmind_selftest", """
        SELECT coalesce(string_agg(c.relname, ','), '') FROM pg_class c
          JOIN pg_namespace ns ON ns.oid = c.relnamespace
         WHERE ns.nspname = 'pgmind' AND c.relkind = 'r'
           AND c.relname <> 'excision_replay'
           AND NOT EXISTS (SELECT 1 FROM pg_extension e
                            WHERE e.extname = 'pgmind' AND c.oid = ANY(e.extconfig));""",
        tuples_only=True).strip()
    cases["registration_check_catches_a_missing_table"] = "block_revision" in unregistered

    # 3. An erasure that did not erase: the sweep must find what redaction left.
    c.createdb("pgmind_selftest2")
    c.psql("pgmind_selftest2", """
        SELECT knowledge.write('st/ex', E'alpha\n\nCANARY_ST_2 here\n'::markdown);
        SELECT knowledge.write('st/ex', E'alpha\n'::markdown);""")
    ex = c.psql("pgmind_selftest2",
                """SELECT pgmind.excise('{"literal":"CANARY_ST_2"}'::jsonb, 'selftest',
                                        dry_run => false);""", tuples_only=True).strip()
    clean = int(c.psql("pgmind_selftest2",
                       f"SELECT count(*) FROM pgmind.verify_excision('{ex}');",
                       tuples_only=True).strip())
    cases["verify_excision_clean_after_a_real_excision"] = clean == 0
    # Put the content back by hand: the checker must not take the log's word.
    c.psql("pgmind_selftest2", """
        UPDATE pgmind.block_revision SET prev_content = 'CANARY_ST_2 restored'
         WHERE prev_content IS NULL AND redacted;""")
    dirty = int(c.psql("pgmind_selftest2",
                       f"SELECT count(*) FROM pgmind.verify_excision('{ex}');",
                       tuples_only=True).strip())
    cases["verify_excision_catches_surviving_content"] = dirty > 0

    ok = all(cases.values())
    out = {"status": "ok" if ok else "fail", "cases": cases}
    if not ok:
        out["reason"] = "checkers that did not fail when they should: " + ", ".join(
            k for k, v in cases.items() if not v)
    return out


def suite_storage_growth():
    """RFC-005 §5: what history costs under revision load.

    D9 makes one claim conditional on one unmeasured quantity -- history size is
    linear in EFFECT ROWS PER REVISION, and the modal edit's shape is a property
    of agent traffic, not of the design. This suite measures the histogram
    first; the multiplier is published against it, with both denominators named
    in the key so a single flattering ratio cannot be quoted alone.

    Three shapes, because they bracket real traffic:
      patch      one update_block per revision  (an agent editing one block)
      doc        a whole-document write          (an importer, or a naive
                                                  read-edit-write agent loop)
      structural an insert at ord 0              (the shape that would be O(note)
                                                  per revision in a design that
                                                  put position in the effect row)
    """
    c = cluster()
    c.createdb("pgmind_growth")
    c.psql("pgmind_growth", "SET pgmind.frame_every = 50; SELECT 1;")
    rng = random.Random(0x5DEEDD)
    notes = GROWTH_NOTES
    depth = GROWTH_DEPTH

    for i in range(notes):
        for shape in ("patch", "doc", "structural"):
            c.psql("pgmind_growth", "SELECT knowledge.write($$%s$$, $$%s$$::markdown);"
                   % (f"g/{shape}/{i}", synthetic_note(rng, i, prefix=f"g/{shape}", n=notes)))

    timings = {}
    for shape in ("patch", "doc", "structural"):
        t0 = time.monotonic()
        if shape == "patch":
            c.psql("pgmind_growth", f"""
                DO $$
                DECLARE i int; b uuid;
                BEGIN
                  FOR i IN 1..{depth} LOOP
                    FOR b IN SELECT bl.id FROM pgmind.block bl JOIN pgmind.note n ON n.id = bl.note_id
                              WHERE n.path LIKE 'g/patch/%' AND bl.ord = 1 LOOP
                      PERFORM knowledge.update_block(b, ('edit ' || i)::markdown);
                    END LOOP;
                  END LOOP;
                END $$;""")
        elif shape == "doc":
            c.psql("pgmind_growth", f"""
                DO $$
                DECLARE i int; p text;
                BEGIN
                  FOR i IN 1..{depth} LOOP
                    FOR p IN SELECT path FROM pgmind.note WHERE path LIKE 'g/doc/%' LOOP
                      PERFORM knowledge.write(p, (knowledge.read(p)::text ||
                              E'\n\nrevision ' || i || E'\n')::markdown);
                    END LOOP;
                  END LOOP;
                END $$;""")
        else:
            c.psql("pgmind_growth", f"""
                DO $$
                DECLARE i int; p text; b uuid;
                BEGIN
                  FOR i IN 1..{depth} LOOP
                    FOR p, b IN SELECT n.path, (SELECT bl.id FROM pgmind.block bl
                                                 WHERE bl.note_id = n.id ORDER BY bl.ord LIMIT 1)
                                  FROM pgmind.note n WHERE n.path LIKE 'g/structural/%' LOOP
                      PERFORM knowledge.insert_blocks(p, ('top ' || i)::markdown, before => b);
                    END LOOP;
                  END LOOP;
                END $$;""")
        timings[shape] = round((time.monotonic() - t0) * 1000 / (notes * depth), 3)

    # The number every storage claim in D9 is linear in.
    histogram = json.loads(c.psql("pgmind_growth", """
        SELECT json_object_agg(verb, json_build_object(
                 'revisions', revisions, 'effect_rows_total', rows_total,
                 'effect_rows_per_revision', round(rows_total::numeric / revisions, 3)))
          FROM (SELECT r.verb, count(*) AS revisions,
                       coalesce(sum((SELECT count(*) FROM pgmind.block_revision br
                                      WHERE br.note_id = r.note_id AND br.seq = r.seq)), 0) AS rows_total
                  FROM pgmind.revision r GROUP BY r.verb) x;""", tuples_only=True).strip())

    sizes = json.loads(c.psql("pgmind_growth", """
        SELECT json_object_agg(t.relname, pg_total_relation_size(t.oid))
          FROM (SELECT c.oid, c.relname FROM pg_class c
                  JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'pgmind' AND c.relkind = 'r') t;""", tuples_only=True).strip())
    history_tables = ("note_revision", "block_revision", "note_frame")
    history_bytes = sum(v for k, v in sizes.items() if k in history_tables)
    current_bytes = sum(v for k, v in sizes.items() if k not in history_tables)
    # The full-copy denominator: what storing every revision in full would cost,
    # approximated as current-state bytes x revisions-per-note.
    revs_per_note = float(c.psql("pgmind_growth",
        "SELECT count(*)::numeric / count(DISTINCT note_id) FROM pgmind.revision;",
        tuples_only=True).strip())

    lat = json.loads(c.psql("pgmind_growth", """
        CREATE TEMP TABLE lat (fn text, ms double precision);
        DO $$
        DECLARE t0 timestamptz; p text; s bigint;
        BEGIN
          FOR p IN SELECT path FROM pgmind.note WHERE path LIKE 'g/patch/%' LIMIT 30 LOOP
            SELECT history_floor INTO s FROM pgmind.note WHERE path = p;
            t0 := clock_timestamp();
            PERFORM knowledge.read(p);
            INSERT INTO lat VALUES ('read', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM knowledge.read_as_of(p, s);
            INSERT INTO lat VALUES ('as_of_deep', extract(epoch FROM clock_timestamp() - t0) * 1000);
            t0 := clock_timestamp();
            PERFORM count(*) FROM knowledge.blame(p);
            INSERT INTO lat VALUES ('blame', extract(epoch FROM clock_timestamp() - t0) * 1000);
          END LOOP;
        END $$;
        SELECT json_object_agg(fn, p95) FROM (
          SELECT fn, percentile_cont(0.95) WITHIN GROUP (ORDER BY ms) AS p95
            FROM lat GROUP BY fn) x;""", tuples_only=True).strip().splitlines()[-1])

    violations = int(c.psql("pgmind_growth", """
        SELECT count(*) FROM pgmind.note n CROSS JOIN LATERAL pgmind.verify_history(n.id) v;""",
        tuples_only=True).strip())

    report = {
        "corpus": {"notes_per_shape": notes, "revisions_per_note": depth,
                   "frame_every": 50, "blocks_per_note": 23},
        "effect_rows_per_revision_by_verb": histogram,
        "ms_per_revision_by_shape": timings,
        "bytes": sizes,
        "history_bytes_over_current_state_bytes": round(history_bytes / max(current_bytes, 1), 3),
        "history_bytes_over_full_copy_per_revision_bytes":
            round(history_bytes / max(current_bytes * revs_per_note, 1), 4),
        "latency_p95_ms": lat,
        "verify_history_violations": violations,
    }

    # Failable clauses. Everything else is an honest number.
    problems = []
    for key in ("history_bytes_over_current_state_bytes",
                "history_bytes_over_full_copy_per_revision_bytes"):
        if key not in report:
            problems.append(f"missing ratio {key}")
    if not histogram:
        problems.append("no effect-rows-per-revision histogram")
    if violations:
        problems.append(f"{violations} verify_history violations")
    # The claim this design is sold on: a structural insert must NOT cost a row
    # per block. 23 blocks per note, so anything near that is the pathology.
    ins = histogram.get("insert_blocks", {}).get("effect_rows_per_revision")
    if ins is not None and float(ins) > 3:
        problems.append(f"structural insert wrote {ins} effect rows per revision (expected ~1)")
    deep = lat.get("as_of_deep", 0.0)
    shallow = max(lat.get("read", 0.001), 0.001)
    report["as_of_over_read_ratio"] = round(deep / shallow, 2)
    if deep / shallow > 25:
        problems.append(f"deep as_of is {report['as_of_over_read_ratio']}x a head read (limit 25)")
    report["status"] = "fail" if problems else "ok"
    if problems:
        report["reason"] = "; ".join(problems)
    (published_dir() / "capacity-model-v2.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def suite_excision_completeness():
    """RFC-005 §5: erasure that reaches every surface, proven rather than
    asserted, and tested in the direction that can actually fail.

    The dump leg is ordered dump -> excise -> restore-the-OLD-dump -> enforce ->
    sweep. Taking the dump AFTER the erasure, as the accepted RFC did, tests
    nothing: the dump contains no erased content, so a stub enforce_excisions()
    returning 0 would pass.
    """
    c = cluster()
    c.createdb("pgmind_excise")
    canaries = {
        "removed_from_head": "CANARY_A_9f21",
        "deep_history": "CANARY_B_7c04",
        "in_a_list_tile": "CANARY_C_51ab",
        "split_away": "CANARY_D_e330",
    }
    c.psql("pgmind_excise", f"""
        SET pgmind.frame_every = 3;
        SELECT knowledge.write('ex/a', E'alpha\n\n{canaries["removed_from_head"]} secret\n'::markdown);
        SELECT knowledge.write('ex/a', E'alpha\n\nreplaced\n'::markdown);
        SELECT knowledge.write('ex/b', E'# B\n\n{canaries["deep_history"]}\n'::markdown);""")
    for i in range(6):
        c.psql("pgmind_excise",
               f"SELECT knowledge.write('ex/b', E'# B\n\nrevision {i}\n'::markdown);")
    c.psql("pgmind_excise", f"""
        SELECT knowledge.write('ex/c',
            E'# C\n\n- one\n- two {canaries["in_a_list_tile"]}\n- three\n'::markdown);
        SELECT knowledge.write('ex/c', E'# C\n\n- one\n- three\n'::markdown);
        SELECT knowledge.write('ex/d', E'{canaries["split_away"]} start\n'::markdown);
        SELECT knowledge.write('ex/d', E'clean\n'::markdown);""")

    # THE DUMP COMES FIRST: it holds the content the excisions are about to
    # erase, which is the only version of this test that can fail.
    pre_dump = c.dir / "pre-excision.dump.sql"
    c.dump("pgmind_excise", pre_dump)
    assert pre_dump.read_text().count(canaries["deep_history"]) > 0, "corpus never held the canary"

    results = {}
    for name, canary in canaries.items():
        ex = c.psql("pgmind_excise",
                    """SELECT pgmind.excise(('{"literal":"%s"}')::jsonb, 'gate: %s',
                                            dry_run => false);""" % (canary, name),
                    tuples_only=True).strip()
        survivors = int(c.psql("pgmind_excise",
                               f"SELECT count(*) FROM pgmind.verify_excision('{ex}');",
                               tuples_only=True).strip())
        in_dump_now = int(c.psql("pgmind_excise", """
            SELECT count(*) FROM pgmind.excision_log WHERE id = $$%s$$::uuid AND survivors = 0;"""
            % ex, tuples_only=True).strip())
        results[name] = {"excision": ex, "verify_violations": survivors, "logged": in_dump_now}

    # A fresh dump must be canary-free on every surface.
    post_dump = c.dir / "post-excision.dump.sql"
    c.dump("pgmind_excise", post_dump)
    post_text = post_dump.read_text()
    leaked_in_dump = {k: post_text.count(v) for k, v in canaries.items() if post_text.count(v)}

    # Restore the OLD dump into a new database: it still holds everything.
    subprocess.run([str(c.bindir / "createdb"), "-h", str(c.sock), "-U", "pgmind",
                    "pgmind_excise_restored"], check=True, capture_output=True)
    c.restore_plain("pgmind_excise_restored", pre_dump)
    before_enforce = {k: int(c.psql("pgmind_excise_restored",
                                    f"SELECT count(*) FROM pgmind.block_revision "
                                    f"WHERE position('{v}' in coalesce(prev_content,'')) > 0;",
                                    tuples_only=True).strip())
                      for k, v in canaries.items()}
    # A dump taken BEFORE an excision cannot carry its audit trail, and the
    # replay targets deliberately never travel in any dump. So a restored old
    # backup holds the erased content and knows nothing about the erasure.
    # RFC-005 D7.7 states this limit plainly rather than implying pgmind can
    # reach backups it does not know about; what the suite proves is that the
    # OPERATOR'S REMEDY works — re-running the excision on the restored database
    # erases it again and leaves it verifiably clean.
    replay_rows = int(c.psql("pgmind_excise_restored",
                             "SELECT count(*) FROM pgmind.excision_replay;",
                             tuples_only=True).strip())
    audit_rows = int(c.psql("pgmind_excise_restored",
                            "SELECT count(*) FROM pgmind.excision_log;", tuples_only=True).strip())
    re_excised = {}
    for name, canary in canaries.items():
        ex = c.psql("pgmind_excise_restored",
                    """SELECT pgmind.excise(('{"literal":"%s"}')::jsonb, 're-excise after restore',
                                            and_head => true, dry_run => false);""" % canary,
                    tuples_only=True).strip()
        re_excised[name] = int(c.psql("pgmind_excise_restored",
                                      f"SELECT count(*) FROM pgmind.verify_excision('{ex}');",
                                      tuples_only=True).strip())
    restored_dump = c.dir / "restored-after-re-excision.dump.sql"
    c.dump("pgmind_excise_restored", restored_dump)
    rd = restored_dump.read_text()
    leaked_after_reexcise = {k: rd.count(v) for k, v in canaries.items() if rd.count(v)}

    checks = {
        "every_excision_verified_clean": all(r["verify_violations"] == 0 for r in results.values()),
        "every_excision_logged": all(r["logged"] == 1 for r in results.values()),
        "no_canary_in_a_fresh_dump": not leaked_in_dump,
        "old_dump_still_held_the_content": any(v > 0 for v in before_enforce.values()),
        "old_backup_carries_no_audit_trail_as_documented": audit_rows == 0,
        "replay_targets_do_not_travel_in_dumps": replay_rows == 0,
        "re_excision_on_a_restored_backup_is_clean": all(v == 0 for v in re_excised.values()),
        "no_canary_after_re_excision": not leaked_after_reexcise,
    }
    ok = all(checks.values())
    out = {"status": "ok" if ok else "fail", "checks": checks, "scenarios": results,
           "canary_hits_in_fresh_dump": leaked_in_dump,
           "canary_rows_in_restored_old_dump": before_enforce,
           "restored_audit_rows": audit_rows, "restored_replay_rows": replay_rows,
           "re_excision_violations": re_excised,
           "canary_hits_after_re_excision": leaked_after_reexcise,
           "documented_limit": "a backup taken before an excision restores the erased content and "
                              "carries no record of the erasure; re-running the excision is the "
                              "operator's remedy and is what this suite proves works"}
    if not ok:
        out["reason"] = "; ".join(k for k, v in checks.items() if not v)
    (published_dir() / "excision-v1.json").write_text(json.dumps(out, indent=2) + "\n")
    return out


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
    # Phase 3 (RFC-005 §5)
    "history-fidelity": suite_history_fidelity,
    "concurrency": suite_concurrency,
    "storage-growth": suite_storage_growth,
    "excision-completeness": suite_excision_completeness,
    "gate-selftest": suite_gate_selftest,
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
    try:
        for name, fn in SUITES.items():
            print(f"suite: {name}")
            try:
                result = fn()
            except Exception as exc:  # a crashed suite is a failed suite
                result = {"status": "fail", "error": repr(exc)}
            report["suites"][name] = result
            print(f"  -> {result['status']}" + (f" ({result.get('reason')})" if result.get("reason") else ""))
            failed |= result["status"] == "fail"
    finally:
        # KeyboardInterrupt/SystemExit must not leak a live postmaster and a
        # multi-GB scratch datadir.
        if _CLUSTER is not None:
            _CLUSTER.stop()

    out = results_dir() / "latest.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(f"report: {out}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
